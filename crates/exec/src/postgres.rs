//! The records of runs in Postgres: the table every node of a cluster
//! shares, and an application reads to show what the servers are doing.
//!
//! The table is the application's to create (`ops/zsql_results.sql` is
//! its reference); this checks it when it opens and says what is
//! missing. Every statement this module runs is written out here, and a
//! row becomes a [`Record`] in one place. No ORM: one table does not
//! need one.
//!
//! Times cross as text and are cast on the other side, and JSON the
//! same way, so no feature of the driver is needed for either.

use crate::error::{Error, Result};
use crate::record::{Kind, Record, Status};
use crate::registry::Registry;
use std::time::Duration;
use zsql_datasource::{Connection, Secret, SslMode};

/// The table's name.
pub const TABLE: &str = "zsql_results";

/// Where the records are kept.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `host:port/database`, with or without `postgres://` in front and
    /// `?sslmode=` behind.
    pub url: String,
    pub user: String,
    pub password: Secret,
}

/// Every column the table must have, in the order a record is read.
const COLUMNS: [&str; 25] = [
    "id",
    "type",
    "status",
    "error",
    "row_count",
    "cache_key",
    "dirty",
    "tables",
    "org_uid",
    "project_uid",
    "branch",
    "datasource_uid",
    "sql",
    "spec",
    "expr",
    "row_limit",
    "hostname",
    "object_store_path",
    "created_at",
    "queued_at",
    "started_at",
    "finished_at",
    "updated_at",
    "cancelled_at",
    "expires_at",
];

/// A time as a record spells it: RFC 3339, UTC, to the second.
macro_rules! time {
    ($column:literal) => {
        concat!("to_char(", $column, " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
    };
}

/// A record's columns, as a row [`record`] reads.
const SELECT: &str = concat!(
    "SELECT id, type, status, error, row_count, cache_key, dirty, tables, org_uid, project_uid, branch, datasource_uid, sql, spec::text, expr, \
     row_limit, hostname, object_store_path, ",
    time!("created_at"),
    ", ",
    time!("queued_at"),
    ", ",
    time!("started_at"),
    ", ",
    time!("finished_at"),
    ", ",
    time!("updated_at"),
    ", ",
    time!("cancelled_at"),
    ", ",
    time!("expires_at"),
    " FROM zsql_results"
);

const INSERT: &str =
    "INSERT INTO zsql_results (id, type, status, error, row_count, cache_key, dirty, tables, org_uid, project_uid, branch, \
     datasource_uid, sql, spec, expr, row_limit, hostname, object_store_path, created_at, queued_at, started_at, finished_at, updated_at, \
     cancelled_at, expires_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::text::jsonb, $15, $16, $17, $18, $19::text::timestamptz, \
     $20::text::timestamptz, $21::text::timestamptz, $22::text::timestamptz, $23::text::timestamptz, $24::text::timestamptz, \
     $25::text::timestamptz)";

/// What a run changes about itself as it goes. `dirty` is not among
/// them: only an invalidation sets it, and a run that ends after one
/// must not unset it.
const UPDATE: &str = "UPDATE zsql_results SET status = $2, error = $3, row_count = $4, object_store_path = $5, hostname = $6, \
     started_at = $7::text::timestamptz, finished_at = $8::text::timestamptz, updated_at = $9::text::timestamptz, \
     cancelled_at = $10::text::timestamptz, expires_at = $11::text::timestamptz WHERE id = $1";

/// The records of runs, in Postgres.
pub struct Postgres {
    // One thread that is always there to drive the connections, so a
    // call from any thread only has to wait for its answer.
    runtime: Option<tokio::runtime::Runtime>,
    pool: deadpool_postgres::Pool,
}

impl Drop for Postgres {
    /// Let go of without waiting, so the registry can be dropped from
    /// anywhere, a server's own async shutdown included.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn failed(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Registry(format!("{what}: {e}"))
}

impl Postgres {
    /// Connects, and checks that the table is there with every column.
    pub fn open(settings: &Settings) -> Result<Postgres> {
        let db = Postgres::connect(settings)?;
        db.check()?;
        Ok(db)
    }

    /// Connects without looking for the table: for a test, which has
    /// to make the table before anything can check it.
    #[doc(hidden)]
    pub fn connect(settings: &Settings) -> Result<Postgres> {
        let conn = connection(settings)?;
        let (config, tls) = zsql_datasource::postgres_settings(&conn, &settings.password).map_err(|e| failed("ZSQL_DB", e))?;
        let manager = deadpool_postgres::Manager::from_config(
            config,
            tls,
            deadpool_postgres::ManagerConfig { recycling_method: deadpool_postgres::RecyclingMethod::Fast },
        );
        let pool = deadpool_postgres::Pool::builder(manager).max_size(8).build().map_err(|e| failed("ZSQL_DB", e))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("zsql-db")
            .enable_all()
            .build()
            .map_err(|e| failed("ZSQL_DB", e))?;
        Ok(Postgres { runtime: Some(runtime), pool })
    }

    /// The table and each of its columns, or what is missing.
    fn check(&self) -> Result<()> {
        let found: Vec<String> = self.with(|client| async move {
            let rows = client
                .query(
                    "SELECT column_name::text FROM information_schema.columns WHERE table_name = $1 AND table_schema = ANY (current_schemas(false))",
                    &[&TABLE],
                )
                .await?;
            Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
        })?;
        if found.is_empty() {
            return Err(Error::Registry(format!(
                "ZSQL_DB has no table {TABLE}. The application that fronts this server creates it; ops/zsql_results.sql is the table it needs"
            )));
        }
        let missing: Vec<&str> = COLUMNS.iter().copied().filter(|c| !found.iter().any(|f| f == c)).collect();
        if !missing.is_empty() {
            return Err(Error::Registry(format!(
                "the table {TABLE} in ZSQL_DB is missing the column{} {}; ops/zsql_results.sql is the table this server needs",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// One piece of work on a connection from the pool, waited for on
    /// the thread that asked: a run's own, or one the server lends for
    /// blocking work. The runtime's own thread drives the sockets.
    fn with<T, F, Fut>(&self, work: F) -> Result<T>
    where
        F: FnOnce(deadpool_postgres::Object) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, tokio_postgres::Error>>,
    {
        let Some(runtime) = &self.runtime else { return Err(Error::Registry("ZSQL_DB: the registry is closed".into())) };
        runtime.block_on(async {
            let client = self.pool.get().await.map_err(|e| failed("connecting to ZSQL_DB", zsql_datasource::chain(&e)))?;
            work(client).await.map_err(|e| match e.code() {
                // Which constraint refused it is what tells a taken id
                // from a question already in flight.
                Some(code) if *code == tokio_postgres::error::SqlState::UNIQUE_VIOLATION => {
                    Error::Taken(e.as_db_error().and_then(|d| d.constraint()).unwrap_or_default().to_string())
                }
                _ => failed("ZSQL_DB", zsql_datasource::chain(&e)),
            })
        })
    }

    /// Runs statements this module did not write: for a test to make
    /// the table, which the server itself never does.
    #[doc(hidden)]
    pub fn batch_execute(&self, sql: &str) -> Result<()> {
        let sql = sql.to_string();
        self.with(|client| async move { client.batch_execute(&sql).await })
    }

    fn one(&self, filter: &'static str, key: String) -> Result<Option<Record>> {
        let rows = self.with(move |client| async move { client.query(&format!("{SELECT} {filter}"), &[&key]).await })?;
        rows.first().map(record).transpose()
    }
}

/// The datasource the registry's URL describes.
fn connection(settings: &Settings) -> Result<Connection> {
    let bad = || Error::Registry(format!("ZSQL_DB is host:port/database, with ?sslmode= when it says; it is {:?}", settings.url));
    let rest = settings.url.trim();
    let rest = rest.strip_prefix("postgresql://").or_else(|| rest.strip_prefix("postgres://")).unwrap_or(rest);
    if rest.contains('@') {
        return Err(Error::Registry("ZSQL_DB carries no user or password: those are ZSQL_DB_USER and ZSQL_DB_PASSWORD".into()));
    }
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host_port, database) = address.split_once('/').ok_or_else(bad)?;
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().map_err(|_| bad())?)),
        None => (host_port, None),
    };
    if host.is_empty() || database.is_empty() {
        return Err(bad());
    }
    let mut conn = Connection {
        uid: "zsql-db".into(),
        adapter: "postgres".into(),
        host: Some(host.to_string()),
        port,
        database: Some(database.to_string()),
        username: Some(settings.user.clone()),
        ..Default::default()
    };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("sslmode", mode)) => conn.sslmode = SslMode::parse(mode).map_err(|e| failed("ZSQL_DB", e))?,
            Some(("sslrootcert", file)) => conn.ca_file = Some(file.into()),
            _ => return Err(Error::Registry(format!("ZSQL_DB takes sslmode and sslrootcert after the ?; it has {pair:?}"))),
        }
    }
    Ok(conn)
}

/// A row [`SELECT`] returned, as the record it is.
fn record(row: &tokio_postgres::Row) -> Result<Record> {
    let text = |at: usize| row.get::<_, Option<String>>(at);
    let count = |at: usize| row.get::<_, Option<i64>>(at).and_then(|n| u64::try_from(n).ok());
    let need = |at: usize| text(at).ok_or_else(|| Error::Registry(format!("{TABLE}.{} is null where a run always says", COLUMNS[at])));
    let kind = match need(1)?.as_str() {
        "semantic" => Kind::Semantic,
        "sql" => Kind::Sql,
        other => return Err(Error::Registry(format!("{TABLE}.type is {other:?}"))),
    };
    let status = match need(2)?.as_str() {
        "queued" => Status::Queued,
        "running" => Status::Running,
        "succeeded" => Status::Succeeded,
        "failed" => Status::Failed,
        "cancelled" => Status::Cancelled,
        other => return Err(Error::Registry(format!("{TABLE}.status is {other:?}"))),
    };
    Ok(Record {
        id: need(0)?,
        kind,
        status,
        error: text(3),
        row_count: count(4),
        cache_key: text(5),
        dirty: row.get::<_, Option<bool>>(6).unwrap_or(false),
        tables: row.get::<_, Option<Vec<String>>>(7).unwrap_or_default(),
        org_uid: need(8)?,
        project_uid: need(9)?,
        branch: need(10)?,
        datasource_uid: need(11)?,
        sql: need(12)?,
        spec: text(13).map(|t| serde_json::from_str(&t)).transpose()?,
        expr: text(14),
        row_limit: count(15),
        hostname: need(16)?,
        object_store_path: text(17),
        created_at: need(18)?,
        queued_at: need(19)?,
        started_at: text(20),
        finished_at: text(21),
        updated_at: need(22)?,
        cancelled_at: text(23),
        expires_at: text(24),
    })
}

impl Registry for Postgres {
    fn insert(&self, r: &Record) -> Result<()> {
        let r = r.clone();
        let (id, key) = (r.id.clone(), r.cache_key.clone());
        let spec = r.spec.as_ref().map(|s| s.to_string());
        let inserted = self.with(move |client| async move {
            client
                .execute(
                    INSERT,
                    &[
                        &r.id,
                        &r.kind.as_str(),
                        &r.status.as_str(),
                        &r.error,
                        &r.row_count.map(|n| n as i64),
                        &r.cache_key,
                        &r.dirty,
                        &r.tables,
                        &r.org_uid,
                        &r.project_uid,
                        &r.branch,
                        &r.datasource_uid,
                        &r.sql,
                        &spec,
                        &r.expr,
                        &r.row_limit.map(|n| n as i64),
                        &r.hostname,
                        &r.object_store_path,
                        &r.created_at,
                        &r.queued_at,
                        &r.started_at,
                        &r.finished_at,
                        &r.updated_at,
                        &r.cancelled_at,
                        &r.expires_at,
                    ],
                )
                .await
        });
        match inserted {
            Ok(_) => Ok(()),
            // Refused as a duplicate: of the id, or of a run of the same
            // question that another node already has in flight.
            Err(Error::Taken(constraint)) if constraint.ends_with("in_flight") => Err(Error::InFlight(key.unwrap_or_default())),
            Err(Error::Taken(_)) => Err(Error::Duplicate(id)),
            Err(e) => Err(e),
        }
    }

    fn save(&self, r: &Record) -> Result<()> {
        let r = r.clone();
        self.with(move |client| async move {
            client
                .execute(
                    UPDATE,
                    &[
                        &r.id,
                        &r.status.as_str(),
                        &r.error,
                        &r.row_count.map(|n| n as i64),
                        &r.object_store_path,
                        &r.hostname,
                        &r.started_at,
                        &r.finished_at,
                        &r.updated_at,
                        &r.cancelled_at,
                        &r.expires_at,
                    ],
                )
                .await
        })
        .map(|_| ())
    }

    fn get(&self, id: &str) -> Result<Option<Record>> {
        self.one("WHERE id = $1", id.to_string())
    }

    fn cached(&self, cache_key: &str) -> Result<Option<Record>> {
        self.one(
            "WHERE cache_key = $1 AND status = 'succeeded' AND NOT dirty AND expires_at > now() ORDER BY finished_at DESC, id DESC LIMIT 1",
            cache_key.to_string(),
        )
    }

    fn in_flight(&self, cache_key: &str) -> Result<Option<Record>> {
        self.one("WHERE cache_key = $1 AND status IN ('queued', 'running') LIMIT 1", cache_key.to_string())
    }

    fn fail_unfinished(&self, hostname: &str, why: &str) -> Result<usize> {
        let (hostname, why) = (hostname.to_string(), why.to_string());
        self.with(move |client| async move {
            client
                .execute(
                    "UPDATE zsql_results SET status = 'failed', error = $2, finished_at = now(), updated_at = now() \
                     WHERE hostname = $1 AND status IN ('queued', 'running')",
                    &[&hostname, &why],
                )
                .await
        })
        .map(|n| n as usize)
    }

    fn touch(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.with(move |client| async move {
            client
                .execute("UPDATE zsql_results SET updated_at = now() WHERE id = ANY($1) AND status IN ('queued', 'running')", &[&ids])
                .await
        })
        .map(|_| ())
    }

    fn fail_abandoned(&self, quiet_for: Duration, why: &str) -> Result<usize> {
        let (seconds, why) = (quiet_for.as_secs_f64(), why.to_string());
        self.with(move |client| async move {
            client
                .execute(
                    "UPDATE zsql_results SET status = 'failed', error = $2, finished_at = now(), updated_at = now() \
                     WHERE status IN ('queued', 'running') AND updated_at < now() - make_interval(secs => $1)",
                    &[&seconds, &why],
                )
                .await
        })
        .map(|n| n as usize)
    }
    fn mark_dirty(&self, org_uid: &str, project_uid: Option<&str>, datasource_uid: &str, tables: &[String]) -> Result<usize> {
        let (org, project, datasource, tables) =
            (org_uid.to_string(), project_uid.map(str::to_string), datasource_uid.to_string(), tables.to_vec());
        self.with(move |client| async move {
            client
                .execute(
                    "UPDATE zsql_results SET dirty = true, updated_at = now() \
                     WHERE NOT dirty AND org_uid = $1 AND datasource_uid = $2 AND tables && $3 AND ($4::text IS NULL OR project_uid = $4)",
                    &[&org, &datasource, &tables, &project],
                )
                .await
        })
        .map(|n| n as usize)
    }

    fn supersede(&self, newer: &Record) -> Result<usize> {
        let (Some(key), Some(until)) = (newer.cache_key.clone(), newer.expires_at.clone()) else { return Ok(0) };
        let (id, never) = (newer.id.clone(), crate::engine::NEVER_EXPIRES.to_string());
        self.with(move |client| async move {
            // Every SET reads the row as it was, so a result that never
            // expired is told apart before its expiry is changed.
            client
                .execute(
                    "UPDATE zsql_results SET \
                       dirty = dirty OR expires_at >= $4::text::timestamptz, \
                       expires_at = CASE WHEN expires_at >= $4::text::timestamptz THEN now() ELSE $3::text::timestamptz END, \
                       updated_at = now() \
                     WHERE cache_key = $1 AND id <> $2 AND status = 'succeeded' \
                       AND (expires_at >= $4::text::timestamptz OR expires_at > $3::text::timestamptz)",
                    &[&key, &id, &until, &never],
                )
                .await
        })
        .map(|n| n as usize)
    }

    fn stale_objects(&self, limit: usize) -> Result<Vec<Record>> {
        let limit = limit as i64;
        let rows = self.with(move |client| async move {
            client
                .query(
                    &format!(
                        "{SELECT} WHERE status = 'succeeded' AND object_store_path IS NOT NULL AND (dirty OR expires_at IS NULL OR expires_at <= now()) LIMIT $1"
                    ),
                    &[&limit],
                )
                .await
        })?;
        rows.iter().map(record).collect()
    }

    /// The table is the application's, and so is what it keeps of it.
    fn prune(&self) -> Result<usize> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: &str) -> Settings {
        Settings { url: url.into(), user: "zsql".into(), password: Secret::new("s3cret") }
    }

    #[test]
    fn the_address_is_read_and_a_credential_in_it_is_refused() {
        let conn = connection(&settings("postgres://db.internal:6432/zsql?sslmode=require")).unwrap();
        assert_eq!((conn.host.as_deref(), conn.port, conn.database.as_deref()), (Some("db.internal"), Some(6432), Some("zsql")));
        assert_eq!((conn.username.as_deref(), conn.sslmode), (Some("zsql"), SslMode::Require));
        let bare = connection(&settings("db.internal/zsql")).unwrap();
        assert_eq!((bare.host.as_deref(), bare.port, bare.sslmode), (Some("db.internal"), None, SslMode::default()));

        let with_login = connection(&settings("postgres://ada:pw@db.internal/zsql")).unwrap_err().to_string();
        assert!(with_login.contains("ZSQL_DB_USER") && !with_login.contains("pw"), "{with_login}");
        for bad in ["db.internal", "db.internal:port/zsql", "/zsql", "db.internal/"] {
            assert!(connection(&settings(bad)).is_err(), "{bad}");
        }
        let odd = connection(&settings("db.internal/zsql?pool=9")).unwrap_err().to_string();
        assert!(odd.contains("sslmode"), "{odd}");
    }
}
