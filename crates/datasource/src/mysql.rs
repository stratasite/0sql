//! MySQL on the wire, through `mysql_async` on the same current-thread
//! runtime the Postgres transport uses, with rustls on ring. The
//! synchronous `mysql` crate was the first choice and its TLS feature
//! dragged in the old `webpki` and the unmaintained `rustls-pemfile`;
//! this crate's tree is ring and the bundle alone.
//!
//! MySQL's `--ssl-mode` is libpq's ladder under other names, and maps one
//! to one. The one difference from the other adapters: verification uses
//! the bundle the crate carries plus `ca_file`, not the platform's
//! store, because the crate takes certificate files and not a store.

use crate::{read_only, Connection, Result, Rows, Secret, Source, SourceError, SslMode};
use mysql_async::prelude::Queryable;

pub struct MySql {
    runtime: tokio::runtime::Runtime,
    conn: mysql_async::Conn,
    uid: String,
    warnings: Vec<String>,
}

impl MySql {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<MySql> {
        let host = conn.need(&conn.host, "host")?;
        let user = conn.need(&conn.username, "username")?;
        let password =
            secret.ok_or_else(|| SourceError::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;
        crate::tls::install_ring();

        let ssl = |mode: SslMode| -> Option<mysql_async::SslOpts> {
            let roots = conn.ca_file.as_ref().map(|p| vec![p.clone().into()]).unwrap_or_default();
            match mode {
                SslMode::Disable => None,
                SslMode::Prefer | SslMode::Require => Some(mysql_async::SslOpts::default().with_danger_accept_invalid_certs(true)),
                SslMode::VerifyCa => Some(mysql_async::SslOpts::default().with_root_certs(roots).with_danger_skip_domain_validation(true)),
                SslMode::VerifyFull => Some(mysql_async::SslOpts::default().with_root_certs(roots)),
            }
        };
        let opts = |ssl_opts: Option<mysql_async::SslOpts>| {
            mysql_async::OptsBuilder::default()
                .ip_or_hostname(host.clone())
                .tcp_port(conn.port.unwrap_or(3306))
                .user(Some(user.clone()))
                .pass(Some(password.expose().to_string()))
                .db_name(conn.database.clone())
                .ssl_opts(ssl_opts)
        };
        let plan: Vec<Option<mysql_async::SslOpts>> = match conn.sslmode {
            SslMode::Prefer => vec![ssl(SslMode::Prefer), None],
            other => vec![ssl(other)],
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        let mut warnings = Vec::new();
        let mut last = String::new();
        let mut c = None;
        for (i, ssl_opts) in plan.into_iter().enumerate() {
            match runtime.block_on(mysql_async::Conn::new(opts(ssl_opts))) {
                Ok(conn_) => {
                    if i > 0 {
                        warnings.push(format!("{}: sslmode is prefer and TLS failed ({last}), so nothing is encrypted", conn.uid));
                    }
                    c = Some(conn_);
                    break;
                }
                Err(e) => last = crate::chain(&e),
            }
        }
        let mut c = c.ok_or_else(|| SourceError::Connect(conn.uid.clone(), last))?;
        // The statement timeout, enforced by the server; a SET is not user
        // SQL, so it does not pass through the guard.
        runtime
            .block_on(c.query_drop(format!("SET SESSION max_execution_time = {}", conn.timeout().as_millis())))
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        Ok(MySql { runtime, conn: c, uid: conn.uid.clone(), warnings })
    }
}

impl Source for MySql {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let uid = self.uid.clone();
        let fail = |e: mysql_async::Error| SourceError::Query(uid.clone(), crate::chain(&e));
        let conn = &mut self.conn;
        self.runtime.block_on(async {
            let mut result = conn.query_iter(sql).await.map_err(fail)?;
            let columns: Vec<String> = result.columns().map(|c| c.iter().map(|c| c.name_str().to_string()).collect()).unwrap_or_default();
            // Which columns are dates without a time: the value alone
            // cannot say, since a DATETIME at midnight looks the same.
            let dates: Vec<bool> = result
                .columns()
                .map(|c| c.iter().map(|c| c.column_type() == mysql_async::consts::ColumnType::MYSQL_TYPE_DATE).collect())
                .unwrap_or_default();
            let rows: Vec<mysql_async::Row> = result.collect().await.map_err(fail)?;
            if rows.len() > max_rows {
                return Err(SourceError::TooManyRows(uid.clone(), max_rows));
            }
            Ok(Rows {
                columns,
                rows: rows
                    .into_iter()
                    .map(|r| r.unwrap().into_iter().enumerate().map(|(i, v)| cell(v, dates.get(i).copied().unwrap_or(false))).collect())
                    .collect(),
            })
        })
    }

    fn adapter(&self) -> &str {
        "mysql"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// A MySQL value as text. Bytes are what strings and decimals arrive as;
/// `date_only` says the column is a DATE, so its value has no time to print.
fn cell(v: mysql_async::Value, date_only: bool) -> Option<String> {
    use mysql_async::Value::*;
    match v {
        NULL => None,
        Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Int(n) => Some(n.to_string()),
        UInt(n) => Some(n.to_string()),
        Float(f) => Some(f.to_string()),
        Double(f) => Some(f.to_string()),
        Date(y, m, d, h, i, s, micro) => Some(if date_only {
            format!("{y:04}-{m:02}-{d:02}")
        } else if micro > 0 {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}.{micro:06}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}")
        }),
        Time(neg, days, h, i, s, _) => Some(format!("{}{:02}:{i:02}:{s:02}", if neg { "-" } else { "" }, u32::from(h) + days * 24)),
    }
}
