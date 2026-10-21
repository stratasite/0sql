//! SQL Server over TDS, through `tiberius`, on the same current-thread
//! runtime the Postgres transport uses.
//!
//! Built without a TLS feature, deliberately. Encryption is off until the
//! `sslmode` key exists — a container on loopback presents a certificate
//! nothing signed, and there is no flag here to trust one — and
//! tiberius's `rustls` feature pulls rustls's default crypto provider,
//! aws-lc, in beside the ring the rest of the tree uses: a second
//! provider, C and assembly in the build, and a panic at run time unless
//! a process default is installed. That is a decision for the `sslmode`
//! change to take with a measurement, not one to take by accident here.
//! Azure SQL requires encryption, so it waits for that key. TDS 7.3 rather
//! than 8.0 for the same reason: 8.0 is TLS-first by definition, and the
//! crate refuses to build it without a TLS feature.

use crate::{read_only, Connection, Result, Rows, Secret, Source, SourceError};
use tokio_util::compat::TokioAsyncWriteCompatExt;

type Client = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

pub struct SqlServer {
    runtime: tokio::runtime::Runtime,
    client: Client,
    uid: String,
    timeout: std::time::Duration,
}

impl SqlServer {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<SqlServer> {
        let host = conn.need(&conn.host, "host")?;
        let user = conn.need(&conn.username, "username")?;
        let password =
            secret.ok_or_else(|| SourceError::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;
        let mut config = tiberius::Config::new();
        config.host(&host);
        config.port(conn.port.unwrap_or(1433));
        config.authentication(tiberius::AuthMethod::sql_server(&user, password.expose()));
        if let Some(db) = &conn.database {
            config.database(db);
        }
        config.application_name("zsql");
        config.encryption(tiberius::EncryptionLevel::NotSupported);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        let connect = |e: String| SourceError::Connect(conn.uid.clone(), e);
        let client = runtime
            .block_on(async {
                let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::TcpStream::connect(config.get_addr()))
                    .await
                    .map_err(|_| "no connection within 10s".to_string())?
                    .map_err(|e| e.to_string())?;
                tcp.set_nodelay(true).map_err(|e| e.to_string())?;
                tiberius::Client::connect(config, tcp.compat_write()).await.map_err(|e| e.to_string())
            })
            .map_err(connect)?;
        Ok(SqlServer { runtime, client, uid: conn.uid.clone(), timeout: conn.timeout() })
    }
}

impl Source for SqlServer {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let uid = self.uid.clone();
        let timeout = self.timeout;
        let client = &mut self.client;
        let sets = self
            .runtime
            .block_on(async {
                tokio::time::timeout(timeout, async {
                    let stream = client.simple_query(sql).await.map_err(|e| e.to_string())?;
                    stream.into_results().await.map_err(|e| e.to_string())
                })
                .await
                .map_err(|_| format!("no answer within {}s", timeout.as_secs()))?
            })
            .map_err(|e| SourceError::Query(uid.clone(), e))?;
        let rows_in = sets.into_iter().next().unwrap_or_default();
        if rows_in.len() > max_rows {
            return Err(SourceError::TooManyRows(uid, max_rows));
        }
        let columns: Vec<String> = rows_in.first().map(|r| r.columns().iter().map(|c| c.name().to_string()).collect()).unwrap_or_default();
        let rows = rows_in.into_iter().map(|r| r.cells().map(|(_, d)| cell(d)).collect()).collect();
        Ok(Rows { columns, rows })
    }

    fn adapter(&self) -> &str {
        "sqlserver"
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
