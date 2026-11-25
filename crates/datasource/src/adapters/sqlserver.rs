//! SQL Server over TDS, through `tiberius` with its rustls on the
//! platform's roots, on the same current-thread runtime the Postgres
//! transport uses.
//!
//! Microsoft's own ladder has three rungs — `Encrypt`, with and without
//! `TrustServerCertificate` — and they map onto libpq's: `require` is
//! encrypted and trusts whatever answered, `verify-full` checks the
//! certificate against the roots or `ca_file`. There is no `verify-ca`
//! rung on SQL Server, as there is none in Microsoft's drivers, so it is
//! treated as `verify-full`. TDS 7.3 rather than 8.0: 8.0 is TLS-first by
//! definition and refuses to build without it, and `strict` can come
//! later with it. tiberius's rustls brings aws-lc in beside ring, which
//! costs 2.7 MB of binary and is why `ring` is installed as the process
//! default once, in `tls`.
use crate::rows::{Batcher, Capped, RowSink};
use crate::{tls, Connection, Error, Result, Rows, Secret, Sink, Source, SslMode};
use tokio_util::compat::TokioAsyncWriteCompatExt;

type Client = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

pub struct SqlServer {
    runtime: tokio::runtime::Runtime,
    /// None after a statement timed out: the TDS stream still holds the
    /// rows that were never read, and tiberius sends no attention on
    /// drop, so the next statement would read them as its own answer.
    /// The next query reconnects instead.
    client: Option<Client>,
    conn: Connection,
    secret: Option<Secret>,
    token: Option<Secret>,
    uid: String,
    timeout: std::time::Duration,
    warnings: Vec<String>,
}

impl SqlServer {
    /// With an OAuth token, a Microsoft Entra access token for the
    /// database, the login is the token and names no user; otherwise it
    /// is a SQL login, a username and its password.
    pub fn open(conn: &Connection, secret: Option<Secret>, token: Option<Secret>) -> Result<SqlServer> {
        let host = conn.need(&conn.host, "host")?;
        let login = match &token {
            Some(t) => tiberius::AuthMethod::aad_token(t.expose()),
            None => {
                let user = conn.need(&conn.username, "username")?;
                let password = secret
                    .clone()
                    .ok_or_else(|| Error::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;
                tiberius::AuthMethod::sql_server(&user, password.expose())
            }
        };
        tls::install_ring();
        let configure = |encryption: tiberius::EncryptionLevel, trust: Trust| {
            let mut config = tiberius::Config::new();
            config.host(&host);
            config.port(conn.port.unwrap_or(1433));
            config.authentication(login.clone());
            if let Some(db) = &conn.database {
                config.database(db);
            }
            config.application_name("zsql");
            config.encryption(encryption);
            match trust {
                Trust::Anyone => config.trust_cert(),
                Trust::File(path) => config.trust_cert_ca(path.display().to_string()),
                Trust::Roots => {}
            }
            config
        };
        let trust = |verifies: bool| match (&conn.ca_file, verifies) {
            (_, false) => Trust::Anyone,
            (Some(path), true) => Trust::File(path.clone()),
            (None, true) => Trust::Roots,
        };
        use tiberius::EncryptionLevel::{NotSupported, Required};
        let plan: Vec<(tiberius::EncryptionLevel, Trust)> = match conn.sslmode {
            SslMode::Disable => vec![(NotSupported, Trust::Anyone)],
            // TLS if it can, plaintext if it cannot, and a warning when it falls.
            SslMode::Prefer => vec![(Required, Trust::Anyone), (NotSupported, Trust::Anyone)],
            SslMode::Require => vec![(Required, Trust::Anyone)],
            SslMode::VerifyCa | SslMode::VerifyFull => vec![(Required, trust(true))],
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Connect(conn.uid.clone(), e.to_string()))?;
        let mut warnings = Vec::new();
        let mut last = String::new();
        let mut client = None;
        for (i, (encryption, trust)) in plan.iter().enumerate() {
            let config = configure(*encryption, trust.clone());
            let attempt = runtime.block_on(async {
                let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::TcpStream::connect(config.get_addr()))
                    .await
                    .map_err(|_| "no connection within 10s".to_string())?
                    .map_err(|e| e.to_string())?;
                tcp.set_nodelay(true).map_err(|e| e.to_string())?;
                tiberius::Client::connect(config, tcp.compat_write()).await.map_err(|e| crate::chain(&e))
            });
            match attempt {
                Ok(c) => {
                    if i > 0 {
                        warnings.push(format!("{}: sslmode is prefer and TLS failed ({last}), so nothing is encrypted", conn.uid));
                    }
                    client = Some(c);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let client = client.ok_or_else(|| Error::Connect(conn.uid.clone(), last))?;
        Ok(SqlServer {
            runtime,
            client: Some(client),
            conn: conn.clone(),
            secret,
            token,
            uid: conn.uid.clone(),
            timeout: conn.timeout(),
            warnings,
        })
    }
}

/// Whose certificate is accepted.
#[derive(Clone)]
enum Trust {
    Anyone,
    File(std::path::PathBuf),
    Roots,
}

/// Why a read ended before its statement did.
enum Stopped {
    TimedOut,
    /// Whoever takes the rows had enough: a cap, or anything else.
    Refused(Error),
    Failed(String),
}

impl SqlServer {
    /// The first result set's rows to `out` as the server sends them,
    /// the whole read under the timeout.
    fn read(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        use futures_util::TryStreamExt;
        self.conn.access.check(sql)?;
        let uid = self.uid.clone();
        let timeout = self.timeout;
        if self.client.is_none() {
            // The new connection's sockets belong to the runtime it was
            // opened on, so that runtime is kept with it: a client moved
            // here without it would find its runtime shut down.
            let fresh = SqlServer::open(&self.conn, self.secret.clone(), self.token.clone())?;
            self.client = fresh.client;
            self.runtime = fresh.runtime;
        }
        let client = self.client.as_mut().expect("reopened");
        let outcome = self.runtime.block_on(async {
            let read = async {
                let failed = |e: tiberius::error::Error| Stopped::Failed(e.to_string());
                let mut stream = client.simple_query(sql).await.map_err(failed)?;
                while let Some(item) = stream.try_next().await.map_err(failed)? {
                    match item {
                        tiberius::QueryItem::Metadata(meta) if meta.result_index() == 0 => {
                            out.columns(&meta.columns().iter().map(|c| c.name().to_string()).collect::<Vec<_>>())
                                .map_err(Stopped::Refused)?;
                        }
                        tiberius::QueryItem::Row(row) if row.result_index() == 0 => {
                            out.row(row.cells().map(|(_, d)| cell(d)).collect()).map_err(Stopped::Refused)?;
                        }
                        // A later result set is read past, not kept.
                        _ => {}
                    }
                }
                Ok(())
            };
            tokio::time::timeout(timeout, read).await.unwrap_or(Err(Stopped::TimedOut))
        });
        match outcome {
            Ok(()) => Ok(()),
            Err(Stopped::Failed(why)) => Err(Error::Query(uid, why)),
            // Rows the read did not take are still on the stream, and
            // nothing sends an attention when it is dropped: the
            // connection is given up and the next statement opens one.
            Err(Stopped::TimedOut) => {
                self.client = None;
                Err(Error::Query(uid, format!("no answer within {}s", timeout.as_secs())))
            }
            Err(Stopped::Refused(e)) => {
                self.client = None;
                Err(e)
            }
        }
    }
}

impl Source for SqlServer {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let mut kept = Capped::new(&self.uid, max_rows);
        self.read(sql, &mut kept)?;
        Ok(kept.finish())
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        let mut batcher = Batcher::new(sink);
        self.read(sql, &mut batcher)?;
        batcher.finish()
    }

    fn adapter(&self) -> &str {
        "sqlserver"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// A TDS value as text. The temporal types print their debug form, which
/// is readable and stable; samples of dates are out of scope anyway.
fn cell(d: &tiberius::ColumnData<'_>) -> Option<String> {
    use tiberius::ColumnData::*;
    match d {
        U8(v) => v.map(|x| x.to_string()),
        I16(v) => v.map(|x| x.to_string()),
        I32(v) => v.map(|x| x.to_string()),
        I64(v) => v.map(|x| x.to_string()),
        F32(v) => v.map(|x| x.to_string()),
        F64(v) => v.map(|x| x.to_string()),
        Bit(v) => v.map(|x| x.to_string()),
        String(v) => v.as_ref().map(|s| s.to_string()),
        Guid(v) => v.map(|g| g.to_string()),
        Binary(v) => v.as_ref().map(|b| format!("<{} bytes>", b.len())),
        Numeric(v) => v.map(|n| n.to_string()),
        Xml(v) => v.as_ref().map(|x| x.to_string()),
        other => Some(format!("{other:?}")),
    }
}
