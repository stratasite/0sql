//! MySQL on the wire, through the synchronous `mysql` crate.
//!
//! Built without its TLS feature, deliberately. The crate has no "prefer"
//! mode, only required-and-verified or none, and a container on loopback
//! presents a certificate nothing signed; and the feature drags in the
//! old `webpki` and the unmaintained `rustls-pemfile`, which cargo-deny
//! refuses. The `sslmode` key that turns TLS on is the one B2 item still
//! open, and it is open here for the same reason it is open for Postgres
//! and SQL Server: it should be decided once, with a measurement.

use crate::{read_only, Connection, Result, Rows, Secret, Source, SourceError};
use mysql::prelude::Queryable;

pub struct MySql {
    conn: mysql::Conn,
    uid: String,
}

impl MySql {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<MySql> {
        let host = conn.need(&conn.host, "host")?;
        let user = conn.need(&conn.username, "username")?;
        let password =
            secret.ok_or_else(|| SourceError::NoSecret { uid: conn.uid.clone(), env: crate::secret_env_var(&conn.uid, "password") })?;
        let opts = mysql::OptsBuilder::new()
            .ip_or_hostname(Some(host))
            .tcp_port(conn.port.unwrap_or(3306))
            .user(Some(user))
            .pass(Some(password.expose().to_string()))
            .db_name(conn.database.clone())
            .tcp_connect_timeout(Some(std::time::Duration::from_secs(10)))
            .read_timeout(Some(conn.timeout()))
            .write_timeout(Some(std::time::Duration::from_secs(30)));
        let mut c = mysql::Conn::new(opts).map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        // The statement timeout, enforced by the server; a SET is not user
        // SQL, so it does not pass through the guard.
        c.query_drop(format!("SET SESSION max_execution_time = {}", conn.timeout().as_millis()))
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        Ok(MySql { conn: c, uid: conn.uid.clone() })
    }
}

impl Source for MySql {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let fail = |e: mysql::Error| SourceError::Query(self.uid.clone(), e.to_string());
        let mut result = self.conn.query_iter(sql).map_err(fail)?;
        let columns: Vec<String> = result.columns().as_ref().iter().map(|c| c.name_str().to_string()).collect();
        let mut out = Vec::new();
        for row in result.by_ref() {
            let row = row.map_err(fail)?;
            if out.len() == max_rows {
                return Err(SourceError::TooManyRows(self.uid.clone(), max_rows));
            }
            out.push(row.unwrap().into_iter().map(cell).collect());
        }
        Ok(Rows { columns, rows: out })
    }

    fn adapter(&self) -> &str {
        "mysql"
    }
}

/// A MySQL value as text. Bytes are what strings and decimals arrive as.
fn cell(v: mysql::Value) -> Option<String> {
    use mysql::Value::*;
    match v {
        NULL => None,
        Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Int(n) => Some(n.to_string()),
        UInt(n) => Some(n.to_string()),
        Float(f) => Some(f.to_string()),
        Double(f) => Some(f.to_string()),
        Date(y, m, d, h, i, s, _) => Some(if h == 0 && i == 0 && s == 0 {
            format!("{y:04}-{m:02}-{d:02}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}")
        }),
        Time(neg, days, h, i, s, _) => Some(format!("{}{:02}:{i:02}:{s:02}", if neg { "-" } else { "" }, u32::from(h) + days * 24)),
    }
}
