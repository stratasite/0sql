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
        if redshift {
            let mut session = format!("SET statement_timeout TO {}", conn.timeout().as_millis());
            if let Some(schema) = &conn.schema {
                session.push_str(&format!("; SET search_path TO {schema}"));
            }
            pg.runtime.block_on(pg.client.simple_query(&session)).map_err(|e| SourceError::Connect(conn.uid.clone(), crate::chain(&e)))?;
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

impl Source for Postgres {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        // The simple-query protocol: every value arrives as the server's
        // own text, whatever its type, which is what a catalogue and a
        // sample want. The extended protocol sends binary, and reading a
        // boolean or a number as a string there yields nothing at all —
        // which is how every non-text cell was null until a test asked
        // pg_stat_ssl a yes-or-no question.
        let uid = self.uid.clone();
        let messages = self
            .runtime
            .block_on(async { tokio::time::timeout(self.timeout, self.client.simple_query(sql)).await })
            .map_err(|_| SourceError::Query(uid.clone(), format!("no answer within {}s", self.timeout.as_secs())))?
            .map_err(|e| SourceError::Query(uid.clone(), crate::chain(&e)))?;
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        for m in messages {
            match m {
                tokio_postgres::SimpleQueryMessage::RowDescription(cols) => {
                    columns = cols.iter().map(|c| c.name().to_string()).collect();
                }
                tokio_postgres::SimpleQueryMessage::Row(row) => {
                    if rows.len() == max_rows {
                        // Past the cap: refuse rather than trim.
                        return Err(SourceError::TooManyRows(uid, max_rows));
                    }
                    if columns.is_empty() {
                        columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    rows.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
                }
                _ => {}
            }
        }
        Ok(Rows { columns, rows })
    }

    fn adapter(&self) -> &str {
        &self.adapter
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
