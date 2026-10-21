//! The wire transport, on `tokio-postgres`: Postgres itself, and Redshift
//! when it is reached over the wire rather than through its Data API.
//!
//! Synchronous from the outside. The caller is a command-line tool, and a
//! current-thread runtime held for the life of the connection costs
//! nothing beside a warehouse round trip while saving every caller from
//! being async. The crate stays free to grow an async twin if the server
//! ever needs one.

use crate::{read_only, tls, Connection, Result, Rows, Secret, Source, SourceError, SslMode};

pub struct Postgres {
    runtime: tokio::runtime::Runtime,
    client: tokio_postgres::Client,
    adapter: String,
    uid: String,
    timeout: std::time::Duration,
    warnings: Vec<String>,
}

impl Postgres {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<Postgres> {
        let host = conn.need(&conn.host, "host")?;
        let user = conn.need(&conn.username, "username")?;
        let database = conn.need(&conn.database, "database")?;
        let password =
            secret.ok_or_else(|| SourceError::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;

        // The statement timeout is the warehouse's to enforce, so it is set
        // on the session rather than trusted from this side alone.
        let mut options = format!("-c statement_timeout={}", conn.timeout().as_millis());
        if let Some(schema) = &conn.schema {
            options.push_str(&format!(" -c search_path={schema}"));
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
            .port(conn.port.unwrap_or(5432))
            .user(&user)
            .dbname(&database)
            .password(password.expose())
            .application_name("zsql")
            .options(&options)
            .connect_timeout(std::time::Duration::from_secs(10))
            .ssl_mode(ssl);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;

        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls::client_config(&conn.uid, conn.sslmode, conn.ca_file.as_deref())?);
        let (client, connection) =
            runtime.block_on(config.connect(tls)).map_err(|e| SourceError::Connect(conn.uid.clone(), crate::chain(&e)))?;
        // The connection drives the socket and must be polled for the
        // client to work; it ends when the client is dropped.
        runtime.spawn(async move {
            let _ = connection.await;
        });

        let mut pg =
            Postgres { runtime, client, adapter: conn.adapter.clone(), uid: conn.uid.clone(), timeout: conn.timeout(), warnings: vec![] };
        // `prefer` may have ended plaintext, and the server knows which.
        if conn.sslmode == SslMode::Prefer {
            let rows = pg.query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", 1)?;
            if rows.rows.first().and_then(|r| rows.get(r, "ssl")) != Some("t") {
                pg.warnings.push(format!("{}: sslmode is prefer and the server offered no TLS, so nothing is encrypted", conn.uid));
            }
        }
        Ok(pg)
    }
}

impl Source for Postgres {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let fail = |e: tokio_postgres::Error| SourceError::Query(self.uid.clone(), crate::chain(&e));
        // One row past the cap, so a result at the cap can be told from one
        // over it and the caller is refused rather than quietly trimmed.
        let statement = self.runtime.block_on(self.client.prepare(sql)).map_err(fail)?;
        let rows = self
            .runtime
            .block_on(async { tokio::time::timeout(self.timeout, self.client.query(&statement, &[])).await })
            .map_err(|_| SourceError::Query(self.uid.clone(), format!("no answer within {}s", self.timeout.as_secs())))?
            .map_err(|e| SourceError::Query(self.uid.clone(), crate::chain(&e)))?;
        if rows.len() > max_rows {
            return Err(SourceError::TooManyRows(self.uid.clone(), max_rows));
        }

        let columns: Vec<String> = statement.columns().iter().map(|c| c.name().to_string()).collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut cells = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                // Everything comes back as text: the catalogue is strings,
                // and a sampled value is the string a consumer matches on.
                cells.push(row.try_get::<_, Option<String>>(i).unwrap_or(None));
            }
            out.push(cells);
        }
        Ok(Rows { columns, rows: out })
    }

    fn adapter(&self) -> &str {
        &self.adapter
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
