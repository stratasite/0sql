//! The wire transport, on `tokio-postgres`: Postgres itself, and Redshift
//! when it is reached over the wire rather than through its Data API.
//!
//! Synchronous from the outside. The caller is a command-line tool, and a
//! current-thread runtime held for the life of the connection costs
//! nothing beside a warehouse round trip while saving every caller from
//! being async. The crate stays free to grow an async twin if the server
//! ever needs one.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{tls, Access, Connection, Error, Result, Rows, Secret, Sink, Source, SslMode};

pub struct Postgres {
    runtime: tokio::runtime::Runtime,
    client: tokio_postgres::Client,
    adapter: String,
    uid: String,
    timeout: std::time::Duration,
    warnings: Vec<String>,
    access: Access,
}

/// How a Postgres connection is made from what a datasource says: the
/// settings and the TLS that goes with its `sslmode`. For a caller that
/// keeps connections of its own to a Postgres it was pointed at.
pub fn settings(conn: &Connection, password: &Secret) -> Result<(tokio_postgres::Config, tokio_postgres_rustls::MakeRustlsConnect)> {
    let host = conn.need(&conn.host, "host")?;
    let user = conn.need(&conn.username, "username")?;
    let database = conn.need(&conn.database, "database")?;
    // The statement timeout is the warehouse's to enforce, so it is set
    // on the session rather than trusted from this side alone. Redshift
    // takes no `options` at startup, so there the same two are SET on
    // the session once it is open.
    let redshift = conn.adapter == "redshift";
    let mut options = format!("-c statement_timeout={}", conn.timeout().as_millis());
    if let Some(schema) = &conn.schema {
        options.push_str(&format!(" -c search_path={schema}"));
    }
    if redshift {
        options.clear();
    }

    // libpq's ladder maps onto tokio-postgres's three settings with the
    // verification carried by the rustls configuration: Require with an
    // unverifying config is libpq's `require`, Require with the roots
    // is `verify-full`, and the chain-only verifier is `verify-ca`.
    let ssl = match conn.sslmode {
        SslMode::Disable => tokio_postgres::config::SslMode::Disable,
        SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        _ => tokio_postgres::config::SslMode::Require,
    };
    let mut config = tokio_postgres::Config::new();
    config
        .host(&host)
        .port(conn.port.unwrap_or(if redshift { 5439 } else { 5432 }))
        .user(&user)
        .dbname(&database)
        .password(password.expose())
        .application_name("zsql")
        .options(&options)
        .connect_timeout(std::time::Duration::from_secs(10))
        .ssl_mode(ssl);
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls::client_config(&conn.uid, conn.sslmode, conn.ca_file.as_deref())?);
    Ok((config, tls))
}

impl Postgres {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<Postgres> {
        let password =
            secret.ok_or_else(|| Error::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;

        let redshift = conn.adapter == "redshift";
        let (config, tls) = settings(conn, &password)?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Connect(conn.uid.clone(), e.to_string()))?;

        let (client, connection) = runtime.block_on(config.connect(tls)).map_err(|e| Error::Connect(conn.uid.clone(), crate::chain(&e)))?;
        // The connection drives the socket and must be polled for the
        // client to work; it ends when the client is dropped.
        runtime.spawn(async move {
            let _ = connection.await;
        });

        let mut pg = Postgres {
            runtime,
            client,
            adapter: conn.adapter.clone(),
            uid: conn.uid.clone(),
            timeout: conn.timeout(),
            warnings: vec![],
            access: conn.access,
        };
        if redshift {
            let mut session = format!("SET statement_timeout TO {}", conn.timeout().as_millis());
            if let Some(schema) = &conn.schema {
                session.push_str(&format!("; SET search_path TO {schema}"));
            }
            pg.runtime.block_on(pg.client.simple_query(&session)).map_err(|e| Error::Connect(conn.uid.clone(), crate::chain(&e)))?;
        }
        // `prefer` may have ended plaintext, and the server knows which —
        // Postgres through pg_stat_ssl; Redshift has no such view, so there
        // the mode is not confirmed and the warning says so.
        if conn.sslmode == SslMode::Prefer {
            if redshift {
                pg.warnings
                    .push(format!("{}: sslmode is prefer, which Redshift gives no way to confirm; say require to be sure", conn.uid));
            } else {
                let rows = pg.query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", 1)?;
                if rows.rows.first().and_then(|r| rows.get(r, "ssl")) != Some("t") {
                    pg.warnings.push(format!("{}: sslmode is prefer and the server offered no TLS, so nothing is encrypted", conn.uid));
                }
            }
        }
        Ok(pg)
    }
}

impl Postgres {
    /// The statement's rows to `out` as the server sends them, the whole
    /// read under the timeout.
    fn read(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        use futures_util::TryStreamExt;
        self.access.check(sql)?;
        // The simple-query protocol: every value arrives as the server's
        // own text, whatever its type, which is what a catalog and a
        // sample want. The extended protocol sends binary, and reading a
        // boolean or a number as a string there yields nothing at all —
        // which is how every non-text cell was null until a test asked
        // pg_stat_ssl a yes-or-no question.
        let uid = self.uid.clone();
        let fail = |e: tokio_postgres::Error| Error::Query(uid.clone(), crate::chain(&e));
        let (client, timeout) = (&self.client, self.timeout);
        self.runtime.block_on(async {
            let read = async {
                let mut messages = std::pin::pin!(client.simple_query_raw(sql).await.map_err(fail)?);
                let mut named = false;
                // A read that stops early drops the stream; the
                // connection reads past what is left of the answer and
                // is good for the next statement.
                while let Some(m) = messages.try_next().await.map_err(fail)? {
                    match m {
                        tokio_postgres::SimpleQueryMessage::RowDescription(cols) => {
                            out.columns(&cols.iter().map(|c| c.name().to_string()).collect::<Vec<_>>())?;
                            named = true;
                        }
                        tokio_postgres::SimpleQueryMessage::Row(row) => {
                            if !named {
                                out.columns(&row.columns().iter().map(|c| c.name().to_string()).collect::<Vec<_>>())?;
                                named = true;
                            }
                            out.row((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect())?;
                        }
                        tokio_postgres::SimpleQueryMessage::CommandComplete(changed) if !named => out.affected(changed)?,
                        _ => {}
                    }
                }
                Ok(())
            };
            tokio::time::timeout(timeout, read)
                .await
                .map_err(|_| Error::Query(uid.clone(), format!("no answer within {}s", timeout.as_secs())))?
        })
    }
}

impl Source for Postgres {
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
        &self.adapter
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
