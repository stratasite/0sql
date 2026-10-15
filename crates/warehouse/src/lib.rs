//! Runs SQL against a project's datasources. Used by the CLI to sample
//! dimension values; the service never connects to a warehouse.
//!
//! Connection settings are the datasource's entry in `datasources.yml`;
//! secrets come from `.strata` in the project directory (the strata CLI's
//! credential file, keyed by datasource key) or from the environment as
//! `ZSQL_DS_<KEY>_PASSWORD`.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum WarehouseError {
    #[error("adapter '{0}' is not supported for sampling yet (duckdb and postgres are)")]
    Unsupported(String),
    #[error("datasource {0}: {1} is required")]
    Missing(String, &'static str),
    #[error("{0}")]
    Connect(String),
    #[error("{0}")]
    Query(String),
}

type Result<T> = std::result::Result<T, WarehouseError>;

/// One row as text, NULL as None. Enough for sampling; no type mapping.
pub type Row = Vec<Option<String>>;

pub trait Warehouse {
    fn query(&mut self, sql: &str) -> Result<Vec<Row>>;
}

/// Settings for one datasource: its `datasources.yml` entry merged with
/// the secrets found for its key.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub key: String,
    pub adapter: String,
    pub values: BTreeMap<String, String>,
}

impl Settings {
    fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }
    fn need(&self, k: &'static str) -> Result<&str> {
        self.get(k).ok_or_else(|| WarehouseError::Missing(self.key.clone(), k))
    }
}

/// Reads a datasource's settings: `datasources.yml` entry, then `.strata`
/// secrets for the same key, then `ZSQL_DS_<KEY>_<SETTING>` environment
/// variables, later ones winning.
pub fn settings_for(project_dir: &Path, key: &str) -> Result<Settings> {
    let mut s = Settings { key: key.to_string(), ..Default::default() };
    let mut absorb = |path: &Path| {
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(serde_yaml::Value::Mapping(top)) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { return };
        let Some(serde_yaml::Value::Mapping(entry)) = top.get(serde_yaml::Value::String(key.to_string())) else { return };
        for (k, v) in entry {
            if let (Some(k), Some(v)) = (k.as_str(), scalar(v)) {
                s.values.insert(k.to_string(), v);
            }
        }
    };
    absorb(&project_dir.join("datasources.yml"));
    absorb(&project_dir.join(".strata"));
    let prefix = format!("ZSQL_DS_{}_", key.to_uppercase().replace('-', "_"));
    for (k, v) in std::env::vars() {
        if let Some(setting) = k.strip_prefix(&prefix) {
            s.values.insert(setting.to_lowercase(), v);
        }
    }
    s.adapter = s.get("adapter").unwrap_or_default().to_lowercase();
    Ok(s)
}

fn scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn connect(settings: &Settings) -> Result<Box<dyn Warehouse>> {
    match settings.adapter.as_str() {
        "duckdb" => Ok(Box::new(DuckDb::open(settings)?)),
        "postgres" | "postgresql" => Ok(Box::new(Postgres::open(settings)?)),
        other => Err(WarehouseError::Unsupported(other.to_string())),
    }
}

// ------------------------------------------------------------------ duckdb

pub struct DuckDb {
    conn: duckdb::Connection,
}

impl DuckDb {
    pub fn open(settings: &Settings) -> Result<Self> {
        let file = settings.need("file")?;
        let config =
            duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly).map_err(|e| WarehouseError::Connect(e.to_string()))?;
        let conn = duckdb::Connection::open_with_flags(file, config).map_err(|e| WarehouseError::Connect(format!("{file}: {e}")))?;
        Ok(DuckDb { conn })
    }

    pub fn in_memory() -> Result<Self> {
        Ok(DuckDb { conn: duckdb::Connection::open_in_memory().map_err(|e| WarehouseError::Connect(e.to_string()))? })
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql).map_err(|e| WarehouseError::Query(e.to_string()))
    }
}

impl Warehouse for DuckDb {
    fn query(&mut self, sql: &str) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| WarehouseError::Query(e.to_string()))?;
        let mut rows = stmt.query([]).map_err(|e| WarehouseError::Query(e.to_string()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| WarehouseError::Query(e.to_string()))? {
            let n = row.as_ref().column_count();
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                let v = row.get_ref(i).map_err(|e| WarehouseError::Query(e.to_string()))?;
                cells.push(duck_text(v));
            }
            out.push(cells);
        }
        Ok(out)
    }
}

fn duck_text(v: duckdb::types::ValueRef<'_>) -> Option<String> {
    use duckdb::types::ValueRef::*;
    Some(match v {
        Null => return None,
        Boolean(b) => b.to_string(),
        TinyInt(i) => i.to_string(),
        SmallInt(i) => i.to_string(),
        Int(i) => i.to_string(),
        BigInt(i) => i.to_string(),
        HugeInt(i) => i.to_string(),
        UTinyInt(i) => i.to_string(),
        USmallInt(i) => i.to_string(),
        UInt(i) => i.to_string(),
        UBigInt(i) => i.to_string(),
        Float(f) => f.to_string(),
        Double(f) => f.to_string(),
        Decimal(d) => d.to_string(),
        Text(t) => String::from_utf8_lossy(t).into_owned(),
        Blob(b) => format!("<{} bytes>", b.len()),
        other => format!("{:?}", duckdb::types::Value::from(other)),
    })
}

// ---------------------------------------------------------------- postgres

pub struct Postgres {
    client: postgres::Client,
}

impl Postgres {
    pub fn open(settings: &Settings) -> Result<Self> {
        let mut config = postgres::Config::new();
        config.host(settings.need("host")?);
        config.port(settings.get("port").and_then(|p| p.parse().ok()).unwrap_or(5432));
        config.dbname(settings.need("database")?);
        config.user(settings.need("username")?);
        if let Some(p) = settings.get("password") {
            config.password(p);
        }
        if let Some(schema) = settings.get("schema") {
            config.options(&format!("-c search_path={schema}"));
        }
        let client = config.connect(postgres::NoTls).map_err(|e| WarehouseError::Connect(e.to_string()))?;
        Ok(Postgres { client })
    }
}

impl Warehouse for Postgres {
    fn query(&mut self, sql: &str) -> Result<Vec<Row>> {
        let messages = self.client.simple_query(sql).map_err(|e| WarehouseError::Query(e.to_string()))?;
        let mut out = Vec::new();
        for m in messages {
            if let postgres::SimpleQueryMessage::Row(row) = m {
                out.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_rows_come_back_as_text() {
        let mut db = DuckDb::in_memory().unwrap();
        db.execute("CREATE TABLE t(c VARCHAR, n INTEGER); INSERT INTO t VALUES ('a', 1), ('b', NULL), ('a', 3)").unwrap();
        let rows = db.query("SELECT c, count(*) AS n FROM t GROUP BY c ORDER BY n DESC").unwrap();
        assert_eq!(rows, vec![vec![Some("a".to_string()), Some("2".to_string())], vec![Some("b".to_string()), Some("1".to_string())]]);
        let rows = db.query("SELECT n FROM t WHERE c = 'b'").unwrap();
        assert_eq!(rows, vec![vec![None]]);
    }

    #[test]
    fn settings_merge_sources_in_order() {
        let dir = std::env::temp_dir().join(format!("zsql-wh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("datasources.yml"), "pg:\n  adapter: postgres\n  host: db\n  port: 5433\n").unwrap();
        std::fs::write(dir.join(".strata"), "api_key: x\npg:\n  password: secret\n").unwrap();
        let s = settings_for(&dir, "pg").unwrap();
        assert_eq!(s.adapter, "postgres");
        assert_eq!(s.get("host"), Some("db"));
        assert_eq!(s.get("password"), Some("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
