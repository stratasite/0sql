//! Reaching a datasource to read from it.
//!
//! The planner never connects to a warehouse; this crate is the only
//! place that does, which is what makes the question "what can open a
//! socket" answerable by reading one directory. It holds three things:
//!
//! - [`Connection`], where a datasource lives, read from the
//!   `datasources.yml` keys the model loader ignores;
//! - [`Secret`], resolved at connect time from the environment or the
//!   project's gitignored `.zsql`, and never part of a deployed model;
//! - [`Source`], one read-only statement in and [`Rows`] out, which is
//!   all that introspecting a catalog and sampling a dimension need.
//!
//! Statements come from `zsql` itself, never from a caller, and the
//! guard in [`read_only`] is a second pair of eyes on that rather than a
//! sandbox: the protection that matters is a warehouse credential with
//! no write grant. See `docs/introspection.md`.

mod adapters;
#[cfg(not(target_env = "musl"))]
pub mod adbc;
pub mod batch;
pub mod catalog;
mod connection;
mod error;
mod http;
mod jwt;
mod rows;
mod secret;
mod sigv4;
mod source;
mod tls;

use adapters::{athena, bigquery, clickhouse, databricks, duckdb, mysql, postgres, snowflake, sqlite, sqlserver, trino};

pub use batch::Batches;
pub use catalog::{Added, Catalog, Column, Drift, Filter, ForeignKey, Listing, Names, Scope, Table};
pub use connection::{Connection, SslMode, CONNECTION_KEYS};
pub use error::{chain, Error, Result};
pub use rows::Rows;
pub use secret::{secret, secret_env_var, secrets, Secret, Secrets, SECRET_FIELDS};
pub(crate) use source::Deadline;
pub use source::{read_only, Sink, Source};

/// Whether this adapter can be opened with [`Secrets::oauth_token`]: an
/// access token obtained elsewhere and sent as the credential. The ones
/// that cannot have no bearer login on the protocol spoken here.
pub fn takes_oauth_token(adapter: &str) -> bool {
    matches!(adapter, "snowflake" | "bigquery" | "databricks" | "trino" | "sqlserver")
}

/// Opens the adapter this connection names, with whichever of the
/// secrets it takes.
pub fn open(conn: &Connection, secrets: Secrets) -> Result<Box<dyn Source>> {
    let started = std::time::Instant::now();
    let opened = open_adapter(conn, secrets);
    match &opened {
        Ok(src) => tracing::debug!(
            datasource = %conn.uid,
            adapter = %conn.adapter,
            transport = src.transport(),
            ms = started.elapsed().as_millis() as u64,
            "a datasource was opened"
        ),
        // The caller reports the failure; this is where and how long.
        Err(e) => {
            tracing::debug!(datasource = %conn.uid, adapter = %conn.adapter, ms = started.elapsed().as_millis() as u64, "a datasource did not open: {e}")
        }
    }
    opened
}

fn open_adapter(conn: &Connection, secrets: Secrets) -> Result<Box<dyn Source>> {
    let secret = secrets.password.clone();
    match conn.adapter.as_str() {
        "postgres" | "redshift" => Ok(Box::new(postgres::Postgres::open(conn, secret)?)),
        "clickhouse" => Ok(Box::new(clickhouse::ClickHouse::open(conn, secret)?)),
        "mysql" => Ok(Box::new(mysql::MySql::open(conn, secret)?)),
        "trino" => Ok(Box::new(trino::Trino::open(conn, secret, secrets.oauth_token)?)),
        "athena" => Ok(Box::new(athena::Athena::open(conn, secrets)?)),
        "sqlserver" => Ok(Box::new(sqlserver::SqlServer::open(conn, secret, secrets.oauth_token)?)),
        "snowflake" => {
            let rest = Box::new(snowflake::Snowflake::open(conn, secrets.clone())?);
            #[cfg(not(target_env = "musl"))]
            return Ok(Box::new(adbc::WithAdbc::new(rest, adbc::snowflake(conn, &secrets), conn)));
            #[cfg(target_env = "musl")]
            Ok(rest)
        }
        "databricks" => Ok(Box::new(databricks::Databricks::open(conn, secrets)?)),
        "bigquery" => {
            let rest = Box::new(bigquery::BigQuery::open(conn, secrets.clone())?);
            #[cfg(not(target_env = "musl"))]
            if let Some(plan) = adbc::bigquery(conn, &secrets, &|k| std::env::var(k).ok()) {
                return Ok(Box::new(adbc::WithAdbc::new(rest, plan, conn)));
            }
            Ok(rest)
        }
        "sqlite" => Ok(Box::new(sqlite::Sqlite::open(conn)?)),
        "duckdb" => Ok(Box::new(duckdb::DuckDb::open(conn)?)),
        other => Err(Error::Unsupported(other.to_string())),
    }
}

#[cfg(test)]
mod tests;
