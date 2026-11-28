//! The parts of reaching a datasource that can be tested without one: the
//! connection a project declares, where a secret comes from, and what
//! counts as a read. The transports themselves need a server, and phase
//! B2's containers are where they get one.

use super::*;
use crate::catalog::{self, Column, ForeignKey, Table};
use std::path::{Path, PathBuf};

/// A sink that keeps what it is handed, and can be told to have had
/// enough after so many batches.
#[derive(Default)]
struct Kept {
    schema: Option<arrow_schema::SchemaRef>,
    batches: Vec<arrow_array::RecordBatch>,
    stop_after: Option<usize>,
    /// What a statement that returned no rows said it changed.
    affected: Option<u64>,
}

impl Sink for Kept {
    fn schema(&mut self, schema: &arrow_schema::SchemaRef) -> Result<()> {
        assert!(self.schema.is_none() && self.batches.is_empty(), "the schema comes once, and first");
        self.schema = Some(schema.clone());
        Ok(())
    }

    fn batch(&mut self, batch: arrow_array::RecordBatch) -> Result<()> {
        assert_eq!(Some(&batch.schema()), self.schema.as_ref(), "every batch is of the schema that was announced");
        assert!(batch.num_rows() > 0, "an empty batch is not sent");
        if self.stop_after == Some(self.batches.len()) {
            return Err(Error::Query("sink".into(), "enough".into()));
        }
        self.batches.push(batch);
        Ok(())
    }

    fn affected(&mut self, rows: u64) -> Result<()> {
        self.affected = Some(rows);
        Ok(())
    }
}

/// A streamed read says exactly what the capped read says, however it
/// is cut into batches. Returns what was streamed.
fn streams_as_it_queries(src: &mut dyn Source, sql: &str) -> Batches {
    let asked = src.query(sql, 1_000_000).expect("the capped read");
    let mut kept = Kept::default();
    src.stream(sql, &mut kept).expect("the streamed read");
    let streamed = Batches { schema: kept.schema.expect("a schema, rows or none"), batches: kept.batches };
    let text = streamed.to_text().unwrap();
    assert_eq!(text.columns, asked.columns, "{sql}");
    assert_eq!(text.rows, asked.rows, "{sql}");
    streamed
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zsql-datasource-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_connection_is_the_keys_the_model_reads_past() {
    let dir = temp("conn");
    std::fs::write(
        dir.join("datasources.yml"),
        "warehouse:\n  name: Warehouse\n  adapter: Postgres\n  tier: hot\n  host: db.internal\n  port: 5433\n  \
         database: analytics\n  schema: public\n  username: analyst\n  query_timeout: 30\nlocal:\n  adapter: duckdb\n  \
         file: ./data/a.duckdb\n",
    )
    .unwrap();

    let all = Connection::read_all(&dir).unwrap();
    assert_eq!(all.len(), 2, "both datasources are read, in file order");

    let wh = Connection::read(&dir, "WAREHOUSE").unwrap();
    assert_eq!(wh.uid, "warehouse", "the key is matched case-insensitively");
    assert_eq!(wh.adapter, "postgres", "the adapter is lower-cased, as the dialect catalog spells it");
    assert_eq!(wh.host.as_deref(), Some("db.internal"));
    assert_eq!(wh.port, Some(5433));
    assert_eq!(wh.database.as_deref(), Some("analytics"));
    assert_eq!(wh.schema.as_deref(), Some("public"));
    assert_eq!(wh.username.as_deref(), Some("analyst"));
    assert_eq!(wh.timeout(), std::time::Duration::from_secs(30));
    assert_eq!(wh.file, None);

    let local = Connection::read(&dir, "local").unwrap();
    assert_eq!(local.file, Some(PathBuf::from("./data/a.duckdb")));
    assert_eq!(local.timeout(), std::time::Duration::from_secs(60), "no query_timeout gives the default");

    let missing = Connection::read(&dir, "nope").unwrap_err().to_string();
    assert!(missing.contains("no datasource nope"), "{missing}");

    // A datasource with nothing but an adapter is still a datasource; what
    // it lacks is reported when something tries to open it, naming the key.
    std::fs::write(dir.join("datasources.yml"), "bare:\n  adapter: postgres\n").unwrap();
    let bare = Connection::read(&dir, "bare").unwrap();
    let err = postgres::Postgres::open(&bare, Some(Secret("x".into()))).err().unwrap().to_string();
    assert!(err.contains("has no host"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_adapter_is_named_rather_than_guessed() {
    let conn = Connection { uid: "wh".into(), adapter: "vertica".into(), ..Default::default() };
    let err = open(&conn, Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("vertica is not an adapter this build can reach"), "{err}");
}

#[test]
fn the_environment_variable_follows_the_template() {
    assert_eq!(secret_env_var("warehouse", "password"), "ZSQL_DS_WAREHOUSE_PASSWORD");
    assert_eq!(secret_env_var("my-dw.eu", "token"), "ZSQL_DS_MY_DW_EU_TOKEN");
}

/// Every assertion about where a secret comes from, in one test: the
/// environment is process-wide, so splitting these would let them race.
#[test]
fn a_secret_comes_from_the_environment_first_and_the_gitignored_files_after() {
    let dir = temp("secret");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Point ZSQL_HOME at an empty directory, or this test would read the
    // real one and pass or fail by what happens to be on the machine.
    std::env::set_var("ZSQL_HOME", &home);
    let var = secret_env_var("wh", "password");
    std::env::remove_var(&var);

    assert!(secret(Some(&dir), "wh", "password").is_none(), "nothing is invented");

    // Nested under the datasource key, as the datasources.yml template says.
    std::fs::write(dir.join(".zsql"), "api_key: zsk_x\nwh:\n  password: from-nested\n").unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-nested");

    // Flat, for one secret and no nesting.
    std::fs::write(dir.join(".zsql"), "wh_password: from-flat\n").unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-flat");

    // The account-wide file when the project has nothing.
    std::fs::write(dir.join(".zsql"), "api_key: zsk_x\n").unwrap();
    std::fs::write(home.join("config"), "wh:\n  password: from-home\n").unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-home");

    // The project's own file wins over the account-wide one.
    std::fs::write(dir.join(".zsql"), "wh_password: from-project\n").unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-project");

    // And the environment wins over both, so a shell can override a
    // checked-out project without editing it.
    std::env::set_var(&var, "from-env");
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-env");
    // A blank variable means unset, as it does for every other ZSQL_
    // variable, so it falls through rather than denying the file's answer.
    std::env::set_var(&var, "   ");
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-project");
    std::env::remove_var(&var);

    // `.strata` is the older CLI's file and is read only when `.zsql` is absent.
    std::fs::write(dir.join(".strata"), "wh_password: from-strata\n").unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-project", ".zsql wins while it exists");
    std::fs::remove_file(dir.join(".zsql")).unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "from-strata");

    // A secret written in datasources.yml itself is read, last of all.
    std::fs::remove_file(dir.join(".strata")).unwrap();
    std::fs::remove_file(home.join("config")).unwrap();
    std::fs::write(
        dir.join("datasources.yml"),
        "Wh:\n  adapter: bigquery\n  project_id: written\n  username: written\n  password: 123456\n  key_file: ./sa.json\n",
    )
    .unwrap();
    assert_eq!(secret(Some(&dir), "wh", "password").unwrap().expose(), "123456", "digits are still a password");
    let written = Connection::read(&dir, "wh").unwrap();
    assert_eq!(
        (written.project.as_deref(), written.username.as_deref(), written.client_email.as_deref()),
        (Some("written"), Some("written"), None)
    );

    // And .zsql goes over that file, setting by setting: a secret, a
    // setting that is no secret, and one spelled another way. What it
    // does not say stays as written; the adapter is never its to say.
    std::fs::write(
        dir.join(".zsql"),
        "wh:\n  password: hidden\n  project: hidden\n  client_email: sa@p.iam.gserviceaccount.com\n  adapter: duckdb\n  auth_mode: kp\nwh_username: flat\nwh_east_username: another-datasource\n",
    )
    .unwrap();
    let c = Connection::read(&dir, "wh").unwrap();
    assert_eq!((c.project.as_deref(), c.username.as_deref()), (Some("hidden"), Some("flat")));
    assert_eq!((c.client_email.as_deref(), c.adapter.as_str()), (Some("sa@p.iam.gserviceaccount.com"), "bigquery"));
    assert_eq!(c.key_file.as_deref(), Some(Path::new("./sa.json")));
    assert_eq!(secrets(Some(&dir), "wh").password.unwrap().expose(), "hidden");
    let held = local_entry(Some(&dir), "wh");
    assert!(held.get("auth_mode").is_none() && held.get("adapter").is_none(), "only what a connection reads: {held:?}");

    // A value is read as the setting it is: a port in quotes is the
    // port, digits are a username, and what cannot be the setting
    // leaves the written one alone.
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: postgres\n  port: 5432\n  username: written\n  query_timeout: 30\n")
        .unwrap();
    std::fs::write(dir.join(".zsql"), "wh:\n  port: \"5433\"\n  query_timeout: soon\nwh_username: 12345\n").unwrap();
    let c = Connection::read(&dir, "wh").unwrap();
    assert_eq!((c.port, c.username.as_deref(), c.query_timeout), (Some(5433), Some("12345"), Some(30)));

    // The nearer file decides a setting however the farther one spells it.
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: bigquery\n").unwrap();
    std::fs::write(dir.join(".zsql"), "wh_project_id: mine\nwh:\n  dataset: d\n  schema: s\n").unwrap();
    std::fs::write(home.join("config"), "wh:\n  project: shared\n  location: EU\n").unwrap();
    let c = Connection::read(&dir, "wh").unwrap();
    assert_eq!((c.project.as_deref(), c.location.as_deref(), c.schema.as_deref()), (Some("mine"), Some("EU"), Some("s")));

    std::env::remove_var("ZSQL_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_secret_does_not_print_itself() {
    let s = Secret("hunter2".into());
    assert_eq!(format!("{s:?}"), "Secret(…)");
    assert!(!format!("{s:?}").contains("hunter2"));
}

#[test]
fn only_a_read_is_allowed_through() {
    for ok in [
        "SELECT 1",
        "select * from t",
        "  \n SELECT 1 ;  ",
        "WITH a AS (SELECT 1) SELECT * FROM a",
        "SHOW TABLES",
        "PRAGMA table_info(t)",
        "DESCRIBE t",
        "-- a comment first\nSELECT 1",
        "/* a block\n comment */ SELECT 1",
        // A semicolon a value holds is not a second statement.
        "SELECT 'a; b' FROM t",
        "SELECT * FROM t WHERE note = 'drop table x;'",
        "SELECT \"odd;column\" FROM t",
    ] {
        assert!(read_only(ok).is_ok(), "should be a read: {ok}");
    }
    for bad in [
        "DELETE FROM t",
        "UPDATE t SET a = 1",
        "INSERT INTO t VALUES (1)",
        "DROP TABLE t",
        "TRUNCATE t",
        "CREATE TABLE t (a int)",
        "GRANT ALL ON t TO public",
        "COPY t FROM '/etc/passwd'",
        "SELECT 1; DROP TABLE t",
        "SELECT 1; DROP TABLE t;",
        // A comment must not be able to hide what comes after it.
        "-- SELECT 1\nDROP TABLE t",
        "/* SELECT 1 */ DROP TABLE t",
        "SELECT 1 -- ;\n; DROP TABLE t",
        "",
        "   ",
        ";",
    ] {
        let err = read_only(bad).unwrap_err().to_string();
        assert!(err.starts_with("this statement is not a read"), "should be refused: {bad} ({err})");
    }
    // A read that carries a write is a write.
    for sql in [
        "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
        "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
        "SELECT * INTO new_t FROM t",
        "SELECT * FROM t; DROP TABLE t",
        "WITH x AS (SELECT 1) UPDATE t SET a = 1",
    ] {
        assert!(read_only(sql).is_err(), "{sql}");
    }
    // The same words inside a literal or a quoted identifier are not syntax.
    assert!(read_only("SELECT 'insert into' AS note, \"update\" FROM t WHERE kind = 'delete'").is_ok());
    assert!(read_only("SELECT update_count, deleted_at FROM t").is_ok(), "a word with an underscore is another word");
}

/// The transport against a real server. Ignored, because it needs one:
/// `ZSQL_TEST_PG=host=… user=… password=… dbname=…` style settings as
/// separate variables, then `cargo test -p zsql-datasource -- --ignored`.
/// Phase B2 replaces this with a container CI runs for itself.
#[test]
#[ignore]
fn a_real_postgres_answers_a_read_and_refuses_a_write() {
    let conn = Connection {
        uid: "test".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).or(Some(5466)),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD");
    let secret = Secret(password.clone());
    let mut src = postgres::Postgres::open(&conn, Some(Secret(password))).expect("connects");

    let rows = src
        .query("SELECT 1 AS one, 'two' AS two, NULL AS three, true AS four, 1.50::numeric AS five, DATE '2026-01-02' AS six", 10)
        .expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three", "four", "five", "six"]);
    assert_eq!(rows.rows.len(), 1);
    // Every type as the server's own text, not only the text ones: the
    // extended protocol's binary values once came back as null here.
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert_eq!(rows.get(&rows.rows[0], "four"), Some("t"));
    assert_eq!(rows.get(&rows.rows[0], "five"), Some("1.50"));
    assert_eq!(rows.get(&rows.rows[0], "six"), Some("2026-01-02"));

    assert!(src.query("DROP TABLE IF EXISTS zsql_should_not_exist", 1).is_err(), "a write is refused before it is sent");

    let err = src.query("SELECT * FROM generate_series(1, 100)", 10).unwrap_err().to_string();
    assert!(err.contains("more than the 10 rows"), "the cap refuses rather than truncates: {err}");
    // Read as it arrives: the same rows as the capped read, in more
    // than one batch, the columns even when nothing is under them, and a
    // read that was stopped early leaves a connection that still answers.
    crate::rows::batch_rows_for_tests(2);
    assert!(streams_as_it_queries(&mut src, "SELECT n, n::text AS t FROM generate_series(1, 7) AS n ORDER BY n").batches.len() > 1);
    assert_eq!(streams_as_it_queries(&mut src, "SELECT n FROM generate_series(1, 7) AS n WHERE n > 99").num_rows(), 0);
    let mut full = Kept { stop_after: Some(1), ..Default::default() };
    assert!(src
        .stream("SELECT n, n::text AS t FROM generate_series(1, 7) AS n ORDER BY n", &mut full)
        .unwrap_err()
        .to_string()
        .contains("enough"));
    assert_eq!(src.query("SELECT 1 AS one", 10).unwrap().rows.len(), 1, "after a read stopped early");
    // Trusted by whoever opened it, a write is a statement like any
    // other, and says how many rows it changed.
    let mut trusted =
        postgres::Postgres::open(&Connection { access: Access::Unchecked, ..conn.clone() }, Some(secret.clone())).expect("connects");
    trusted.query("CREATE TEMP TABLE zsql_w (id int, note text)", 10).expect("a write, unchecked");
    let mut wrote = Kept::default();
    trusted.stream("INSERT INTO zsql_w VALUES (1, 'a'), (2, 'b'), (3, 'c')", &mut wrote).unwrap();
    assert_eq!((wrote.affected, wrote.batches.len()), (Some(3), 0));
    let mut changed = Kept::default();
    trusted.stream("UPDATE zsql_w SET note = 'x' WHERE id > 1", &mut changed).unwrap();
    assert_eq!(changed.affected, Some(2));
    assert_eq!(trusted.query("SELECT count(*) FROM zsql_w", 10).unwrap().rows[0][0].as_deref(), Some("3"));
}

/// SQLite needs no server, so this is the one transport the ordinary test
/// run proves end to end: a read, a null, the row cap, and a write refused
/// by the file's own read-only flag before the guard is even consulted.
#[test]
fn sqlite_reads_its_file_and_cannot_write_it() {
    let dir = temp("sqlite");
    let file = dir.join("t.db");
    {
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, note TEXT); INSERT INTO t VALUES (1, 'a', NULL), (2, 'b', 'x'), (3, 'c', 'y');")
            .unwrap();
    }
    let conn = Connection { uid: "local".into(), adapter: "sqlite".into(), file: Some(file.clone()), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).expect("opens");
    assert_eq!(src.adapter(), "sqlite");

    let rows = src.query("SELECT id, name, note FROM t ORDER BY id", 10).unwrap();
    assert_eq!(rows.columns, ["id", "name", "note"]);
    assert_eq!(rows.rows.len(), 3);

    // The same read streamed: the same rows, a batch at a time, the
    // columns announced even when there is nothing under them, and a
    // sink that has had enough ends the read with its own error.
    crate::rows::batch_rows_for_tests(2);
    let streamed = streams_as_it_queries(src.as_mut(), "SELECT id, name, note FROM t ORDER BY id");
    assert_eq!(streamed.batches.iter().map(|b| b.num_rows()).collect::<Vec<_>>(), [2, 1], "three rows, two to a batch");
    let none = streams_as_it_queries(src.as_mut(), "SELECT id, name FROM t WHERE id > 99");
    assert_eq!((none.columns(), none.num_rows()), (vec!["id".to_string(), "name".to_string()], 0));
    let mut full = Kept { stop_after: Some(1), ..Default::default() };
    let err = src.stream("SELECT id FROM t ORDER BY id", &mut full).unwrap_err();
    assert!(err.to_string().contains("enough"), "{err}");
    assert_eq!(full.batches.len(), 1, "what arrived before it stopped is what it has");
    assert_eq!(src.query("SELECT count(*) FROM t", 10).unwrap().rows[0][0].as_deref(), Some("3"), "and the connection is still good");

    // Opened by a caller that checks nothing, what may be done is the
    // database's to say: a write is a statement like any other.
    let unchecked = Connection { access: Access::Unchecked, ..conn.clone() };
    let mut trusted = open(&unchecked, Secrets::default()).unwrap();
    trusted.query("INSERT INTO t VALUES (4, 'd', NULL)", 10).expect("a write, unchecked");
    assert_eq!(trusted.query("SELECT count(*) FROM t", 10).unwrap().rows[0][0].as_deref(), Some("4"));
    // A statement that returns no rows says how many it changed, and
    // has a schema all the same: no columns.
    let mut wrote = Kept::default();
    trusted.stream("UPDATE t SET note = 'seen' WHERE id > 1", &mut wrote).unwrap();
    assert_eq!((wrote.affected, wrote.batches.len(), wrote.schema.map(|s| s.fields().len())), (Some(3), 0, Some(0)));
    let mut read = Kept::default();
    trusted.stream("SELECT id FROM t", &mut read).unwrap();
    assert_eq!(read.affected, None, "a read changed nothing, and says nothing about it");
    assert!(matches!(src.query("INSERT INTO t VALUES (5, 'e', NULL)", 10), Err(Error::NotARead(_))), "checked, it is still refused");
    assert_eq!(rows.get(&rows.rows[0], "name"), Some("a"));
    assert_eq!(rows.get(&rows.rows[0], "note"), None, "null is none, not the word null");
    assert_eq!(rows.get(&rows.rows[1], "id"), Some("2"), "numbers come back as text");

    let err = src.query("SELECT id FROM t", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "the cap refuses rather than trims: {err}");

    // Refused by the guard, and if it ever were not, by the file.
    assert!(src.query("DELETE FROM t", 1).is_err());
    let missing = Connection { uid: "none".into(), adapter: "sqlite".into(), ..Default::default() };
    let err = open(&missing, Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("has no file"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The HTTP transport against a ClickHouse the environment names:
/// `ZSQL_TEST_CLICKHOUSE_URL` (and `_USER`, `_PASSWORD` when it wants
/// them). Ignored otherwise. The public playground at
/// `https://play.clickhouse.com` with user `play` is enough to run it by
/// hand; CI points it at a container.
#[test]
#[ignore]
fn a_real_clickhouse_answers_over_http_and_refuses_a_write_itself() {
    let conn = Connection {
        uid: "ch".into(),
        adapter: "clickhouse".into(),
        url: Some(std::env::var("ZSQL_TEST_CLICKHOUSE_URL").expect("ZSQL_TEST_CLICKHOUSE_URL")),
        username: std::env::var("ZSQL_TEST_CLICKHOUSE_USER").ok(),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let secret = std::env::var("ZSQL_TEST_CLICKHOUSE_PASSWORD").ok().map(Secret);
    let mut src = open(&conn, Secrets::password(secret)).expect("opens");

    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three, 1.5 AS four", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three", "four"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert_eq!(rows.get(&rows.rows[0], "four"), Some("1.5"));

    // Past the cap the server throws, which is what result_overflow_mode asks.
    let err = src.query("SELECT number FROM system.numbers LIMIT 100", 10).err().unwrap().to_string();
    assert!(err.contains("ch"), "named: {err}");
    // Read as it arrives, a line a row: the same rows as the capped
    // read, the columns even when nothing is under them, and an error
    // the server raises is the read's.
    crate::rows::batch_rows_for_tests(2);
    assert!(
        streams_as_it_queries(src.as_mut(), "SELECT number, toString(number) AS s, NULL AS z FROM system.numbers LIMIT 7").batches.len()
            > 1
    );
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT number FROM system.numbers WHERE number > 5 LIMIT 0").num_rows(), 0);
    let err = src.stream("SELECT no_such_column FROM system.numbers LIMIT 1", &mut Kept::default()).unwrap_err().to_string();
    assert!(err.contains("no_such_column"), "{err}");

    // The guard refuses first; the server's readonly mode would refuse second.
    assert!(src.query("CREATE TABLE zsql_no (a UInt8) ENGINE = Memory", 1).is_err());
}

#[test]
fn a_sqlite_catalog_has_its_tables_views_and_columns_in_order() {
    let dir = temp("cat-sqlite");
    let file = dir.join("c.db");
    {
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, placed TEXT NOT NULL, note TEXT);\
             CREATE VIEW recent AS SELECT id FROM orders;",
        )
        .unwrap();
    }
    let conn = Connection { uid: "local".into(), adapter: "sqlite".into(), file: Some(file), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let cat = Catalog::read(src.as_mut(), "local", None).unwrap();

    assert_eq!(cat.datasource, "local");
    assert_eq!(cat.adapter, "sqlite");
    assert!(cat.read_at.ends_with('Z') && cat.read_at.len() == 20, "RFC 3339 to the second: {}", cat.read_at);
    let names: Vec<(&str, &str)> = cat.tables.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect();
    assert_eq!(names, [("orders", "table"), ("recent", "view")]);

    let orders = cat.table(None, "orders").unwrap();
    let cols: Vec<(&str, &str, bool, i64)> =
        orders.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable, c.position)).collect();
    assert_eq!(cols, [("id", "INTEGER", true, 1), ("placed", "TEXT", false, 2), ("note", "TEXT", true, 3)]);
    assert_eq!(orders.physical_name(), "orders", "no schema, so no qualifier");
    assert!(orders.rows.is_none() && orders.comment.is_none(), "SQLite keeps neither");

    let json = serde_json::to_string(&cat).unwrap();
    let back: Catalog = serde_json::from_str(&json).unwrap();
    assert_eq!(back.tables.len(), 2, "the file round-trips");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Postgres catalog, scoped to `pg_catalog` because that schema is
/// in every server and needs no fixture written: its tables have a row
/// estimate, typed columns in order, and the odd comment.
#[test]
#[ignore]
fn a_real_postgres_catalog_reads_pg_catalog() {
    let conn = Connection {
        uid: "test".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).or(Some(5466)),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD");
    let mut src = open(&conn, Secrets::password(Some(Secret(password)))).expect("connects");
    let cat = Catalog::read(src.as_mut(), "test", Some("pg_catalog")).expect("reads");
    pages_agree(src.as_mut(), &cat, Some("pg_catalog"), 40);
    a_search_means_its_underscores(src.as_mut(), Some("pg_catalog"), "G_CLA", "pg_class", "p__class");

    let class = cat.table(Some("pg_catalog"), "pg_class").expect("pg_class is in every server");
    assert_eq!(class.kind, "table");
    assert_eq!(class.physical_name(), "pg_catalog.pg_class");
    assert!(class.rows.is_some(), "reltuples gives an estimate");
    let relname = class.columns.iter().find(|c| c.name == "relname").expect("relname");
    assert_eq!(relname.data_type, "name");
    assert!(!relname.nullable);
    assert!(class.columns.windows(2).all(|w| w[0].position < w[1].position), "columns in ordinal order");
    assert!(cat.tables.iter().any(|t| t.kind == "view"), "pg_catalog has views too");
    assert!(cat.tables.iter().all(|t| t.schema.as_deref() == Some("pg_catalog")), "the scope held");
}

/// The ClickHouse catalog, scoped to the playground's `blogs` database
/// by default: real tables with row counts from `system.tables`, and
/// ClickHouse's own type spellings kept as they are.
#[test]
#[ignore]
fn a_real_clickhouse_catalog_reads_a_database() {
    let conn = Connection {
        uid: "ch".into(),
        adapter: "clickhouse".into(),
        url: Some(std::env::var("ZSQL_TEST_CLICKHOUSE_URL").expect("ZSQL_TEST_CLICKHOUSE_URL")),
        username: std::env::var("ZSQL_TEST_CLICKHOUSE_USER").ok(),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let secret = std::env::var("ZSQL_TEST_CLICKHOUSE_PASSWORD").ok().map(Secret);
    let mut src = open(&conn, Secrets::password(secret)).expect("opens");
    // The playground has `blogs`; a container has `default`, empty but
    // real. Either way the read works and the scope holds.
    let scope = std::env::var("ZSQL_TEST_CLICKHOUSE_DB").unwrap_or_else(|_| "blogs".into());
    let cat = Catalog::read(src.as_mut(), "ch", Some(&scope)).expect("reads");
    pages_agree(src.as_mut(), &cat, Some(&scope), 2);
    // `system` is never empty, so the paging clause is proven even when
    // the scope above holds nothing.
    let system = Catalog::list(src.as_mut(), &Filter::schema(Some("system")), 3, 1).expect("a page of system");
    assert!(system.tables.len() == 3 && system.more, "{:?}", system.tables.len());
    a_search_means_its_underscores(src.as_mut(), Some("system"), "A_TYPE_F", "data_type_families", "dat__type_families");
    assert_eq!(cat.adapter, "clickhouse");
    assert!(cat.tables.iter().all(|t| t.schema.as_deref() == Some(scope.as_str())), "the scope held");
    if let Some(countries) = cat.table(Some("blogs"), "countries") {
        assert_eq!(countries.kind, "table");
        assert!(countries.rows.unwrap_or(0) > 0, "total_rows is read");
        assert_eq!(countries.physical_name(), "blogs.countries");
        let name = countries.columns.iter().find(|c| c.name == "name").expect("name");
        assert_eq!(name.data_type, "String", "ClickHouse's spelling, kept");
        assert!(!name.nullable);
        assert_eq!(name.position, 1);
    }
    // Nothing from the system schemas leaks in when there is no scope.
    let all = Catalog::read(src.as_mut(), "ch", None).expect("reads");
    assert!(all
        .tables
        .iter()
        .all(|t| !["system", "INFORMATION_SCHEMA", "information_schema"].contains(&t.schema.as_deref().unwrap_or(""))));
}

/// MySQL against a server the environment names, as CI's container does:
/// `ZSQL_TEST_MYSQL_HOST`, `_USER`, `_PASSWORD`, `_DB`.
#[test]
#[ignore]
fn a_real_mysql_answers_and_its_catalog_reads() {
    let conn = Connection {
        uid: "my".into(),
        adapter: "mysql".into(),
        host: std::env::var("ZSQL_TEST_MYSQL_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_MYSQL_PORT").ok().and_then(|p| p.parse().ok()),
        database: std::env::var("ZSQL_TEST_MYSQL_DB").ok().or(Some("zsql".into())),
        username: std::env::var("ZSQL_TEST_MYSQL_USER").ok().or(Some("root".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_MYSQL_PASSWORD").expect("ZSQL_TEST_MYSQL_PASSWORD");
    let mut src = open(&conn, Secrets::password(Some(Secret(password)))).expect("connects");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three, 1.5 AS four", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three", "four"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert!(src.query("CREATE TABLE zsql_no (a INT)", 1).is_err());
    let err = src.query("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    // Read as it arrives: the same rows as the capped read, in more
    // than one batch, the columns even when nothing is under them, and a
    // read that was stopped early leaves a connection that still answers.
    crate::rows::batch_rows_for_tests(2);
    assert!(
        streams_as_it_queries(src.as_mut(), "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5")
            .batches
            .len()
            > 1
    );
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT 1 AS n FROM DUAL WHERE 1 = 0").num_rows(), 0);
    let mut full = Kept { stop_after: Some(1), ..Default::default() };
    assert!(src
        .stream("SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5", &mut full)
        .unwrap_err()
        .to_string()
        .contains("enough"));
    assert_eq!(src.query("SELECT 1 AS one", 10).unwrap().rows.len(), 1, "after a read stopped early");
    // The container makes itself a self-signed certificate, which is
    // exactly what `require` accepts and `verify-full` refuses.
    let tls = |mode: SslMode| Connection { sslmode: mode, ..conn.clone() };
    let pw = || Some(Secret(std::env::var("ZSQL_TEST_MYSQL_PASSWORD").unwrap()));
    assert!(open(&tls(SslMode::Require), Secrets::password(pw())).is_ok(), "require accepts a certificate signed by nobody");
    assert!(open(&tls(SslMode::VerifyFull), Secrets::password(pw())).is_err(), "verify-full refuses a self-signed certificate");
    let p = open(&tls(SslMode::Prefer), Secrets::password(pw())).expect("prefer connects");
    assert!(p.warnings().is_empty(), "TLS was on offer, so nothing to warn about: {:?}", p.warnings());
    // The catalog of mysql's own schema: always there, never written to.
    let cat = Catalog::read(src.as_mut(), "my", Some("mysql")).expect("reads");
    pages_agree(src.as_mut(), &cat, Some("mysql"), 15);
    a_search_means_its_underscores(src.as_mut(), Some("mysql"), "E_ZONE_N", "time_zone_name", "tim__zone_name");
    let user = cat.table(Some("mysql"), "user").expect("mysql.user is in every server");
    assert!(user.columns.iter().any(|c| c.name == "Host"), "typed columns");
    assert!(user.columns.windows(2).all(|w| w[0].position < w[1].position));
    assert!(user.columns.iter().any(|c| c.max_length.is_some()), "character_maximum_length is read");
    assert_eq!(user.primary_key, ["Host", "User"], "a composite key, in order");
    // The container's shop schema, from harness/sources/mysql-init.sql.
    let shop = Catalog::read(src.as_mut(), "my", Some("zsql")).expect("reads zsql");
    let customers = shop.table(Some("zsql"), "customers").expect("customers");
    assert_eq!(customers.comment.as_deref(), Some("Who buys"));
    assert_eq!(customers.columns.iter().find(|c| c.name == "name").unwrap().comment.as_deref(), Some("As they sign"));
    let orders = shop.table(Some("zsql"), "orders").expect("orders");
    assert_eq!(orders.primary_key, ["id"]);
    assert_eq!(
        orders.foreign_keys,
        [ForeignKey {
            name: Some("orders_customer".into()),
            columns: vec!["customer_id".into()],
            references_schema: Some("zsql".into()),
            references_table: "customers".into(),
            references_columns: vec!["id".into()],
        }]
    );
}

/// SQL Server against a server the environment names: `ZSQL_TEST_MSSQL_HOST`,
/// `_USER`, `_PASSWORD`, `_DB`. Encryption off, as the transport is built.
#[test]
#[ignore]
fn a_real_sqlserver_answers_and_its_catalog_reads() {
    let conn = Connection {
        uid: "ms".into(),
        adapter: "sqlserver".into(),
        host: std::env::var("ZSQL_TEST_MSSQL_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_MSSQL_PORT").ok().and_then(|p| p.parse().ok()),
        database: std::env::var("ZSQL_TEST_MSSQL_DB").ok().or(Some("master".into())),
        username: std::env::var("ZSQL_TEST_MSSQL_USER").ok().or(Some("sa".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_MSSQL_PASSWORD").expect("ZSQL_TEST_MSSQL_PASSWORD");
    let mut src = open(&conn, Secrets::password(Some(Secret(password)))).expect("connects");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert!(src.query("CREATE TABLE zsql_no (a INT)", 1).is_err());
    // TOP, since this is the dialect with no LIMIT, and the cap still refuses.
    let err = src.query("SELECT TOP (5) object_id FROM sys.objects", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    // Read as it arrives: the same rows as the capped read, in more
    // than one batch, the columns even when nothing is under them, and a
    // read that was stopped early leaves a connection that still answers.
    crate::rows::batch_rows_for_tests(2);
    assert!(streams_as_it_queries(src.as_mut(), "SELECT TOP (5) object_id FROM sys.objects ORDER BY object_id").batches.len() > 1);
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT TOP (5) object_id FROM sys.objects WHERE 1 = 0").num_rows(), 0);
    let mut full = Kept { stop_after: Some(1), ..Default::default() };
    assert!(src
        .stream("SELECT TOP (5) object_id FROM sys.objects ORDER BY object_id", &mut full)
        .unwrap_err()
        .to_string()
        .contains("enough"));
    assert_eq!(src.query("SELECT 1 AS one", 10).unwrap().rows.len(), 1, "after a read stopped early");
    // SQL Server also makes itself a self-signed certificate.
    let tls = |mode: SslMode| Connection { sslmode: mode, ..conn.clone() };
    let pw = || Some(Secret(std::env::var("ZSQL_TEST_MSSQL_PASSWORD").unwrap()));
    assert!(open(&tls(SslMode::Require), Secrets::password(pw())).is_ok(), "require accepts a certificate signed by nobody");
    assert!(open(&tls(SslMode::VerifyFull), Secrets::password(pw())).is_err(), "verify-full refuses a self-signed certificate");
    let p = open(&tls(SslMode::Prefer), Secrets::password(pw())).expect("prefer connects");
    assert!(p.warnings().is_empty(), "TLS was on offer, so nothing to warn about: {:?}", p.warnings());
    // sys.objects lists user objects, so the catalog views themselves
    // are not in it, which is right for a model. master.dbo always holds
    // the spt_ tables setup leaves behind, and spt_values is a view.
    let cat = Catalog::read(src.as_mut(), "ms", Some("dbo")).expect("reads");
    pages_agree(src.as_mut(), &cat, Some("dbo"), 2);
    a_search_means_its_underscores(src.as_mut(), Some("dbo"), "T_MON", "spt_monitor", "sp__monitor");
    let monitor = cat.table(Some("dbo"), "spt_monitor").expect("master.dbo.spt_monitor is on every server");
    assert_eq!(monitor.kind, "table");
    assert_eq!(monitor.physical_name(), "dbo.spt_monitor");
    assert!(monitor.columns.iter().any(|c| c.name == "lastrun" && c.data_type == "datetime"), "{:?}", monitor.columns);
    assert!(monitor.columns.windows(2).all(|w| w[0].position < w[1].position));
    assert_eq!(cat.table(Some("dbo"), "spt_values").map(|t| t.kind.as_str()), Some("view"));
}

/// Trino against a coordinator the environment names: `ZSQL_TEST_TRINO_URL`,
/// and the catalog as `_DB`, `tpch` by default since the image ships it.
#[test]
#[ignore]
fn a_real_trino_answers_follows_next_uri_and_its_catalog_reads() {
    let conn = Connection {
        uid: "tr".into(),
        adapter: "trino".into(),
        url: Some(std::env::var("ZSQL_TEST_TRINO_URL").expect("ZSQL_TEST_TRINO_URL")),
        database: std::env::var("ZSQL_TEST_TRINO_DB").ok().or(Some("tpch".into())),
        username: Some("zsql".into()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut src = open(&conn, Secrets::default()).expect("opens");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    // tiny.nation is 25 rows and arrives across pages; the cap cancels and refuses.
    let err = src.query("SELECT name FROM tpch.tiny.nation", 10).err().unwrap().to_string();
    assert!(err.contains("more than the 10 rows"), "{err}");
    let all = src.query("SELECT name FROM tpch.tiny.nation", 100).expect("reads across pages");
    crate::rows::batch_rows_for_tests(2);
    assert_eq!(
        streams_as_it_queries(src.as_mut(), "SELECT name FROM tpch.tiny.nation ORDER BY name").num_rows(),
        25,
        "streamed across pages"
    );
    streams_as_it_queries(src.as_mut(), "SELECT name FROM tpch.tiny.nation WHERE nationkey < 0");
    assert_eq!(all.rows.len(), 25);
    assert!(src.query("CREATE TABLE memory.default.zsql_no (a INT)", 1).is_err());
    // The catalog is a header, so information_schema alone means tpch's.
    let cat = Catalog::read(src.as_mut(), "tr", Some("tiny")).expect("reads");
    pages_agree(src.as_mut(), &cat, Some("tiny"), 3);
    // tpch has no underscore in a table name, so the hit is plain and
    // the miss is what proves `_` is not read as any character.
    a_search_means_its_underscores(src.as_mut(), Some("tiny"), "ATIO", "nation", "n_tion");
    let nation = cat.table(Some("tiny"), "nation").expect("tpch.tiny.nation");
    assert_eq!(nation.kind, "table");
    let name = nation.columns.iter().find(|c| c.name == "name").expect("name");
    assert!(name.data_type.starts_with("varchar"), "{}", name.data_type);
    assert!(nation.columns.windows(2).all(|w| w[0].position < w[1].position));
}

fn test_cert() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../harness/sources/tls/ca.crt")
}

#[test]
fn sslmode_is_libpqs_ladder_under_libpqs_names_and_mysqls() {
    use SslMode::*;
    for (s, want) in [
        ("disable", Disable),
        ("DISABLED", Disable),
        ("off", Disable),
        ("prefer", Prefer),
        ("Preferred", Prefer),
        ("allow", Prefer),
        ("require", Require),
        ("REQUIRED", Require),
        ("on", Require),
        ("verify-ca", VerifyCa),
        ("VERIFY_CA", VerifyCa),
        ("verify-full", VerifyFull),
        ("verify_identity", VerifyFull),
        (" Verify-Full ", VerifyFull),
    ] {
        assert_eq!(SslMode::parse(s).unwrap(), want, "{s}");
    }
    let err = SslMode::parse("yes please").err().unwrap().to_string();
    assert!(err.contains("not one of disable, prefer, require, verify-ca, verify-full"), "{err}");

    assert_eq!(SslMode::default(), VerifyFull, "secure by default, as the drivers moved to in 2022");
    assert!(!Disable.insists() && !Prefer.insists() && Require.insists());
    assert!(!Require.verifies(), "require encrypts and does not verify, as in libpq");
    assert!(VerifyCa.verifies() && VerifyFull.verifies());
    assert!(Disable < Prefer && Prefer < Require && Require < VerifyCa && VerifyCa < VerifyFull);
}

#[test]
fn the_connection_reads_sslmode_and_ca_file_under_every_spelling() {
    let dir = temp("sslkeys");
    std::fs::write(
        dir.join("datasources.yml"),
        "a:\n  adapter: postgres\nb:\n  adapter: postgres\n  sslmode: require\n  ca_file: /x/ca.pem\n\
         c:\n  adapter: mysql\n  ssl_mode: VERIFY_CA\n  ssl_ca: /y/ca.pem\nd:\n  adapter: postgres\n  sslrootcert: /z/root.crt\n",
    )
    .unwrap();
    let all = Connection::read_all(&dir).unwrap();
    let by = |u: &str| all.iter().find(|c| c.uid == u).unwrap().clone();
    assert_eq!(by("a").sslmode, SslMode::VerifyFull, "unsaid is verify-full");
    assert_eq!(by("a").ca_file, None);
    assert_eq!(by("b").sslmode, SslMode::Require);
    assert_eq!(by("b").ca_file, Some(PathBuf::from("/x/ca.pem")));
    assert_eq!(by("c").sslmode, SslMode::VerifyCa, "MySQL's spelling and key");
    assert_eq!(by("c").ca_file, Some(PathBuf::from("/y/ca.pem")));
    assert_eq!(by("d").ca_file, Some(PathBuf::from("/z/root.crt")), "libpq's key");
    std::fs::write(dir.join("datasources.yml"), "e:\n  adapter: postgres\n  sslmode: maybe\n").unwrap();
    assert!(Connection::read_all(&dir).err().unwrap().to_string().contains("sslmode maybe"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plain_url_is_refused_when_the_mode_insists_and_noted_when_it_prefers() {
    let conn = |mode: SslMode, url: &str| Connection {
        uid: "ch".into(),
        adapter: "clickhouse".into(),
        url: Some(url.into()),
        sslmode: mode,
        ..Default::default()
    };
    // verify-full, the default, refuses http outright.
    let err = open(&conn(SslMode::VerifyFull, "http://ch.internal:8123"), Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("requires TLS") && err.contains("plain http"), "{err}");
    let err = open(&conn(SslMode::Require, "HTTP://ch.internal:8123"), Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("sslmode: disable"), "it says the way out: {err}");
    // disable allows it and says nothing; prefer allows it and says so.
    // (Neither reaches a server: ClickHouse's open probes, so they fail
    // on connect, which is past the rule being tested.)
    let e = open(&conn(SslMode::Disable, "http://127.0.0.1:1"), Secrets::default()).err().unwrap().to_string();
    assert!(!e.contains("requires TLS"), "{e}");
    // A host with no url builds https under an insisting mode and http otherwise.
    let built = |mode: SslMode| {
        let c = Connection { uid: "t".into(), adapter: "trino".into(), host: Some("h".into()), sslmode: mode, ..Default::default() };
        crate::http::base_url(&c, 8080, "trino").map(|(u, note)| (u, note.is_some()))
    };
    assert_eq!(built(SslMode::VerifyFull).unwrap(), ("https://h:8080".to_string(), false));
    assert_eq!(built(SslMode::Disable).unwrap(), ("http://h:8080".to_string(), false));
    assert_eq!(built(SslMode::Prefer).unwrap(), ("http://h:8080".to_string(), true), "prefer over http is noted");
}

#[test]
fn ca_file_is_read_and_an_empty_or_missing_one_is_named() {
    let store = tls::roots("t", Some(&test_cert())).unwrap();
    assert!(!store.is_empty());
    let err = tls::roots("t", Some(Path::new("/nonexistent/ca.pem"))).err().unwrap().to_string();
    assert!(err.starts_with("reading ca_file t: /nonexistent/ca.pem"), "{err}");
    let dir = temp("ca-empty");
    std::fs::write(dir.join("empty.pem"), "not a certificate\n").unwrap();
    let err = tls::roots("t", Some(&dir.join("empty.pem"))).err().unwrap().to_string();
    assert!(err.contains("no certificates"), "{err}");
    for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
        tls::client_config("t", mode, Some(&test_cert())).unwrap_or_else(|e| panic!("{mode:?}: {e}"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Postgres container offers TLS with harness/sources/tls's pair, so
/// each rung can be proven: `ZSQL_TEST_PG_TLS=1`, with the usual
/// `ZSQL_TEST_PG_*` and `ZSQL_TEST_PG_CA` naming the certificate.
#[test]
#[ignore]
fn a_real_postgres_with_tls_proves_every_rung() {
    // Never a pass by default: an ignored test that is run is meant to run.
    std::env::var("ZSQL_TEST_PG_TLS").expect("ZSQL_TEST_PG_TLS");
    let base = Connection {
        uid: "pg".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).or(Some(5466)),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        ..Default::default()
    };
    let secret = || Some(Secret(std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD")));
    // A relative ZSQL_TEST_PG_CA is relative to the repository, as bin/ci
    // and the workflow spell it; cargo runs this binary from the crate.
    let ca = std::env::var("ZSQL_TEST_PG_CA")
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
            }
        })
        .unwrap_or_else(|_| test_cert());
    let with = |mode: SslMode, ca_file: Option<PathBuf>| Connection { sslmode: mode, ca_file, ..base.clone() };
    let ssl_of = |src: &mut Box<dyn Source>| {
        let r = src.query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", 1).unwrap();
        r.rows[0][0].clone()
    };

    let mut s = open(&with(SslMode::Disable, None), Secrets::password(secret())).expect("disable connects");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("f"));
    let mut s = open(&with(SslMode::Prefer, None), Secrets::password(secret())).expect("prefer connects");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"), "the server offers TLS, so prefer takes it");
    assert!(s.warnings().is_empty(), "nothing to warn about: {:?}", s.warnings());
    let mut s = open(&with(SslMode::Require, None), Secrets::password(secret())).expect("require accepts a certificate signed by nobody");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
    let err = open(&with(SslMode::VerifyFull, None), Secrets::password(secret()))
        .err()
        .expect("verify-full refuses a self-signed certificate")
        .to_string();
    assert!(err.contains("certificate") || err.contains("Certificate") || err.contains("tls") || err.contains("TLS"), "{err}");
    let mut s =
        open(&with(SslMode::VerifyFull, Some(ca.clone())), Secrets::password(secret())).expect("verify-full with the private CA trusted");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
    let mut s = open(&with(SslMode::VerifyCa, Some(ca)), Secrets::password(secret())).expect("verify-ca with the private CA trusted");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
}

// ---------------------------------------------------------------- snowflake

/// A warehouse of one thread on a loopback port: every request is
/// answered by `reply` from its method, path and body, and recorded with
/// its headers for the test to read back. Responses close the socket so
/// the agent reconnects each time and nothing is left half-read.
struct Fake {
    url: String,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String, String, String)>>>,
}

fn fake(reply: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static) -> Fake {
    // A fake answers REST and nothing else, so no driver is looked for on
    // the thread that talks to it, whatever this machine has installed.
    #[cfg(not(target_env = "musl"))]
    crate::adbc::rest_only_for_tests();
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let split = loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break Some(i);
                    }
                }
            };
            let Some(i) = split else { continue };
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let body = String::from_utf8_lossy(&buf[i + 4..]).to_string();
            let mut first = head.lines().next().unwrap_or_default().split(' ');
            let method = first.next().unwrap_or_default().to_string();
            let path = first.next().unwrap_or_default().to_string();
            let (status, out) = reply(&method, &path, &body);
            record.lock().unwrap().push((method, path, head, body));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    Fake { url, seen }
}

/// A finished statement as the SQL API shapes it.
fn sf_result(handle: &str, columns: &[&str], data: serde_json::Value, num_rows: usize, partitions: usize) -> String {
    serde_json::json!({
        "resultSetMetaData": {
            "numRows": num_rows,
            "format": "jsonv2",
            "rowType": columns.iter().map(|c| serde_json::json!({"name": c, "type": "text"})).collect::<Vec<_>>(),
            "partitionInfo": (0..partitions).map(|_| serde_json::json!({"rowCount": 1})).collect::<Vec<_>>(),
        },
        "data": data,
        "code": "090001",
        "statementHandle": handle,
        "message": "Statement executed successfully.",
    })
    .to_string()
}

/// `query_timeout` is the whole read's, the arrival of its rows
/// included: a statement that finished in time and whose pages are slow
/// to come is stopped at the same limit, and a capped read with it.
#[test]
fn the_timeout_covers_rows_that_are_still_arriving() {
    let server = fake(|method, path, _body| match (method, path) {
        ("POST", "/api/v2/statements") => (200, sf_result("h0", &["N"], serde_json::json!([["1"]]), 4, 4)),
        ("GET", p) if p.starts_with("/api/v2/statements/h0?partition=") => {
            std::thread::sleep(std::time::Duration::from_millis(700));
            (200, serde_json::json!({"data": [["2"]]}).to_string())
        }
        _ => (404, format!("{{\"message\":\"no route {method} {path}\"}}")),
    });
    let conn = Connection {
        uid: "snow".into(),
        adapter: "snowflake".into(),
        url: Some(server.url.clone()),
        database: Some("db".into()),
        sslmode: SslMode::Disable,
        query_timeout: Some(1),
        ..Default::default()
    };
    let token = || Secrets { oauth_token: Some(Secret("t".into())), ..Default::default() };
    let mut src = crate::adapters::snowflake::Snowflake::open(&conn, token()).unwrap();
    let started = std::time::Instant::now();
    let err = src.stream("SELECT n FROM slow_to_arrive", &mut Kept::default()).unwrap_err().to_string();
    assert!(err.contains("still arriving after 1s") && err.contains("query_timeout"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "it stopped fetching: {:?}", started.elapsed());
    let err = src.query("SELECT n FROM slow_to_arrive", 100).unwrap_err().to_string();
    assert!(err.contains("still arriving after 1s"), "the capped read is under the same limit: {err}");

    // What a datasource says about waiting: ten seconds unless it says.
    assert_eq!(conn.wait(), std::time::Duration::from_secs(10));
    let said: serde_yaml::Value = serde_yaml::from_str("adapter: snowflake\nquery_wait_time: 3\ncache_ttl: 0\n").unwrap();
    let read = Connection::from_value("wh", &said).unwrap();
    assert_eq!((read.wait(), read.cache_ttl), (std::time::Duration::from_secs(3), Some(crate::CacheTtl::For(std::time::Duration::ZERO))));
    assert!(CONNECTION_KEYS.contains(&"query_wait_time"));
    // A TTL with a unit, never, and one that is not a duration, named.
    let ttl = |v: &str| Connection::from_value("wh", &serde_yaml::from_str(&format!("adapter: snowflake\ncache_ttl: {v}\n")).unwrap());
    assert_eq!(ttl("30m").unwrap().cache_ttl, Some(crate::CacheTtl::For(std::time::Duration::from_secs(1800))));
    assert_eq!(ttl("-1").unwrap().cache_ttl, Some(crate::CacheTtl::Never));
    let err = ttl("1.5h").unwrap_err().to_string();
    assert!(err.contains("datasource wh") && err.contains("not a duration"), "{err}");
}

fn sf_key() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../harness/sources/snowflake/rsa_key.p8")
}

#[test]
fn snowflake_submits_polls_pages_and_caps_with_a_token() {
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polled = polls.clone();
    let server = fake(move |method, path, body| match (method, path) {
        ("POST", "/api/v2/statements") if body.contains("slow") => {
            (202, r#"{"code":"333334","statementHandle":"h1","message":"Asynchronous execution in progress."}"#.into())
        }
        ("POST", "/api/v2/statements") if body.contains("broken") => {
            (422, r#"{"code":"002003","message":"SQL compilation error: Object 'NOPE' does not exist","sqlState":"42S02"}"#.into())
        }
        ("POST", "/api/v2/statements") => (200, sf_result("h0", &["STATUS", "N"], serde_json::json!([["paid", "3"], [null, "1"]]), 3, 2)),
        ("GET", "/api/v2/statements/h0?partition=1") => (200, serde_json::json!({"data": [["open", "1"]]}).to_string()),
        ("GET", "/api/v2/statements/h1") => {
            if polled.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (202, r#"{"code":"333334","statementHandle":"h1","message":"still running"}"#.into())
            } else {
                (200, sf_result("h1", &["ONE"], serde_json::json!([["1"]]), 1, 1))
            }
        }
        _ => (404, format!("{{\"message\":\"no route {method} {path}\"}}")),
    });
    let conn = Connection {
        uid: "snow".into(),
        adapter: "snowflake".into(),
        url: Some(server.url.clone()),
        database: Some("analytics".into()),
        schema: Some("\"Public\"".into()),
        warehouse: Some("compute_wh".into()),
        sslmode: SslMode::Disable,
        query_timeout: Some(5),
        ..Default::default()
    };
    let secrets = Secrets { personal_access_token: Some(Secret("tok".into())), ..Default::default() };
    let mut src = open(&conn, secrets).expect("opens");
    assert_eq!(src.adapter(), "snowflake");

    // The guard refuses before any request.
    assert!(src.query("DROP TABLE orders", 10).is_err());
    assert!(server.seen.lock().unwrap().is_empty());

    // Rows across two partitions, every cell text or null.
    let rows = src.query("SELECT status, count(*) AS n FROM orders GROUP BY 1", 10).expect("reads");
    // The same partitions streamed, handed on as each arrives.
    crate::rows::batch_rows_for_tests(2);
    streams_as_it_queries(src.as_mut(), "SELECT status, count(*) AS n FROM orders GROUP BY 1");
    assert_eq!(rows.columns, ["STATUS", "N"]);
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.get(&rows.rows[0], "STATUS"), Some("paid"));
    assert_eq!(rows.get(&rows.rows[1], "STATUS"), None);
    assert_eq!(rows.get(&rows.rows[2], "STATUS"), Some("open"));
    {
        let seen = server.seen.lock().unwrap();
        let (method, path, head, body) = &seen[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/api/v2/statements"));
        assert!(head.contains("Authorization: Bearer tok") || head.contains("authorization: Bearer tok"), "{head}");
        assert!(head.to_lowercase().contains("x-snowflake-authorization-token-type: programmatic_access_token"), "{head}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["statement"], "SELECT status, count(*) AS n FROM orders GROUP BY 1");
        assert_eq!(body["database"], "ANALYTICS", "folded as Snowflake folds an unquoted name");
        assert_eq!(body["schema"], "Public", "a quoted name is sent as written");
        assert_eq!(body["warehouse"], "COMPUTE_WH");
        assert!(body.get("role").is_none(), "an absent context key is not sent as null");
        assert_eq!(body["timeout"], 5);
        assert_eq!(body["parameters"]["DATE_OUTPUT_FORMAT"], "YYYY-MM-DD");
        assert_eq!(seen[1].1, "/api/v2/statements/h0?partition=1");
    }

    // Over the cap: refused from the total, before the second partition.
    let before = server.seen.lock().unwrap().len();
    let err = src.query("SELECT status FROM orders", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    assert_eq!(server.seen.lock().unwrap().len(), before + 1, "one request, no partition fetched");

    // A statement still running is polled until it is done.
    let rows = src.query("SELECT 1 AS one FROM slow", 10).expect("polls");
    assert_eq!(rows.get(&rows.rows[0], "ONE"), Some("1"));
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);

    // A refusal carries the server's message.
    let err = src.query("SELECT * FROM broken", 10).err().unwrap().to_string();
    assert!(err.contains("422") && err.contains("Object 'NOPE' does not exist"), "{err}");
}

#[test]
fn snowflake_key_pair_signs_a_token_the_account_can_check() {
    // The fingerprint is what openssl prints for the same key; see the README beside it.
    let pem = std::fs::read(sf_key()).unwrap();
    let signer = jwt::Signer::from_pem("snow", &pem).expect("pkcs8");
    assert_eq!(signer.public_key_fingerprint(), "W/K39Ak+DVbIXnQSGsa+qi9HMOPiwrK3vLEAYTyLuF0=");
    let pkcs1 = std::fs::read(sf_key().with_file_name("rsa_key.pkcs1.pem")).unwrap();
    assert_eq!(jwt::Signer::from_pem("snow", &pkcs1).expect("pkcs1").public_key_fingerprint(), signer.public_key_fingerprint());
    let encrypted = std::fs::read(sf_key().with_file_name("rsa_key.encrypted.p8")).unwrap();
    let err = jwt::Signer::from_pem("snow", &encrypted).err().unwrap().to_string();
    assert!(err.contains("encrypted") && err.contains("-nocrypt"), "{err}");

    let server = fake(|_, _, _| (200, sf_result("h", &["ONE"], serde_json::json!([["1"]]), 1, 1)));
    let conn = Connection {
        uid: "snow".into(),
        adapter: "snowflake".into(),
        url: Some(server.url.clone()),
        account: Some("xy12345.us-east-2.aws".into()),
        database: Some("ANALYTICS".into()),
        username: Some("analyst".into()),
        private_key_file: Some(sf_key()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut src = open(&conn, Secrets::default()).expect("a key file in datasources.yml is enough");
    src.query("SELECT 1 AS one", 1).expect("signed");
    let seen = server.seen.lock().unwrap();
    let head = &seen[0].2;
    assert!(head.to_lowercase().contains("x-snowflake-authorization-token-type: keypair_jwt"), "{head}");
    let token =
        head.lines().find_map(|l| l.strip_prefix("Authorization: Bearer ").or_else(|| l.strip_prefix("authorization: Bearer "))).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header: serde_json::Value = serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    let claims: serde_json::Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(
        claims["iss"], "XY12345.ANALYST.SHA256:W/K39Ak+DVbIXnQSGsa+qi9HMOPiwrK3vLEAYTyLuF0=",
        "region and cloud stripped, upper-cased"
    );
    assert_eq!(claims["sub"], "XY12345.ANALYST");
    assert_eq!(claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(), 3600);
    let public = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signer.public_key_pkcs1());
    public
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64.decode(parts[2]).unwrap())
        .expect("the signature checks against the public key");
    drop(seen);

    // The PEM itself in .zsql works too, and so does a path there.
    let text = String::from_utf8(pem).unwrap();
    let secrets = Secrets { private_key: Some(Secret(text)), ..Default::default() };
    assert!(open(&Connection { private_key_file: None, ..conn.clone() }, secrets).is_ok());
    let secrets = Secrets { private_key: Some(Secret(sf_key().display().to_string())), ..Default::default() };
    assert!(open(&Connection { private_key_file: None, ..conn.clone() }, secrets).is_ok());

    // Everything missing is named at once.
    let bare = Connection { uid: "snow".into(), adapter: "snowflake".into(), ..Default::default() };
    let err = open(&bare, Secrets::default()).err().unwrap().to_string();
    for want in [
        "account",
        "database",
        "personal_access_token",
        "ZSQL_DS_SNOW_PERSONAL_ACCESS_TOKEN",
        "private_key_file",
        "ZSQL_DS_SNOW_PRIVATE_KEY",
    ] {
        assert!(err.contains(want), "{want} named: {err}");
    }
    let err = open(&Connection { username: None, ..conn.clone() }, Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("username"), "{err}");
}

#[test]
fn a_snowflake_catalog_reads_information_schema_and_show_keys() {
    let server = fake(|_, _, body| {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        let sql = body["statement"].as_str().unwrap_or_default();
        if sql.contains("INFORMATION_SCHEMA.TABLES") {
            assert!(
                sql.contains("table_schema = 'PUBLIC'") || (sql.contains("table_schema NOT IN (") && sql.contains("'INFORMATION_SCHEMA'")),
                "{sql}"
            );
            (
                200,
                sf_result(
                    "t",
                    &["table_schema", "table_name", "table_kind", "row_estimate", "table_comment"],
                    serde_json::json!([
                        ["PUBLIC", "CUSTOMERS", "table", "20", "Who buys"],
                        ["PUBLIC", "ORDERS", "table", "100", null],
                        ["PUBLIC", "V_ORDERS", "view", null, null]
                    ]),
                    3,
                    1,
                ),
            )
        } else if sql.contains("INFORMATION_SCHEMA.COLUMNS") {
            (
                200,
                sf_result(
                    "c",
                    &["table_schema", "table_name", "column_name", "data_type", "is_nullable", "max_length", "position", "column_comment"],
                    serde_json::json!([
                        ["PUBLIC", "CUSTOMERS", "ID", "NUMBER", "NO", null, "1", null],
                        ["PUBLIC", "CUSTOMERS", "NAME", "TEXT", "YES", "255", "2", "Full name"],
                        ["PUBLIC", "ORDERS", "ID", "NUMBER", "NO", null, "1", null],
                        ["PUBLIC", "ORDERS", "CUSTOMER_ID", "NUMBER", "YES", null, "2", null],
                        ["PUBLIC", "V_ORDERS", "ID", "NUMBER", "YES", null, "1", null]
                    ]),
                    5,
                    1,
                ),
            )
        } else if sql == "SHOW PRIMARY KEYS IN DATABASE" || sql == "SHOW PRIMARY KEYS IN SCHEMA \"PUBLIC\"" {
            (
                200,
                sf_result(
                    "p",
                    &["created_on", "database_name", "schema_name", "table_name", "column_name", "key_sequence", "constraint_name"],
                    serde_json::json!([
                        ["x", "ANALYTICS", "PUBLIC", "ORDERS", "ID", "1", "ORDERS_PK"],
                        ["x", "ANALYTICS", "PUBLIC", "CUSTOMERS", "ID", "1", "CUSTOMERS_PK"]
                    ]),
                    2,
                    1,
                ),
            )
        } else if sql == "SHOW IMPORTED KEYS IN DATABASE" || sql == "SHOW IMPORTED KEYS IN SCHEMA \"PUBLIC\"" {
            (
                200,
                sf_result(
                    "f",
                    &[
                        "pk_schema_name",
                        "pk_table_name",
                        "pk_column_name",
                        "fk_schema_name",
                        "fk_table_name",
                        "fk_column_name",
                        "key_sequence",
                        "fk_name",
                    ],
                    serde_json::json!([["PUBLIC", "CUSTOMERS", "ID", "PUBLIC", "ORDERS", "CUSTOMER_ID", "1", "ORDERS_CUSTOMER_FK"]]),
                    1,
                    1,
                ),
            )
        } else {
            (422, format!("{{\"message\":\"unexpected statement: {sql}\"}}"))
        }
    });
    let conn = Connection {
        uid: "snow".into(),
        adapter: "snowflake".into(),
        url: Some(server.url.clone()),
        database: Some("analytics".into()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let secrets = Secrets { personal_access_token: Some(Secret("tok".into())), ..Default::default() };
    let mut src = open(&conn, secrets).unwrap();
    let cat = Catalog::read(&mut *src, "snow", None).expect("reads");
    assert_eq!(cat.adapter, "snowflake");
    let names: Vec<(String, String)> = cat.tables.iter().map(|t| (t.name.clone(), t.kind.clone())).collect();
    assert_eq!(
        names,
        [("CUSTOMERS".to_string(), "table".to_string()), ("ORDERS".into(), "table".into()), ("V_ORDERS".into(), "view".into())]
    );
    let customers = cat.table(Some("PUBLIC"), "CUSTOMERS").unwrap();
    assert_eq!(customers.rows, Some(20));
    assert_eq!(customers.comment.as_deref(), Some("Who buys"));
    assert_eq!(customers.columns[1].max_length, Some(255));
    assert_eq!(customers.columns[1].comment.as_deref(), Some("Full name"));
    assert_eq!(customers.primary_key, ["ID"]);
    let orders = cat.table(Some("PUBLIC"), "ORDERS").unwrap();
    assert_eq!(orders.primary_key, ["ID"]);
    assert_eq!(
        orders.foreign_keys,
        [ForeignKey {
            name: Some("ORDERS_CUSTOMER_FK".into()),
            columns: vec!["CUSTOMER_ID".into()],
            references_schema: Some("PUBLIC".into()),
            references_table: "CUSTOMERS".into(),
            references_columns: vec!["ID".into()],
        }]
    );

    // Scoped to a schema, the SHOWs name it and the scope is folded.
    let seen_before = server.seen.lock().unwrap().len();
    let scoped = Catalog::read(&mut *src, "snow", Some("public")).expect("reads one schema");
    assert_eq!(scoped.tables.len(), 3);
    let seen = server.seen.lock().unwrap();
    let statements: Vec<String> = seen[seen_before..]
        .iter()
        .map(|(_, _, _, b)| serde_json::from_str::<serde_json::Value>(b).unwrap()["statement"].as_str().unwrap().to_string())
        .collect();
    assert!(statements[0].contains("table_schema = 'PUBLIC'"), "{}", statements[0]);
    assert_eq!(statements[2], "SHOW PRIMARY KEYS IN SCHEMA \"PUBLIC\"");
    assert_eq!(statements[3], "SHOW IMPORTED KEYS IN SCHEMA \"PUBLIC\"");
}

#[test]
fn a_connection_reads_snowflakes_keys() {
    let dir = temp("snowflake-keys");
    std::fs::write(
        dir.join("datasources.yml"),
        "snow:\n  adapter: snowflake\n  account_identifier: myorg-myaccount\n  warehouse: COMPUTE_WH\n  role: analyst\n  private_key_path: ./rsa_key.p8\n  database: ANALYTICS\n",
    )
    .unwrap();
    let c = Connection::read(&dir, "snow").unwrap();
    assert_eq!(c.account.as_deref(), Some("myorg-myaccount"));
    assert_eq!(c.warehouse.as_deref(), Some("COMPUTE_WH"));
    assert_eq!(c.role.as_deref(), Some("analyst"));
    assert_eq!(c.private_key_file.as_deref(), Some(Path::new("./rsa_key.p8")));
    assert!(SECRET_FIELDS.contains(&"private_key") && SECRET_FIELDS.contains(&"secret_access_key"));
}

// ----------------------------------------------------------------- redshift

#[test]
fn redshift_key_arrays_expand_against_the_columns_read() {
    let col = |name: &str, position: i64| Column {
        name: name.into(),
        data_type: "integer".into(),
        nullable: true,
        max_length: None,
        position,
        comment: None,
    };
    let table = |schema: &str, name: &str, columns: Vec<Column>| Table {
        schema: Some(schema.into()),
        name: name.into(),
        kind: "table".into(),
        rows: None,
        comment: None,
        columns,
        primary_key: vec![],
        foreign_keys: vec![],
    };
    // Attribute numbers have gaps where a column was dropped.
    let mut tables = vec![
        table("shop", "customers", vec![col("id", 1), col("name", 3)]),
        table("shop", "orders", vec![col("id", 1), col("customer_id", 2), col("region", 4)]),
    ];
    let k = Rows {
        columns: ["table_schema", "table_name", "key_kind", "key_name", "key_attnums", "ref_schema", "ref_table", "ref_attnums"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows: vec![
            vec![
                Some("shop".into()),
                Some("orders".into()),
                Some("primary".into()),
                Some("orders_pkey".into()),
                Some("{1}".into()),
                None,
                None,
                None,
            ],
            vec![
                Some("shop".into()),
                Some("orders".into()),
                Some("foreign".into()),
                Some("orders_customer_fk".into()),
                Some("{4,2}".into()),
                Some("shop".into()),
                Some("customers".into()),
                Some("{3,1}".into()),
            ],
            vec![Some("shop".into()), Some("nowhere".into()), Some("primary".into()), None, Some("{1}".into()), None, None, None],
        ],
    };
    let expanded = catalog::expand_attnums(&tables, &k);
    assert_eq!(expanded.rows.len(), 3, "one row per key column; a table not read contributes none");
    catalog::apply_keys(&mut tables, &expanded);
    let orders = &tables[1];
    assert_eq!(orders.primary_key, ["id"]);
    assert_eq!(
        orders.foreign_keys,
        [ForeignKey {
            name: Some("orders_customer_fk".into()),
            columns: vec!["region".into(), "customer_id".into()],
            references_schema: Some("shop".into()),
            references_table: "customers".into(),
            references_columns: vec!["name".into(), "id".into()],
        }],
        "columns paired by index, in the array's order"
    );
}

/// Two of Redshift's three statements are valid Postgres, so the fixture
/// proves them: the columns from pg_attribute and the keys as arrays.
#[test]
#[ignore]
fn redshifts_column_and_key_statements_hold_on_postgres() {
    let conn = Connection {
        uid: "test".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).or(Some(5466)),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD");
    let mut src = open(&conn, Secrets::password(Some(Secret(password)))).expect("connects");
    let (_, columns_sql, keys_sql) = catalog::redshift_sql(&Filter::schema(Some("shop")));
    let c = src.query(&columns_sql, 1000).expect("the column statement runs on Postgres");
    let name = c
        .rows
        .iter()
        .find(|r| c.get(r, "table_name") == Some("customers") && c.get(r, "column_name") == Some("name"))
        .expect("customers.name");
    assert_eq!(c.get(name, "column_comment"), Some("As they sign"));
    assert_eq!(c.get(name, "is_nullable"), Some("NO"));
    let k = src.query(&keys_sql, 1000).expect("the key statement runs on Postgres");
    let fk = k.rows.iter().find(|r| k.get(r, "key_kind") == Some("foreign")).expect("orders references customers");
    assert_eq!(k.get(fk, "table_name"), Some("orders"));
    assert_eq!(k.get(fk, "ref_table"), Some("customers"));
    assert!(k.get(fk, "key_attnums").unwrap().starts_with('{'), "arrays arrive as their text");
}

// --------------------------------------------------------------- databricks

/// A finished Databricks statement, as the API shapes it.
fn dbx_result(
    id: &str,
    state: &str,
    columns: &[&str],
    data: serde_json::Value,
    total: usize,
    next: Option<i64>,
    truncated: bool,
) -> String {
    serde_json::json!({
        "statement_id": id,
        "status": { "state": state },
        "manifest": {
            "format": "JSON_ARRAY",
            "schema": { "column_count": columns.len(), "columns": columns.iter().enumerate().map(|(i, c)| serde_json::json!({"name": c, "position": i})).collect::<Vec<_>>() },
            "total_row_count": total,
            "truncated": truncated,
        },
        "result": { "chunk_index": 0, "row_offset": 0, "data_array": data, "next_chunk_index": next },
    })
    .to_string()
}

#[test]
fn databricks_submits_polls_pages_caps_and_mints_a_service_principal_token() {
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polled = polls.clone();
    let server = fake(move |method, path, body| {
        match (method, path) {
        ("POST", "/oidc/v1/token") => (200, r#"{"access_token":"minted","expires_in":3600,"token_type":"Bearer"}"#.into()),
        ("POST", "/api/2.0/sql/statements") if body.contains("slow") => {
            (200, serde_json::json!({"statement_id": "s1", "status": {"state": "PENDING"}}).to_string())
        }
        ("POST", "/api/2.0/sql/statements") if body.contains("broken") => {
            (200, serde_json::json!({"statement_id": "s2", "status": {"state": "FAILED", "error": {"error_code": "BAD_REQUEST", "message": "TABLE_OR_VIEW_NOT_FOUND: nope"}}}).to_string())
        }
        // Streamed, a result is asked for as links to its chunks: the
        // first in the answer, the second behind the chunk it names.
        ("POST", "/api/2.0/sql/statements") if body.contains("EXTERNAL_LINKS") && body.contains("cut") => {
            (200, dbx_result("s4", "SUCCEEDED", &["x"], serde_json::json!([]), 2, None, true))
        }
        ("POST", "/api/2.0/sql/statements") if body.contains("EXTERNAL_LINKS") => (
            200,
            serde_json::json!({
                "statement_id": "s7",
                "status": { "state": "SUCCEEDED" },
                "manifest": { "schema": { "columns": [{"name": "status", "position": 0}, {"name": "n", "position": 1}] }, "total_row_count": 3 },
                "result": { "external_links": [{ "chunk_index": 0, "external_link": "/files/s7-0", "next_chunk_index": 1 }] },
            })
            .to_string(),
        ),
        ("GET", "/files/s7-0") => (200, serde_json::json!([["paid", "3"], [null, "1"]]).to_string()),
        ("GET", "/api/2.0/sql/statements/s7/result/chunks/1") => {
            (200, serde_json::json!({ "external_links": [{ "chunk_index": 1, "external_link": "/files/s7-1" }] }).to_string())
        }
        ("GET", "/files/s7-1") => (200, serde_json::json!([["open", "1"]]).to_string()),
        ("POST", "/api/2.0/sql/statements") if body.contains("wide") => {
            (200, dbx_result("s3", "SUCCEEDED", &["x"], serde_json::json!([["1"], ["2"], ["3"]]), 3, None, true))
        }
        // Cut at row_limit exactly: the total fits the cap, truncated says it did not.
        ("POST", "/api/2.0/sql/statements") if body.contains("cut") => {
            (200, dbx_result("s4", "SUCCEEDED", &["x"], serde_json::json!([["1"], ["2"]]), 2, None, true))
        }
        ("POST", "/api/2.0/sql/statements") => {
            (200, dbx_result("s0", "SUCCEEDED", &["status", "n"], serde_json::json!([["paid", "3"], [null, "1"]]), 3, Some(1), false))
        }
        ("GET", "/api/2.0/sql/statements/s0/result/chunks/1") => {
            (200, serde_json::json!({"chunk_index": 1, "row_offset": 2, "data_array": [["open", "1"]]}).to_string())
        }
        ("GET", "/api/2.0/sql/statements/s1") => {
            if polled.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (200, serde_json::json!({"statement_id": "s1", "status": {"state": "RUNNING"}}).to_string())
            } else {
                (200, dbx_result("s1", "SUCCEEDED", &["one"], serde_json::json!([["1"]]), 1, None, false))
            }
        }
        ("POST", "/api/2.0/sql/statements/s9") => (403, r#"{"error_code":"PERMISSION_DENIED","message":"no"}"#.into()),
        _ => (404, format!("{{\"message\":\"no route {method} {path}\"}}")),
    }
    });
    let conn = Connection {
        uid: "dbx".into(),
        adapter: "databricks".into(),
        url: Some(server.url.clone()),
        warehouse: Some("abc123".into()),
        catalog: Some("main".into()),
        schema: Some("shop".into()),
        client_id: Some("app-id".into()),
        sslmode: SslMode::Disable,
        query_timeout: Some(5),
        ..Default::default()
    };
    let secrets = Secrets { oauth_client_secret: Some(Secret("app-secret".into())), ..Default::default() };
    let mut src = open(&conn, secrets).expect("opens");
    assert_eq!(src.adapter(), "databricks");

    let rows = src.query("SELECT status, count(*) AS n FROM orders GROUP BY 1", 10).expect("reads");
    // The same chunks streamed, handed on as each arrives.
    crate::rows::batch_rows_for_tests(2);
    streams_as_it_queries(src.as_mut(), "SELECT status, count(*) AS n FROM orders GROUP BY 1");
    assert_eq!(rows.columns, ["status", "n"]);
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.get(&rows.rows[1], "status"), None);
    assert_eq!(rows.get(&rows.rows[2], "status"), Some("open"));
    {
        let seen = server.seen.lock().unwrap();
        // The token first, with the client credentials as basic auth and the form body.
        let (method, path, head, body) = &seen[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/oidc/v1/token"));
        use base64::Engine;
        let basic = base64::engine::general_purpose::STANDARD.encode("app-id:app-secret");
        assert!(head.contains(&format!("Basic {basic}")), "{head}");
        assert_eq!(body, "grant_type=client_credentials&scope=all-apis");
        // Then the statement, with the minted token and the cap one past.
        let (_, path, head, body) = &seen[1];
        assert_eq!(path, "/api/2.0/sql/statements");
        assert!(head.contains("Bearer minted"), "{head}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["warehouse_id"], "abc123");
        assert_eq!(body["catalog"], "main");
        assert_eq!(body["schema"], "shop");
        assert_eq!(body["row_limit"], 11);
        assert_eq!(body["wait_timeout"], "5s", "the statement timeout, within what the API allows");
        assert_eq!(body["format"], "JSON_ARRAY");
        assert_eq!(seen[2].1, "/api/2.0/sql/statements/s0/result/chunks/1");
    }
    // One token for the whole session.
    src.query("SELECT 1 AS one FROM slow", 10).expect("polls");
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(server.seen.lock().unwrap().iter().filter(|(_, p, _, _)| p == "/oidc/v1/token").count(), 1);

    // A cap with no room above it, as `zsql run --all` gives, is not
    // added to: it used to overflow.
    assert_eq!(src.query("SELECT status, count(*) AS n FROM orders GROUP BY 1", usize::MAX).expect("reads").rows.len(), 3);
    let err = src.query("SELECT x FROM wide", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    let err = src.query("SELECT x FROM cut", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "truncated alone refuses: {err}");
    // Streamed there is no cap to have cut it, so a truncated answer is
    // Databricks' own limit on an inline result, and is said to be.
    let err = src.stream("SELECT x FROM cut", &mut Kept::default()).unwrap_err().to_string();
    assert!(err.contains("larger than Databricks returns for one statement"), "{err}");
    // The links are fetched with no credential of ours: each carries
    // its own, and the workspace's token is not for where it points.
    {
        let seen = server.seen.lock().unwrap();
        let files: Vec<&(String, String, String, String)> = seen.iter().filter(|r| r.1.starts_with("/files/")).collect();
        assert_eq!(files.len(), 2, "both chunks were fetched");
        assert!(files.iter().all(|r| !r.2.to_lowercase().contains("authorization")), "{files:?}");
        assert!(seen.iter().any(|r| r.1.ends_with("/result/chunks/1") && r.2.to_lowercase().contains("authorization: bearer")));
    }
    let err = src.query("SELECT * FROM broken", 10).err().unwrap().to_string();
    assert!(err.contains("failed") && err.contains("TABLE_OR_VIEW_NOT_FOUND"), "{err}");
    assert!(src.query("DELETE FROM orders", 1).is_err());

    // A token works too, sent as it is.
    let secrets = Secrets { personal_access_token: Some(Secret("dapi123".into())), ..Default::default() };
    let mut src = open(&Connection { client_id: None, ..conn.clone() }, secrets).unwrap();
    src.query("SELECT 1", 10).unwrap();
    assert!(server.seen.lock().unwrap().last().unwrap().2.contains("Bearer dapi123"));

    // Everything missing, named at once.
    let bare = Connection { uid: "dbx".into(), adapter: "databricks".into(), ..Default::default() };
    let err = open(&bare, Secrets::default()).err().unwrap().to_string();
    for want in ["host", "warehouse", "personal_access_token", "client_id", "ZSQL_DS_DBX_OAUTH_CLIENT_SECRET"] {
        assert!(err.contains(want), "{want} named: {err}");
    }
    let err = open(&Connection { client_id: Some("x".into()), ..bare.clone() }, Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("oauth_client_secret"), "{err}");
}

#[test]
fn a_databricks_catalog_reads_unity_information_schema() {
    let server = fake(|_, path, body| {
        if path == "/oidc/v1/token" {
            return (200, r#"{"access_token":"t","expires_in":3600}"#.into());
        }
        let sql = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|b| b["statement"].as_str().map(str::to_string))
            .unwrap_or_default();
        if sql.contains("information_schema.tables") {
            (
                200,
                dbx_result(
                    "t",
                    "SUCCEEDED",
                    &["table_schema", "table_name", "table_kind", "row_estimate", "table_comment"],
                    serde_json::json!([["shop", "customers", "table", null, "Who buys"], ["shop", "orders", "table", null, null]]),
                    2,
                    None,
                    false,
                ),
            )
        } else if sql.contains("information_schema.columns") {
            (
                200,
                dbx_result(
                    "c",
                    "SUCCEEDED",
                    &["table_schema", "table_name", "column_name", "data_type", "is_nullable", "max_length", "position", "column_comment"],
                    serde_json::json!([
                        ["shop", "customers", "id", "bigint", "NO", null, "1", null],
                        ["shop", "customers", "name", "string", "YES", null, "2", "Full name"],
                        ["shop", "orders", "id", "bigint", "NO", null, "1", null],
                        ["shop", "orders", "customer_id", "bigint", "YES", null, "2", null]
                    ]),
                    4,
                    None,
                    false,
                ),
            )
        } else if sql.contains("key_column_usage") {
            assert!(sql.contains("position_in_unique_constraint") && sql.contains("k.table_schema = 'shop'"), "{sql}");
            (
                200,
                dbx_result(
                    "k",
                    "SUCCEEDED",
                    &[
                        "table_schema",
                        "table_name",
                        "key_kind",
                        "key_name",
                        "column_name",
                        "position",
                        "ref_schema",
                        "ref_table",
                        "ref_column",
                    ],
                    serde_json::json!([
                        ["shop", "customers", "primary", "customers_pk", "id", "1", null, null, null],
                        ["shop", "orders", "foreign", "orders_customers_fk", "customer_id", "1", "shop", "customers", "id"]
                    ]),
                    2,
                    None,
                    false,
                ),
            )
        } else {
            (400, format!("{{\"message\":\"unexpected: {sql}\"}}"))
        }
    });
    let conn = Connection {
        uid: "dbx".into(),
        adapter: "databricks".into(),
        url: Some(server.url.clone()),
        warehouse: Some("w".into()),
        catalog: Some("main".into()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut src = open(&conn, Secrets { personal_access_token: Some(Secret("t".into())), ..Default::default() }).unwrap();
    let cat = Catalog::read(&mut *src, "dbx", Some("shop")).expect("reads");
    let orders = cat.table(Some("shop"), "orders").unwrap();
    assert!(orders.primary_key.is_empty());
    assert_eq!(orders.foreign_keys[0].references_table, "customers");
    let customers = cat.table(Some("shop"), "customers").unwrap();
    assert_eq!(customers.primary_key, ["id"]);
    assert_eq!(customers.columns[1].comment.as_deref(), Some("Full name"));
}

// ----------------------------------------------------------------- bigquery

fn bq_result(job: &str, complete: bool, columns: &[&str], rows: serde_json::Value, total: usize, page: Option<&str>) -> String {
    let mut v = serde_json::json!({
        "kind": "bigquery#queryResponse",
        "jobReference": { "projectId": "p", "jobId": job, "location": "US" },
        "jobComplete": complete,
    });
    if complete {
        v["schema"] =
            serde_json::json!({ "fields": columns.iter().map(|c| serde_json::json!({"name": c, "type": "STRING"})).collect::<Vec<_>>() });
        v["totalRows"] = serde_json::Value::String(total.to_string());
        v["rows"] = serde_json::Value::Array(
            rows.as_array()
                .unwrap()
                .iter()
                .map(|r| serde_json::json!({"f": r.as_array().unwrap().iter().map(|c| serde_json::json!({"v": c})).collect::<Vec<_>>()}))
                .collect(),
        );
        if let Some(p) = page {
            v["pageToken"] = serde_json::Value::String(p.into());
        }
    }
    v.to_string()
}

#[test]
fn bigquery_queries_polls_pages_caps_and_signs_a_service_account_grant() {
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polled = polls.clone();
    let server = fake(move |method, path, body| match (method, path) {
        ("POST", "/token") => (200, r#"{"access_token":"granted","expires_in":3599,"token_type":"Bearer"}"#.into()),
        ("POST", "/bigquery/v2/projects/p/queries") if body.contains("slow") => {
            (200, bq_result("j1", false, &[], serde_json::json!([]), 0, None))
        }
        ("POST", "/bigquery/v2/projects/p/queries") if body.contains("broken") => {
            (400, r#"{"error":{"code":400,"message":"Not found: Table p:d.nope","status":"NOT_FOUND"}}"#.into())
        }
        ("POST", "/bigquery/v2/projects/p/queries") if body.contains("wide") => {
            (200, bq_result("j3", true, &["x"], serde_json::json!([["1"]]), 500, Some("more")))
        }
        ("POST", "/bigquery/v2/projects/p/queries") => {
            (200, bq_result("j0", true, &["status", "n"], serde_json::json!([["paid", "3"], [null, "1"]]), 3, Some("pg2")))
        }
        ("GET", p) if p.starts_with("/bigquery/v2/projects/p/queries/j0?") && p.contains("pageToken=pg2") => {
            (200, bq_result("j0", true, &["status", "n"], serde_json::json!([["open", {"f": [{"v": "nested"}]}]]), 3, None))
        }
        ("GET", p) if p.starts_with("/bigquery/v2/projects/p/queries/j1?") => {
            if polled.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (200, bq_result("j1", false, &[], serde_json::json!([]), 0, None))
            } else {
                (200, bq_result("j1", true, &["one"], serde_json::json!([["1"]]), 1, None))
            }
        }
        _ => (404, format!("{{\"error\":{{\"message\":\"no route {method} {path}\"}}}}")),
    });
    // A service-account key file whose token endpoint is the fake.
    let dir = temp("bigquery-key");
    let key = dir.join("sa.json");
    let pem = std::fs::read_to_string(sf_key()).unwrap();
    std::fs::write(
        &key,
        serde_json::json!({"type": "service_account", "client_email": "zsql@p.iam.gserviceaccount.com", "private_key": pem, "token_uri": format!("{}/token", server.url)}).to_string(),
    )
    .unwrap();
    let conn = Connection {
        uid: "bq".into(),
        adapter: "bigquery".into(),
        url: Some(server.url.clone()),
        project: Some("p".into()),
        schema: Some("d".into()),
        location: Some("US".into()),
        key_file: Some(key.clone()),
        sslmode: SslMode::Disable,
        query_timeout: Some(5),
        ..Default::default()
    };
    let mut src = open(&conn, Secrets::default()).expect("opens");
    assert_eq!(src.adapter(), "bigquery");

    let rows = src.query("SELECT status, count(*) AS n FROM d.orders GROUP BY 1", 10).expect("reads");
    // The same pages streamed, handed on as each arrives.
    crate::rows::batch_rows_for_tests(2);
    streams_as_it_queries(src.as_mut(), "SELECT status, count(*) AS n FROM d.orders GROUP BY 1");
    assert_eq!(rows.columns, ["status", "n"]);
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.get(&rows.rows[1], "status"), None);
    assert_eq!(rows.get(&rows.rows[2], "n"), Some(r#"{"f":[{"v":"nested"}]}"#), "a record stays JSON");
    {
        let seen = server.seen.lock().unwrap();
        let (_, path, _, body) = &seen[0];
        assert_eq!(path, "/token");
        let assertion = body.strip_prefix("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=").expect(body);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let parts: Vec<&str> = assertion.split('.').collect();
        let claims: serde_json::Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "zsql@p.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], "https://www.googleapis.com/auth/bigquery.readonly");
        assert_eq!(claims["aud"], format!("{}/token", server.url));
        let signer = jwt::Signer::from_pem("bq", pem.as_bytes()).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signer.public_key_pkcs1());
        public.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64.decode(parts[2]).unwrap()).expect("signed by the key");
        let (_, path, head, body) = &seen[1];
        assert_eq!(path, "/bigquery/v2/projects/p/queries");
        assert!(head.contains("Bearer granted"), "{head}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["useLegacySql"], false);
        assert_eq!(body["defaultDataset"]["datasetId"], "d");
        assert_eq!(body["location"], "US");
        assert_eq!(body["timeoutMs"], 5000);
        assert!(seen[2].1.contains("pageToken=pg2") && seen[2].1.contains("location=US"), "{}", seen[2].1);
    }
    src.query("SELECT 1 AS one FROM slow", 10).expect("polls until complete");
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let before = server.seen.lock().unwrap().len();
    let err = src.query("SELECT x FROM wide", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    assert_eq!(server.seen.lock().unwrap().len(), before + 1, "refused from totalRows, no page fetched");
    let err = src.query("SELECT * FROM broken", 10).err().unwrap().to_string();
    assert!(err.contains("400") && err.contains("Not found: Table"), "{err}");

    // Application-default credentials: a refresh token traded at the same endpoint.
    let adc = dir.join("adc.json");
    std::fs::write(&adc, serde_json::json!({"type": "authorized_user", "client_id": "cid", "client_secret": "csec", "refresh_token": "rt", "token_uri": format!("{}/token", server.url)}).to_string()).unwrap();
    let env = |k: &str| (k == "GOOGLE_APPLICATION_CREDENTIALS").then(|| adc.display().to_string());
    let mut src =
        bigquery::BigQuery::open_with(&Connection { key_file: None, ..conn.clone() }, Secrets::default(), &env, None).expect("adc");
    src.query("SELECT 1", 10).unwrap();
    let seen = server.seen.lock().unwrap();
    let token = seen.iter().rev().find(|(_, p, _, _)| p == "/token").unwrap();
    assert_eq!(token.3, "grant_type=refresh_token&client_id=cid&client_secret=csec&refresh_token=rt");
    drop(seen);

    // A service account kept in .zsql: its key and the address it signs
    // for, with no file anywhere. The key is the PEM, as YAML unescaped
    // it or with the JSON file's own escapes left in, or a path to it.
    let bare = Connection { key_file: None, client_email: Some("zsql@p.iam.gserviceaccount.com".into()), ..conn.clone() };
    let nowhere = |_: &str| None;
    for key in [pem.clone(), pem.replace('\n', "\\n"), sf_key().display().to_string()] {
        let secrets = Secrets { private_key: Some(Secret(key)), ..Default::default() };
        assert_eq!(credentials_at_hand(&bare, &secrets), Some(true));
        bigquery::BigQuery::open_with(&bare, secrets.clone(), &nowhere, None).expect("a key in .zsql opens");
        assert!(crate::adbc::bigquery(&bare, &secrets, &nowhere).is_none(), "the driver is not handed the key");
    }
    // It is used before a file, so a bad one is said and not passed over.
    let secrets = Secrets { private_key: Some(Secret("-----BEGIN PRIVATE KEY-----\nnot a key".into())), ..Default::default() };
    let err =
        bigquery::BigQuery::open_with(&Connection { key_file: Some(key.clone()), ..bare.clone() }, secrets, &nowhere, None).err().unwrap();
    assert!(matches!(err, Error::NoCredentials(..)), "{err}");
    // Half of one is named by the half that is missing.
    let secrets = Secrets { private_key: Some(Secret(pem.clone())), ..Default::default() };
    let half = Connection { client_email: None, key_file: None, ..conn.clone() };
    let err = bigquery::BigQuery::open_with(&half, secrets.clone(), &nowhere, None).err().unwrap().to_string();
    assert!(err.contains("client_email") && err.contains(".zsql under bq"), "{err}");
    assert!(!crate::adapters::bigquery::has_inline(&half, &secrets));
    // And a listing says so, even beside a file that would have done.
    assert_eq!(credentials_at_hand(&Connection { key_file: Some(key.clone()), ..half.clone() }, &secrets), Some(false));
    assert_eq!(credentials_at_hand(&Connection { key_file: Some(key.clone()), ..half.clone() }, &Secrets::default()), Some(true));

    // Nothing to go on is named; a wrong file is named with its path.
    let err = bigquery::BigQuery::open_with(
        &Connection { key_file: None, ..conn.clone() },
        Secrets::default(),
        &|_| None,
        Some(dir.join("absent.json")),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("key_file") && err.contains("GOOGLE_APPLICATION_CREDENTIALS"), "{err}");
    assert!(err.contains("private_key and client_email in .zsql under bq"), "{err}");
    std::fs::write(dir.join("odd.json"), r#"{"type": "external_account"}"#).unwrap();
    let err = open(&Connection { key_file: Some(dir.join("odd.json")), ..conn.clone() }, Secrets::default()).err().unwrap().to_string();
    assert!(err.contains("odd.json") && err.contains("external_account"), "{err}");
}

#[test]
fn a_bigquery_catalog_reads_a_dataset_or_a_region() {
    let server = fake(|_, path, body| {
        if path == "/token" {
            return (200, r#"{"access_token":"t","expires_in":3600}"#.into());
        }
        let sql =
            serde_json::from_str::<serde_json::Value>(body).ok().and_then(|b| b["query"].as_str().map(str::to_string)).unwrap_or_default();
        if sql.contains("INFORMATION_SCHEMA.TABLES") {
            (
                200,
                bq_result(
                    "t",
                    true,
                    &["table_schema", "table_name", "table_kind", "row_estimate", "table_comment"],
                    serde_json::json!([["d", "orders", "table", "100", "Sales"]]),
                    1,
                    None,
                ),
            )
        } else if sql.contains("INFORMATION_SCHEMA.COLUMNS") {
            (
                200,
                bq_result(
                    "c",
                    true,
                    &["table_schema", "table_name", "column_name", "data_type", "is_nullable", "max_length", "position", "column_comment"],
                    serde_json::json!([
                        ["d", "orders", "id", "INT64", "NO", null, "1", null],
                        ["d", "orders", "status", "STRING(20)", "YES", "20", "2", "Paid or open"]
                    ]),
                    2,
                    None,
                ),
            )
        } else if sql.contains("KEY_COLUMN_USAGE") {
            (
                200,
                bq_result(
                    "k",
                    true,
                    &[
                        "table_schema",
                        "table_name",
                        "key_kind",
                        "key_name",
                        "column_name",
                        "position",
                        "ref_schema",
                        "ref_table",
                        "ref_column",
                    ],
                    serde_json::json!([["d", "orders", "primary", "pk$", "id", "1", null, null, null]]),
                    1,
                    None,
                ),
            )
        } else {
            (400, format!("{{\"error\":{{\"message\":\"unexpected: {sql}\"}}}}"))
        }
    });
    let dir = temp("bigquery-cat");
    let key = dir.join("adc.json");
    std::fs::write(&key, serde_json::json!({"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r", "token_uri": format!("{}/token", server.url)}).to_string()).unwrap();
    let conn = Connection {
        uid: "bq".into(),
        adapter: "bigquery".into(),
        url: Some(server.url.clone()),
        project: Some("p".into()),
        key_file: Some(key),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let cat = Catalog::read(&mut *src, "bq", Some("d")).expect("a dataset");
    let orders = cat.table(Some("d"), "orders").unwrap();
    assert_eq!(orders.rows, Some(100));
    assert_eq!(orders.comment.as_deref(), Some("Sales"));
    assert_eq!(orders.columns[1].max_length, Some(20), "from STRING(20)");
    assert_eq!(orders.primary_key, ["id"]);
    let statements: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, p, _, _)| p != "/token")
        .map(|(_, _, _, b)| serde_json::from_str::<serde_json::Value>(b).unwrap()["query"].as_str().unwrap().to_string())
        .collect();
    assert!(
        statements[0].contains("`p`.`d`.INFORMATION_SCHEMA.TABLES") && statements[0].contains("`p`.`d`.__TABLES__"),
        "{}",
        statements[0]
    );
    assert!(statements[1].contains("COLUMN_FIELD_PATHS") && statements[1].contains("REGEXP_EXTRACT"), "{}", statements[1]);

    // Without a dataset the region is read, and that needs a location.
    let err = Catalog::read(&mut *src, "bq", None).err().unwrap().to_string();
    assert!(err.contains("location"), "{err}");
    let mut src = open(&Connection { location: Some("eu".into()), ..conn.clone() }, Secrets::default()).unwrap();
    let _ = Catalog::read(&mut *src, "bq", None).expect("a region");
    let last = server.seen.lock().unwrap();
    let regional: Vec<String> = last
        .iter()
        .rev()
        .take(3)
        .filter(|(_, p, _, _)| p != "/token")
        .map(|(_, _, _, b)| serde_json::from_str::<serde_json::Value>(b).unwrap()["query"].as_str().unwrap().to_string())
        .collect();
    assert!(regional.iter().all(|s| s.contains("`p`.`region-eu`.INFORMATION_SCHEMA")), "{regional:?}");
    assert!(regional.iter().all(|s| !s.contains("__TABLES__")), "no row count region-wide: {regional:?}");
}

// ------------------------------------------------------------------- athena

/// The worked example in the Signature Version 4 documentation: an IAM
/// ListUsers call on 2015-08-30 with the example key, whose signature the
/// documentation prints.
#[test]
fn sigv4_reproduces_the_documented_example() {
    let creds = sigv4::Credentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: Secret("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
        session_token: None,
    };
    let headers = vec![
        ("Content-Type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string()),
        ("Host".to_string(), "iam.amazonaws.com".to_string()),
    ];
    let req = sigv4::Request { method: "GET", path: "/", query: "Action=ListUsers&Version=2010-05-08", headers: &headers, body: b"" };
    let signed = sigv4::sign(&creds, "us-east-1", "iam", &req, "20150830T123600Z");
    let auth = &signed.iter().find(|(k, _)| k == "authorization").unwrap().1;
    assert_eq!(
        auth,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
    );
    assert!(signed.iter().all(|(k, _)| k != "x-amz-security-token"));
    let with_token = sigv4::sign(
        &sigv4::Credentials { session_token: Some(Secret("tok".into())), ..creds },
        "us-east-1",
        "iam",
        &req,
        "20150830T123600Z",
    );
    assert!(with_token.iter().any(|(k, v)| k == "x-amz-security-token" && v == "tok"));
    assert!(with_token
        .iter()
        .any(|(k, v)| k == "authorization" && v.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token")));
    assert_eq!(sigv4::stamp(1_440_938_160), "20150830T123600Z");
}

#[test]
fn athena_starts_polls_pages_drops_the_header_row_and_walks_the_credential_chain() {
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polled = polls.clone();
    let server = fake(move |_, _, body| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        // The fake dispatches on the body: the target rides in a header the
        // reply closure does not see, but each action's body is distinct.
        if let Some(sql) = v.get("QueryString").and_then(|q| q.as_str()) {
            let id = if sql.contains("slow") {
                "q-slow"
            } else if sql.contains("broken") {
                "q-broken"
            } else if sql.contains("wide") {
                "q-wide"
            } else {
                "q0"
            };
            return (200, serde_json::json!({"QueryExecutionId": id}).to_string());
        }
        let id = v.get("QueryExecutionId").and_then(|i| i.as_str()).unwrap_or_default();
        if v.get("MaxResults").is_none() {
            // GetQueryExecution (or StopQueryExecution, which has no MaxResults either).
            let state = match id {
                "q-slow" => {
                    if polled.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        "RUNNING"
                    } else {
                        "SUCCEEDED"
                    }
                }
                "q-broken" => "FAILED",
                _ => "SUCCEEDED",
            };
            return (
                200,
                serde_json::json!({"QueryExecution": {"Status": {"State": state, "StateChangeReason": "SYNTAX_ERROR: line 1:8: nope"}}})
                    .to_string(),
            );
        }
        let page = |cols: &[&str], rows: &[&[Option<&str>]], next: Option<&str>| {
            let mut r = serde_json::json!({"ResultSet": {
                "Rows": rows.iter().map(|row| serde_json::json!({"Data": row.iter().map(|c| match c { Some(s) => serde_json::json!({"VarCharValue": s}), None => serde_json::json!({}) }).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                "ResultSetMetadata": {"ColumnInfo": cols.iter().map(|c| serde_json::json!({"Name": c, "Type": "varchar"})).collect::<Vec<_>>()}
            }});
            if let Some(n) = next {
                r["NextToken"] = serde_json::Value::String(n.into());
            }
            r.to_string()
        };
        match (id, v.get("NextToken").and_then(|t| t.as_str())) {
            ("q0", None) => {
                (200, page(&["status", "n"], &[&[Some("status"), Some("n")], &[Some("paid"), Some("3")], &[None, Some("1")]], Some("t2")))
            }
            ("q0", Some("t2")) => (200, page(&["status", "n"], &[&[Some("open"), Some("1")]], None)),
            ("q-wide", None) => (200, page(&["x"], &[&[Some("x")], &[Some("1")], &[Some("2")], &[Some("3")]], None)),
            _ => (200, page(&["one"], &[&[Some("one")], &[Some("1")]], None)),
        }
    });
    let conn = Connection {
        uid: "ath".into(),
        adapter: "athena".into(),
        url: Some(server.url.clone()),
        region: Some("us-east-1".into()),
        database: Some("shop".into()),
        output_location: Some("s3://results/zsql/".into()),
        workgroup: Some("primary".into()),
        sslmode: SslMode::Disable,
        query_timeout: Some(5),
        ..Default::default()
    };
    let secrets = Secrets {
        access_key_id: Some(Secret("AKIAEXAMPLE".into())),
        secret_access_key: Some(Secret("s3cr3t".into())),
        ..Default::default()
    };
    let mut src = open(&conn, secrets).expect("opens");
    assert_eq!(src.adapter(), "athena");

    let rows = src.query("SELECT status, count(*) AS n FROM orders GROUP BY 1", 10).expect("reads");
    // The same pages streamed: the header row dropped once, the rows handed on as they come.
    crate::rows::batch_rows_for_tests(2);
    streams_as_it_queries(src.as_mut(), "SELECT status, count(*) AS n FROM orders GROUP BY 1");
    assert_eq!(rows.columns, ["status", "n"]);
    assert_eq!(rows.rows.len(), 3, "the header row is dropped: {:?}", rows.rows);
    assert_eq!(rows.get(&rows.rows[0], "status"), Some("paid"));
    assert_eq!(rows.get(&rows.rows[1], "status"), None);
    assert_eq!(rows.get(&rows.rows[2], "status"), Some("open"));
    {
        let seen = server.seen.lock().unwrap();
        let (_, _, head, body) = &seen[0];
        let head = head.to_lowercase();
        assert!(head.contains("x-amz-target: amazonathena.startqueryexecution"), "{head}");
        assert!(
            head.contains("authorization: aws4-hmac-sha256 credential=akiaexample/")
                && head.contains("/us-east-1/athena/aws4_request, signedheaders=content-type;host;x-amz-date;x-amz-target, signature="),
            "{head}"
        );
        assert!(head.contains("x-amz-date: 20"), "{head}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["QueryExecutionContext"]["Database"], "shop");
        assert_eq!(body["QueryExecutionContext"]["Catalog"], "AwsDataCatalog");
        assert_eq!(body["ResultConfiguration"]["OutputLocation"], "s3://results/zsql/");
        assert_eq!(body["WorkGroup"], "primary");
        assert!(seen[1].2.to_lowercase().contains("x-amz-target: amazonathena.getqueryexecution"));
        assert!(seen[2].2.to_lowercase().contains("x-amz-target: amazonathena.getqueryresults"));
        assert!(seen[2].3.contains("MaxResults"));
        assert!(seen[3].3.contains("\"NextToken\":\"t2\""), "{}", seen[3].3);
    }
    src.query("SELECT 1 AS one FROM slow", 10).expect("polls");
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let err = src.query("SELECT x FROM wide", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    let err = src.query("SELECT * FROM broken", 10).err().unwrap().to_string();
    assert!(err.contains("failed") && err.contains("SYNTAX_ERROR"), "{err}");
    assert!(src.query("INSERT INTO orders VALUES (1)", 1).is_err());

    // The chain: explicit keys, then the environment, then the profile file.
    let dir = temp("athena-chain");
    std::fs::create_dir_all(dir.join(".aws")).unwrap();
    std::fs::write(dir.join(".aws/credentials"), "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = d\n\n[analytics]\n# a comment\naws_access_key_id = AKIAPROFILE\naws_secret_access_key = p\naws_session_token = sess\n").unwrap();
    std::fs::write(dir.join(".aws/config"), "[profile analytics]\nregion = eu-west-1\n").unwrap();
    let home = dir.clone();
    let from_env = |k: &str| match k {
        "AWS_ACCESS_KEY_ID" => Some("AKIAENV".to_string()),
        "AWS_SECRET_ACCESS_KEY" => Some("e".to_string()),
        "AWS_SHARED_CREDENTIALS_FILE" => Some(home.join(".aws/credentials").display().to_string()),
        "AWS_CONFIG_FILE" => Some(home.join(".aws/config").display().to_string()),
        _ => None,
    };
    let bare = Connection {
        uid: "ath".into(),
        adapter: "athena".into(),
        url: Some(server.url.clone()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut s =
        athena::Athena::open_with(&Connection { profile: Some("analytics".into()), ..bare.clone() }, Secrets::default(), &from_env, None)
            .expect("env keys, profile region");
    s.query("SELECT 1", 10).unwrap();
    let head = server.seen.lock().unwrap().last().unwrap().2.to_lowercase();
    assert!(head.contains("credential=akiaenv/") && head.contains("/eu-west-1/athena/"), "{head}");
    let no_env = |k: &str| match k {
        "AWS_SHARED_CREDENTIALS_FILE" | "AWS_CONFIG_FILE" => from_env(k),
        _ => None,
    };
    let mut s =
        athena::Athena::open_with(&Connection { profile: Some("analytics".into()), ..bare.clone() }, Secrets::default(), &no_env, None)
            .expect("profile keys");
    s.query("SELECT 1", 10).unwrap();
    let head = server.seen.lock().unwrap().last().unwrap().2.to_lowercase();
    assert!(head.contains("credential=akiaprofile/") && head.contains("x-amz-security-token: sess"), "{head}");
    // The default profile from the home directory, with no variables at all; the region then must be said.
    let err = athena::Athena::open_with(&bare, Secrets::default(), &|_| None, Some(dir.clone())).err().unwrap().to_string();
    assert!(err.contains("region") && !err.contains("credentials:"), "{err}");
    let s = athena::Athena::open_with(
        &Connection { region: Some("us-west-2".into()), ..bare.clone() },
        Secrets::default(),
        &|_| None,
        Some(dir.clone()),
    );
    assert!(s.is_ok(), "{:?}", s.err().map(|e| e.to_string()));
    let err = athena::Athena::open_with(&bare, Secrets::default(), &|_| None, None).err().unwrap().to_string();
    for want in ["region", "access_key_id", "AWS_ACCESS_KEY_ID", "ZSQL_DS_ATH_SECRET_ACCESS_KEY", "'default'"] {
        assert!(err.contains(want), "{want} named: {err}");
    }
}

#[test]
fn an_athena_catalog_reads_information_schema_without_comments_on_tables() {
    let server = fake(|_, _, body| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        if let Some(sql) = v.get("QueryString").and_then(|q| q.as_str()) {
            let id = if sql.contains("information_schema.tables") { "t" } else { "c" };
            assert!(sql.contains("table_schema = 'shop'"), "{sql}");
            return (200, serde_json::json!({"QueryExecutionId": id}).to_string());
        }
        if v.get("MaxResults").is_none() {
            return (200, serde_json::json!({"QueryExecution": {"Status": {"State": "SUCCEEDED"}}}).to_string());
        }
        let rows = |rows: Vec<Vec<&str>>| {
            serde_json::json!({"ResultSet": {"Rows": rows.iter().map(|r| serde_json::json!({"Data": r.iter().map(|c| serde_json::json!({"VarCharValue": c})).collect::<Vec<_>>()})).collect::<Vec<_>>(), "ResultSetMetadata": {"ColumnInfo": rows[0].iter().map(|c| serde_json::json!({"Name": c})).collect::<Vec<_>>()}}}).to_string()
        };
        match v["QueryExecutionId"].as_str() {
            Some("t") => (
                200,
                rows(vec![
                    vec!["table_schema", "table_name", "table_kind", "row_estimate", "table_comment"],
                    vec!["shop", "orders", "table", "", ""],
                ]),
            ),
            _ => (
                200,
                rows(vec![
                    vec![
                        "table_schema",
                        "table_name",
                        "column_name",
                        "data_type",
                        "is_nullable",
                        "max_length",
                        "position",
                        "column_comment",
                    ],
                    vec!["shop", "orders", "status", "varchar", "YES", "", "1", "Paid or open"],
                ]),
            ),
        }
    });
    let conn = Connection {
        uid: "ath".into(),
        adapter: "athena".into(),
        url: Some(server.url.clone()),
        region: Some("us-east-1".into()),
        database: Some("shop".into()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let secrets = Secrets { access_key_id: Some(Secret("a".into())), secret_access_key: Some(Secret("s".into())), ..Default::default() };
    let mut src = open(&conn, secrets).unwrap();
    let cat = Catalog::read(&mut *src, "ath", Some("shop")).expect("reads");
    let orders = cat.table(Some("shop"), "orders").unwrap();
    assert_eq!(orders.rows, None);
    assert_eq!(orders.columns[0].comment.as_deref(), Some("Paid or open"));
    assert!(orders.primary_key.is_empty());
}

/// A statement that never settles is cancelled at the statement timeout,
/// on every API that polls, and the cancel is sent where that API takes it.
#[test]
fn a_statement_that_never_settles_is_cancelled_at_the_timeout() {
    let server = fake(|method, path, body| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        if path == "/oidc/v1/token" || path == "/token" {
            return (200, r#"{"access_token":"t","expires_in":3600}"#.into());
        }
        if path.contains("/cancel") {
            return (200, "{}".into());
        }
        // Snowflake
        if path == "/api/v2/statements" && method == "POST" && v.get("statement").is_some() && v.get("warehouse_id").is_none() {
            return (202, r#"{"code":"333334","statementHandle":"sf","message":"running"}"#.into());
        }
        if path == "/api/v2/statements/sf" {
            return (202, r#"{"code":"333334","statementHandle":"sf","message":"running"}"#.into());
        }
        // Databricks
        if path.starts_with("/api/2.0/sql/statements") {
            return (200, serde_json::json!({"statement_id": "db", "status": {"state": "RUNNING"}}).to_string());
        }
        // BigQuery
        if path.contains("/bigquery/") {
            return (
                200,
                serde_json::json!({"jobReference": {"projectId": "p", "jobId": "bq", "location": "US"}, "jobComplete": false}).to_string(),
            );
        }
        // Athena: start, then forever RUNNING; the stop has no MaxResults either.
        if v.get("QueryString").is_some() {
            return (200, serde_json::json!({"QueryExecutionId": "ath"}).to_string());
        }
        (200, serde_json::json!({"QueryExecution": {"Status": {"State": "RUNNING"}}}).to_string())
    });
    let base = Connection { url: Some(server.url.clone()), sslmode: SslMode::Disable, query_timeout: Some(1), ..Default::default() };
    let dir = temp("never-settles");
    let key = dir.join("adc.json");
    std::fs::write(&key, serde_json::json!({"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r", "token_uri": format!("{}/token", server.url)}).to_string()).unwrap();
    let cases: Vec<(Connection, Secrets, &str)> = vec![
        (
            Connection { uid: "snow".into(), adapter: "snowflake".into(), database: Some("d".into()), ..base.clone() },
            Secrets { personal_access_token: Some(Secret("t".into())), ..Default::default() },
            "/api/v2/statements/sf/cancel",
        ),
        (
            Connection { uid: "dbx".into(), adapter: "databricks".into(), warehouse: Some("w".into()), ..base.clone() },
            Secrets { personal_access_token: Some(Secret("t".into())), ..Default::default() },
            "/api/2.0/sql/statements/db/cancel",
        ),
        (
            Connection {
                uid: "bq".into(),
                adapter: "bigquery".into(),
                project: Some("p".into()),
                key_file: Some(key.clone()),
                ..base.clone()
            },
            Secrets::default(),
            "/bigquery/v2/projects/p/jobs/bq/cancel?location=US",
        ),
        (
            Connection { uid: "ath".into(), adapter: "athena".into(), region: Some("us-east-1".into()), ..base.clone() },
            Secrets { access_key_id: Some(Secret("a".into())), secret_access_key: Some(Secret("s".into())), ..Default::default() },
            "/",
        ),
    ];
    for (conn, secrets, cancel_path) in cases {
        let mut src = open(&conn, secrets).unwrap_or_else(|e| panic!("{}: {e}", conn.uid));
        let before = server.seen.lock().unwrap().len();
        let started = std::time::Instant::now();
        let err = src.query("SELECT 1 AS one", 10).err().unwrap_or_else(|| panic!("{} should time out", conn.uid)).to_string();
        assert!(err.contains("still running after 1s"), "{}: {err}", conn.uid);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{}: gave up in time", conn.uid);
        let seen = server.seen.lock().unwrap();
        let last = &seen[seen.len() - 1];
        if conn.adapter == "athena" {
            assert!(last.2.to_lowercase().contains("x-amz-target: amazonathena.stopqueryexecution"), "{}: {}", conn.uid, last.2);
        } else {
            assert_eq!(last.1, cancel_path, "{}", conn.uid);
        }
        assert!(seen.len() > before + 1, "{}: polled before giving up", conn.uid);
    }
}

// ------------------------------------------------------------------- duckdb

fn duckdb_file(name: &str) -> PathBuf {
    let dir = temp(name);
    let path = dir.join("shop.duckdb");
    let db = ::duckdb::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name VARCHAR(80) NOT NULL, notes TEXT);\
         COMMENT ON TABLE customers IS 'Who buys';\
         COMMENT ON COLUMN customers.name IS 'As they sign';\
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers (id), \
           placed DATE NOT NULL, total DECIMAL(12, 2) NOT NULL, paid BOOLEAN, tags VARCHAR[], seen TIMESTAMP);\
         CREATE VIEW recent AS SELECT id, customer_id FROM orders WHERE placed > DATE '2026-01-01';\
         INSERT INTO customers VALUES (1, 'Ann', NULL), (2, 'Bo', 'vip');\
         INSERT INTO orders VALUES (1, 1, DATE '2026-03-01', 10.50, true, ['a', 'b'], TIMESTAMP '2026-03-01 09:30:00'), \
           (2, 2, DATE '2026-03-02', 4.25, false, NULL, NULL), (3, 1, DATE '2026-03-03', 7, NULL, [], NULL);",
    )
    .unwrap();
    drop(db);
    path
}

#[test]
fn duckdb_reads_its_file_renders_every_type_as_text_and_cannot_write_it() {
    let path = duckdb_file("duckdb-reads");
    let conn = Connection { uid: "local".into(), adapter: "duckdb".into(), file: Some(path.clone()), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).expect("opens read-only");
    assert_eq!(src.adapter(), "duckdb");
    // Streamed, the engine's own batches with their types, and the same
    // text as the capped read.
    let streamed = streams_as_it_queries(src.as_mut(), "SELECT id, placed, total, paid, tags, seen FROM orders ORDER BY id");
    assert_eq!(streamed.schema.field(2).data_type(), &arrow_schema::DataType::Decimal128(12, 2));
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT id FROM orders WHERE id > 99").num_rows(), 0);
    let rows = src.query("SELECT id, placed, total, paid, tags, seen FROM orders ORDER BY id", 10).expect("reads");
    // Unchecked, nothing here refuses a write, and the engine does: the
    // file is still opened for reading, since opening it for writing
    // would take it from every other process. Writable is for a caller
    // that means to write, and has the file to itself while it does.
    {
        let mut unchecked = open(&Connection { access: Access::Unchecked, ..conn.clone() }, Secrets::default()).unwrap();
        let err = unchecked.query("INSERT INTO customers VALUES (9, 'Cy', NULL)", 10).unwrap_err();
        assert!(matches!(err, Error::Query(_, _)), "refused by DuckDB, not by the guard: {err}");
    }
    drop(src);
    {
        let mut writable = open(&Connection { access: Access::Writable, ..conn.clone() }, Secrets::default()).unwrap();
        writable.query("INSERT INTO customers VALUES (9, 'Cy', NULL)", 10).expect("a write, on a writable connection");
        assert_eq!(writable.query("SELECT count(*) AS n FROM customers", 10).unwrap().rows[0][0].as_deref(), Some("3"));
        writable.query("DELETE FROM customers WHERE id = 9", 10).unwrap();
    }
    let mut src = open(&conn, Secrets::default()).expect("opens read-only again");
    assert_eq!(rows.columns, ["id", "placed", "total", "paid", "tags", "seen"]);
    assert_eq!(rows.rows.len(), 3);
    let first = &rows.rows[0];
    assert_eq!(rows.get(first, "id"), Some("1"));
    assert_eq!(rows.get(first, "placed"), Some("2026-03-01"));
    assert_eq!(rows.get(first, "total"), Some("10.50"), "a decimal keeps its scale");
    assert_eq!(rows.get(first, "paid"), Some("true"));
    assert_eq!(rows.get(first, "tags"), Some("[a, b]"), "a list prints as DuckDB prints it");
    assert_eq!(rows.get(first, "seen"), Some("2026-03-01T09:30:00"));
    assert_eq!(rows.get(&rows.rows[1], "tags"), None);
    assert_eq!(rows.get(&rows.rows[2], "paid"), None);
    // Past the cap: refused, never trimmed.
    let err = src.query("SELECT id FROM orders", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    // The guard refuses a write first; the engine would second it.
    assert!(src.query("DELETE FROM orders", 1).is_err());
    let conn2 =
        ::duckdb::Connection::open_with_flags(&path, ::duckdb::Config::default().access_mode(::duckdb::AccessMode::ReadOnly).unwrap())
            .unwrap();
    assert!(conn2.execute_batch("DELETE FROM orders").is_err(), "the file is opened read-only by the engine");
    // A missing file is named.
    let err = open(&Connection { file: Some(path.with_file_name("absent.duckdb")), ..conn.clone() }, Secrets::default())
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("absent.duckdb") && err.contains("does not exist"), "{err}");
}

#[test]
fn duckdb_interrupts_a_statement_past_the_timeout() {
    let path = duckdb_file("duckdb-interrupt");
    let conn = Connection { uid: "local".into(), adapter: "duckdb".into(), file: Some(path), query_timeout: Some(1), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let started = std::time::Instant::now();
    let err = src.query("SELECT count(*) FROM range(200000) a, range(200000) b WHERE a.range + b.range = -1", 1).err().unwrap().to_string();
    assert!(err.contains("no answer within 1s"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "interrupted, not waited out");
}

#[test]
fn a_duckdb_catalog_reads_tables_views_columns_comments_and_keys() {
    let path = duckdb_file("duckdb-catalog");
    let conn = Connection { uid: "local".into(), adapter: "duckdb".into(), file: Some(path), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let cat = Catalog::read(&mut *src, "local", None).expect("reads");
    assert_eq!(cat.adapter, "duckdb");
    let names: Vec<(String, String)> = cat.tables.iter().map(|t| (t.name.clone(), t.kind.clone())).collect();
    assert_eq!(
        names,
        [("customers".to_string(), "table".to_string()), ("orders".into(), "table".into()), ("recent".into(), "view".into())]
    );
    let customers = cat.table(Some("main"), "customers").unwrap();
    assert_eq!(customers.comment.as_deref(), Some("Who buys"));
    assert_eq!(customers.rows, Some(2), "duckdb_tables() estimates");
    let name = customers.columns.iter().find(|c| c.name == "name").unwrap();
    assert_eq!(name.comment.as_deref(), Some("As they sign"));
    assert_eq!(name.max_length, None, "DuckDB keeps no width on VARCHAR(80): it is VARCHAR");
    assert!(!name.nullable);
    assert_eq!(customers.primary_key, ["id"]);
    let orders = cat.table(Some("main"), "orders").unwrap();
    assert_eq!(orders.primary_key, ["id"]);
    assert_eq!(orders.foreign_keys.len(), 1, "{:?}", orders.foreign_keys);
    assert_eq!(orders.foreign_keys[0].columns, ["customer_id"]);
    assert_eq!(orders.foreign_keys[0].references_table, "customers");
    assert_eq!(orders.foreign_keys[0].references_columns, ["id"]);
    assert!(orders.columns.windows(2).all(|w| w[0].position < w[1].position));
    let scoped = Catalog::read(&mut *src, "local", Some("main")).unwrap();
    assert_eq!(scoped.tables.len(), 3);
    assert!(Catalog::read(&mut *src, "local", Some("nowhere")).unwrap().tables.is_empty());
}

// ------------------------------------------------------------ oauth token

/// A token obtained upstream is the credential, sent as it was given, by
/// every adapter that says it takes one. Nothing here mints or refreshes
/// anything: no token endpoint is called and no file is read.
#[test]
fn an_oauth_token_from_upstream_is_the_credential_wherever_one_is_taken() {
    for adapter in ["snowflake", "bigquery", "databricks", "trino", "sqlserver", "druid"] {
        assert!(takes_oauth_token(adapter), "{adapter}");
    }
    for adapter in ["postgres", "redshift", "mysql", "clickhouse", "athena", "sqlite", "duckdb"] {
        assert!(!takes_oauth_token(adapter), "{adapter}");
    }
    assert!(SECRET_FIELDS.contains(&"oauth_token"));

    let server = fake(|method, path, _body| match (method, path) {
        ("POST", "/api/v2/statements") => (200, sf_result("h0", &["ONE"], serde_json::json!([["1"]]), 1, 1)),
        ("POST", "/bigquery/v2/projects/p/queries") => (200, bq_result("j0", true, &["one"], serde_json::json!([["1"]]), 1, None)),
        ("POST", "/api/2.0/sql/statements") => (200, dbx_result("s0", "SUCCEEDED", &["one"], serde_json::json!([["1"]]), 1, None, false)),
        ("POST", "/v1/statement") => (200, serde_json::json!({"columns": [{"name": "one"}], "data": [["1"]]}).to_string()),
        ("POST", "/druid/v2/sql") => (200, "[\"one\"]\n[1]\n\n".to_string()),
        _ => (404, format!("{{\"message\":\"no route {method} {path}\"}}")),
    });
    let token = || Secrets { oauth_token: Some(Secret("user-token".into())), ..Default::default() };
    let base = Connection { url: Some(server.url.clone()), sslmode: SslMode::Disable, query_timeout: Some(5), ..Default::default() };
    let cases = [
        Connection { uid: "snow".into(), adapter: "snowflake".into(), database: Some("db".into()), ..base.clone() },
        // No key file and no application-default credentials: the token is enough.
        Connection { uid: "bq".into(), adapter: "bigquery".into(), project: Some("p".into()), ..base.clone() },
        Connection { uid: "dbx".into(), adapter: "databricks".into(), warehouse: Some("w1".into()), ..base.clone() },
        Connection { uid: "trino".into(), adapter: "trino".into(), username: Some("ada".into()), ..base.clone() },
        // A token wins over a username, which then has nothing to say.
        Connection { uid: "druid".into(), adapter: "druid".into(), username: Some("ada".into()), ..base.clone() },
    ];
    for conn in cases {
        let before = server.seen.lock().unwrap().len();
        let mut src = open(&conn, token()).unwrap_or_else(|e| panic!("{}: {e}", conn.adapter));
        let rows = src.query("SELECT 1 AS one", 10).unwrap_or_else(|e| panic!("{}: {e}", conn.adapter));
        assert_eq!(rows.rows, vec![vec![Some("1".to_string())]], "{}", conn.adapter);
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.len(), before + 1, "{}: one request, and none of them for a token", conn.adapter);
        let head = seen[before].2.to_lowercase();
        assert!(head.contains("authorization: bearer user-token"), "{}: {head}", conn.adapter);
        if conn.adapter == "snowflake" {
            assert!(head.contains("x-snowflake-authorization-token-type: oauth"), "{head}");
        }
    }
}
// -------------------------------------------------------------------- druid

/// A Druid on a loopback port that answers every statement with `body`,
/// and a connection to it.
fn druid_fake(status: u16, body: &'static str) -> (Fake, Connection) {
    let server = fake(move |method, path, _| match (method, path) {
        ("POST", "/druid/v2/sql") => (status, body.to_string()),
        ("DELETE", p) if p.starts_with("/druid/v2/sql/") => (202, String::new()),
        _ => (404, format!("{{\"errorMessage\":\"no route {method} {path}\"}}")),
    });
    let conn = Connection {
        uid: "dr".into(),
        adapter: "druid".into(),
        url: Some(server.url.clone()),
        sslmode: SslMode::Disable,
        query_timeout: Some(7),
        ..Default::default()
    };
    (server, conn)
}

/// A statement goes as `arrayLines` with a header, in UTC, under Druid's
/// own timeout, with an id to cancel it by and, for a capped read, one
/// row past the cap; the answer is read a line a row, nulls as none.
#[test]
fn druid_is_asked_for_lines_and_its_rows_are_read_as_they_come() {
    let (server, conn) = druid_fake(200, "[\"x\",\"y\",\"z\"]\n[1,\"two\",null]\n[2,\"three\",4.5]\n\n");
    let password = Secrets::password(Some(Secret("pw".into())));
    let mut src = open(&Connection { username: Some("ada".into()), ..conn.clone() }, password).unwrap();
    let rows = src.query("SELECT x, y, z FROM t", 10).unwrap();
    assert_eq!(rows.columns, ["x", "y", "z"]);
    assert_eq!(rows.rows[0], vec![Some("1".into()), Some("two".into()), None]);
    assert_eq!(rows.get(&rows.rows[1], "z"), Some("4.5"));
    assert_eq!((src.adapter(), src.transport()), ("druid", "rest"));

    let seen = server.seen.lock().unwrap();
    let (_, _, head, body) = &seen[0];
    assert!(head.to_lowercase().contains("authorization: basic YWRhOnB3".to_lowercase().as_str()), "ada:pw, Basic: {head}");
    let sent: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(sent["query"], "SELECT x, y, z FROM t");
    assert_eq!((sent["resultFormat"].as_str(), sent["header"].as_bool()), (Some("arrayLines"), Some(true)));
    let context = &sent["context"];
    assert_eq!(context["sqlTimeZone"], "Etc/UTC");
    assert_eq!(context["timeout"], 7000, "query_timeout, in Druid's milliseconds");
    assert_eq!(context["sqlOuterLimit"], 11, "one past the cap, so too many is seen and no more is computed");
    assert!(context["sqlQueryId"].as_str().is_some_and(|id| id.starts_with("zsql-")), "{context}");
    drop(seen);

    // Streamed, there is no cap and so no outer limit.
    crate::rows::batch_rows_for_tests(1);
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT x, y, z FROM t").num_rows(), 2);
    let seen = server.seen.lock().unwrap();
    let last: serde_json::Value = serde_json::from_str(&seen.last().unwrap().3).unwrap();
    assert!(last["context"].get("sqlOuterLimit").is_none(), "{last}");
}

/// A 200 is sent before Druid knows the statement will finish; the
/// blank line at the end is what says it did. Without it the result is
/// refused, capped or streamed, never taken for the whole.
#[test]
fn a_druid_result_without_its_closing_blank_line_was_cut_short() {
    let (_server, conn) = druid_fake(200, "[\"x\"]\n[1]\n[2]\n");
    let mut src = open(&conn, Secrets::default()).unwrap();
    let err = src.query("SELECT x FROM t", 10).unwrap_err().to_string();
    assert!(err.contains("cut short"), "{err}");
    let err = src.stream("SELECT x FROM t", &mut Kept::default()).unwrap_err().to_string();
    assert!(err.contains("cut short"), "{err}");
    // A line that is not a row is Druid's error in place of one.
    let (_server, conn) = druid_fake(200, "[\"x\"]\n[1]\n{\"error\":\"Unknown exception\"\n");
    let err = open(&conn, Secrets::default()).unwrap().query("SELECT x FROM t", 10).unwrap_err().to_string();
    assert!(err.contains("Unknown exception"), "{err}");
}

/// Past the cap the read refuses, and the statement is cancelled by its
/// id rather than left to run.
#[test]
fn a_druid_read_past_its_cap_refuses_and_cancels_the_statement() {
    let (server, conn) = druid_fake(200, "[\"x\"]\n[1]\n[2]\n[3]\n\n");
    let mut src = open(&conn, Secrets::default()).unwrap();
    let err = src.query("SELECT x FROM t", 2).unwrap_err().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    let seen = server.seen.lock().unwrap();
    let sent: serde_json::Value = serde_json::from_str(&seen[0].3).unwrap();
    let id = sent["context"]["sqlQueryId"].as_str().unwrap();
    assert_eq!(seen.get(1).map(|s| (s.0.as_str(), s.1.clone())), Some(("DELETE", format!("/druid/v2/sql/{id}"))));
}

/// A refusal says what Druid said, with its category; a write is
/// refused before anything is sent; a password with no user to go with
/// it is named rather than sent as half a login.
#[test]
fn druid_refusals_are_its_own_words_and_writes_never_leave() {
    let (server, conn) = druid_fake(
        400,
        r#"{"error":"druidException","errorCode":"invalidInput","persona":"USER","category":"INVALID_INPUT","errorMessage":"Column 'nope' not found in any table"}"#,
    );
    let mut src = open(&conn, Secrets::default()).unwrap();
    let err = src.query("SELECT nope FROM t", 10).unwrap_err().to_string();
    assert!(err.contains("400: Column 'nope' not found in any table (INVALID_INPUT)"), "{err}");
    assert!(src.query("INSERT INTO t SELECT * FROM u", 10).is_err());
    assert_eq!(server.seen.lock().unwrap().len(), 1, "the write was never sent");
    assert!(!server.seen.lock().unwrap()[0].2.to_lowercase().contains("authorization"), "no authenticator, no header");

    let err = open(&conn, Secrets::password(Some(Secret("pw".into())))).err().unwrap().to_string();
    assert!(err.contains("has no username to go with its password"), "{err}");
}

/// The counts `sys.segments` gives land on the datasources they name,
/// and only in the `druid` schema.
#[test]
fn druid_row_counts_land_on_their_datasources() {
    let table = |schema: &str, name: &str| Table {
        schema: Some(schema.into()),
        name: name.into(),
        kind: "table".into(),
        rows: None,
        comment: None,
        columns: vec![],
        primary_key: vec![],
        foreign_keys: vec![],
    };
    let mut tables = vec![table("druid", "posts"), table("lookup", "posts"), table("druid", "empty")];
    let counts = Rows {
        columns: vec!["table_name".into(), "row_estimate".into()],
        rows: vec![vec![Some("posts".into()), Some("4".into())], vec![Some("empty".into()), Some("0".into())]],
    };
    catalog::apply_druid_counts(&mut tables, &counts);
    assert_eq!(tables.iter().map(|t| t.rows).collect::<Vec<_>>(), [Some(4), None, None]);
}

/// Druid against a router the environment names, `ZSQL_TEST_DRUID_URL`:
/// the profile `druid` in harness/sources/compose.yml. The tables it
/// reads are loaded here when they are not there yet, through SQL
/// ingestion and the lookup API, so a fresh container is enough.
#[test]
#[ignore]
fn a_real_druid_answers_streams_cancels_and_its_catalog_reads() {
    let url = std::env::var("ZSQL_TEST_DRUID_URL").expect("ZSQL_TEST_DRUID_URL");
    druid_seed(&url);
    let conn = Connection { uid: "dr".into(), adapter: "druid".into(), url: Some(url), sslmode: SslMode::Disable, ..Default::default() };
    let mut src = open(&conn, Secrets::default()).expect("opens");
    // `one` is a keyword in Druid's SQL, so these are letters.
    let rows = src.query("SELECT 1 AS x, 'two' AS y, CAST(NULL AS VARCHAR) AS z", 10).expect("reads");
    assert_eq!(rows.columns, ["x", "y", "z"]);
    assert_eq!((rows.get(&rows.rows[0], "x"), rows.get(&rows.rows[0], "z")), (Some("1"), None));
    let err = src.query("SELECT title FROM posts", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    let all = src.query("SELECT title FROM posts", 4).expect("exactly at the cap");
    assert_eq!(all.rows.len(), 4);
    crate::rows::batch_rows_for_tests(2);
    assert_eq!(streams_as_it_queries(src.as_mut(), "SELECT title FROM posts ORDER BY __time").num_rows(), 4);
    streams_as_it_queries(src.as_mut(), "SELECT title FROM posts WHERE id < 0");
    let ts = src.query("SELECT __time FROM posts ORDER BY __time LIMIT 1", 1).unwrap();
    assert_eq!(ts.rows[0][0].as_deref(), Some("2024-01-01T13:00:00.000Z"), "UTC, as the dialect assumes");
    let err = src.query("SELECT nope FROM posts", 1).err().unwrap().to_string();
    assert!(err.contains("nope") && err.contains("400"), "{err}");
    assert!(src.query("INSERT INTO posts SELECT * FROM posts", 1).is_err());

    let cat = Catalog::read(src.as_mut(), "dr", Some("druid")).expect("reads");
    pages_agree(src.as_mut(), &cat, Some("druid"), 2);
    a_search_means_its_underscores(src.as_mut(), Some("druid"), "ST_T", "post_tags", "s_s");
    let posts = cat.table(Some("druid"), "posts").expect("druid.posts");
    assert_eq!((posts.kind.as_str(), posts.rows), ("table", Some(4)), "the count from sys.segments");
    let names: Vec<&str> = posts.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["__time", "id", "user_id", "title", "published", "views"]);
    assert_eq!(posts.columns[0].data_type, "TIMESTAMP");
    assert!(!posts.columns[0].nullable);
    assert!(posts.primary_key.is_empty() && posts.foreign_keys.is_empty(), "Druid keeps no constraints");

    // Every schema but the system ones: the lookups too, keyed on k.
    let every = Catalog::read(src.as_mut(), "dr", None).expect("reads");
    assert!(every.tables.iter().all(|t| !matches!(t.schema.as_deref(), Some("sys" | "INFORMATION_SCHEMA"))));
    let status = every.table(Some("lookup"), "post_status").expect("lookup.post_status");
    assert_eq!(status.primary_key, ["k"]);
    assert_eq!(status.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["k", "v"]);
}

/// The tables the live test reads, loaded once: three datasources by SQL
/// ingestion and one lookup, waited for until a query sees each.
fn druid_seed(url: &str) {
    let agent = ureq::Agent::new_with_config(ureq::config::Config::builder().http_status_as_error(false).build());
    let sql = |q: &str| -> serde_json::Value {
        let mut r = agent.post(&format!("{url}/druid/v2/sql")).send_json(serde_json::json!({ "query": q })).expect("sql");
        r.body_mut().read_json().unwrap_or_default()
    };
    let present = |schema: &str, names: &[&str]| -> usize {
        let list = names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
        sql(&format!("SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = '{schema}' AND TABLE_NAME IN ({list})"))
            .as_array()
            .map_or(0, Vec::len)
    };
    let datasources = [
        (
            "posts",
            "created_at,id,user_id,title,published,views\n2024-01-01 13:00:00,1,1,First Post,1,10\n2024-01-01 14:00:00,2,1,Draft Post,0,0\n\
             2024-01-02 09:00:00,3,2,Second Post,1,25\n2024-01-03 10:00:00,4,3,Third Post,1,7",
            "id BIGINT, user_id BIGINT, title VARCHAR, published VARCHAR, views BIGINT",
        ),
        (
            "users",
            "created_at,id,name,email\n2024-01-01 10:00:00,1,John Doe,john@example.com\n2024-01-01 11:00:00,2,Jane Smith,jane@example.com\n\
             2024-01-01 12:00:00,3,Bob Johnson,bob@example.com",
            "id BIGINT, name VARCHAR, email VARCHAR",
        ),
        ("post_tags", "created_at,post_id,tag\n2024-01-01 13:00:00,1,intro\n2024-01-02 09:00:00,3,news", "post_id BIGINT, tag VARCHAR"),
    ];
    let names: Vec<&str> = datasources.iter().map(|d| d.0).collect();
    if present("druid", &names) < names.len() {
        for (name, csv, columns) in datasources {
            let cols: Vec<&str> = columns.split(", ").map(|c| c.split(' ').next().unwrap()).collect();
            let source = serde_json::json!({"type": "inline", "data": csv}).to_string().replace('\'', "''");
            let q = format!(
                "REPLACE INTO {name} OVERWRITE ALL SELECT TIME_PARSE(created_at, 'yyyy-MM-dd HH:mm:ss') AS __time, {} \
                 FROM TABLE(EXTERN('{source}', '{{\"type\":\"csv\",\"findColumnsFromHeader\":true}}')) \
                 EXTEND (created_at VARCHAR, {columns}) PARTITIONED BY DAY",
                cols.join(", ")
            );
            let body = serde_json::json!({"query": q, "context": {"maxNumTasks": 2, "waitUntilSegmentsLoad": true}});
            let mut r = agent.post(&format!("{url}/druid/v2/sql/task")).send_json(body).expect("submits");
            let task: serde_json::Value = r.body_mut().read_json().expect("a task");
            let id = task["taskId"].as_str().unwrap_or_else(|| panic!("{name}: {task}")).to_string();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(240);
            loop {
                let mut r = agent.get(&format!("{url}/druid/indexer/v1/task/{id}/status")).call().expect("status");
                let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
                match v.pointer("/status/statusCode").and_then(|s| s.as_str()) {
                    Some("SUCCESS") => break,
                    Some("FAILED") => panic!("{name}: {v}"),
                    _ if std::time::Instant::now() > until => panic!("{name}: still {v}"),
                    _ => std::thread::sleep(std::time::Duration::from_secs(2)),
                }
            }
        }
    }
    if present("lookup", &["post_status"]) == 0 {
        let config = format!("{url}/druid/coordinator/v1/lookups/config");
        // The lookup configuration has to exist before a lookup is put in it.
        let _ = agent.post(&config).send_json(serde_json::json!({}));
        let lookup = serde_json::json!({"__default": {"post_status": {
            "version": "v1",
            "lookupExtractorFactory": {"type": "map", "map": {"0": "draft", "1": "published"}}
        }}});
        let r = agent.post(&config).send_json(lookup).expect("a lookup");
        assert!(r.status().is_success(), "{}", r.status());
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(180);
    while present("druid", &names) < names.len() || present("lookup", &["post_status"]) == 0 {
        assert!(std::time::Instant::now() < until, "the datasources and the lookup never became queryable");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

// ------------------------------------------------------------------ listing

/// A listing walked page by page names exactly the tables a whole read
/// of the same scope finds, in the same order, and reads nothing below
/// a table. Called from each adapter's own catalog test, so the paging
/// clause is proven on every engine that has one here.
fn pages_agree(src: &mut dyn Source, cat: &Catalog, schema: Option<&str>, size: usize) {
    let f = Filter::schema(schema);
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = Catalog::list(src, &f, size, offset).expect("a page");
        assert!(page.tables.len() <= size, "a page holds at most what was asked for");
        assert!(page.tables.iter().all(|t| t.columns.is_empty() && t.primary_key.is_empty() && t.foreign_keys.is_empty()));
        assert!(!page.more || page.tables.len() == size, "a short page is the last one");
        offset += page.tables.len();
        seen.extend(page.tables.into_iter().map(|t| (t.schema, t.name)));
        if !page.more {
            break;
        }
        assert!(offset <= cat.tables.len(), "the pages never end");
    }
    let whole: Vec<(Option<String>, String)> = cat.tables.iter().map(|t| (t.schema.clone(), t.name.clone())).collect();
    assert_eq!(seen, whole);
}

#[test]
fn a_listing_pages_through_a_file_and_narrows_by_name() {
    let path = duckdb_file("duckdb-listing");
    let conn = Connection { uid: "local".into(), adapter: "duckdb".into(), file: Some(path), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let cat = Catalog::read(&mut *src, "local", None).unwrap();
    for size in [1, 2, 3, 100] {
        pages_agree(&mut *src, &cat, None, size);
    }
    let first = Catalog::list(&mut *src, &Filter::schema(None), 2, 0).unwrap();
    assert!(first.more, "three tables, two asked for");
    assert_eq!(first.tables[0].comment.as_deref(), Some("Who buys"), "what the tables statement reads is kept");
    let hit = Catalog::list(&mut *src, &Filter::like(None, "ORD"), 100, 0).unwrap();
    assert_eq!(hit.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["orders"]);
    assert!(!hit.more);
    assert!(Catalog::list(&mut *src, &Filter::schema(None), 100, 3).unwrap().tables.is_empty(), "past the end is empty, not an error");

    // SQLite has no statement to page, so the page is counted in the read.
    let dir = temp("listing-sqlite");
    let file = dir.join("c.db");
    rusqlite::Connection::open(&file)
        .unwrap()
        .execute_batch("CREATE TABLE a (x); CREATE TABLE b (x); CREATE TABLE ab (x); CREATE VIEW v AS SELECT x FROM a;")
        .unwrap();
    let conn = Connection { uid: "lite".into(), adapter: "sqlite".into(), file: Some(file), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    let cat = Catalog::read(&mut *src, "lite", None).unwrap();
    for size in [1, 3, 4, 100] {
        pages_agree(&mut *src, &cat, None, size);
    }
    let hit = Catalog::list(&mut *src, &Filter::like(None, "a"), 1, 1).unwrap();
    assert_eq!(hit.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["ab"], "the second of a and ab");
    assert!(!hit.more);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_page_is_cut_in_each_engines_own_words() {
    let sql = "SELECT 1 ORDER BY 1, 2";
    let list = |adapter: &str| catalog::paged_for_test(adapter, sql, 101, 200);
    assert_eq!(list("postgres"), "SELECT 1 ORDER BY 1, 2 LIMIT 101 OFFSET 200");
    assert_eq!(list("trino"), "SELECT 1 ORDER BY 1, 2 OFFSET 200 LIMIT 101");
    assert_eq!(list("athena"), list("trino"));
    assert_eq!(list("sqlserver"), "SELECT 1 ORDER BY 1, 2 OFFSET 200 ROWS FETCH NEXT 101 ROWS ONLY");
    assert_eq!(list("druid"), list("postgres"));
}

#[test]
fn a_connection_sent_whole_is_read_as_the_file_is_and_a_catalog_lands_where_the_engine_keeps_one() {
    let sent: serde_yaml::Value = serde_yaml::from_str(
        "host: db.internal\nport: 5432\ndatabase: analytics\nusername: analyst\npassword: ' hunter2 '\nsslmode: require\nsession_token: ''\n",
    )
    .unwrap();
    let mut conn = Connection::from_value("Warehouse", &sent).unwrap();
    conn.adapter = "postgres".into();
    assert_eq!((conn.uid.as_str(), conn.host.as_deref(), conn.port), ("warehouse", Some("db.internal"), Some(5432)));
    assert_eq!(conn.sslmode, SslMode::Require);
    let secrets = Secrets::from_value(&sent);
    assert_eq!(secrets.password.as_ref().map(Secret::expose), Some("hunter2"));
    assert!(secrets.session_token.is_none(), "an empty secret is no secret");
    assert!(!format!("{secrets:?}").contains("hunter2"), "a secret never prints");
    // Every secret a caller may name is one that is read: a field added
    // to the list and not to the reader would be dropped without a word.
    let every: serde_yaml::Value =
        serde_yaml::from_str(&SECRET_FIELDS.iter().map(|f| format!("{f}: s3cret-{f}\n")).collect::<String>()).unwrap();
    let all = Secrets::from_value(&every);
    assert_eq!(format!("{all:?}").matches("Some(").count(), SECRET_FIELDS.len(), "{all:?}");
    assert_eq!(all.oauth_token.as_ref().map(Secret::expose), Some("s3cret-oauth_token"), "a token an upstream obtained");
    assert!(!format!("{all:?}").contains("s3cret"), "and none of them prints");
    for key in sent.as_mapping().unwrap().keys().filter_map(|k| k.as_str()) {
        assert!(CONNECTION_KEYS.contains(&key) || SECRET_FIELDS.contains(&key), "{key} is a key a caller may send");
    }

    conn.in_catalog("other").unwrap();
    assert_eq!(conn.database.as_deref(), Some("other"), "Postgres's catalog is its database");
    for (adapter, at) in
        [("databricks", "catalog"), ("athena", "catalog"), ("bigquery", "project"), ("trino", "database"), ("snowflake", "database")]
    {
        let mut c = Connection { adapter: adapter.into(), ..Default::default() };
        c.in_catalog("x").unwrap();
        let got = match at {
            "catalog" => &c.catalog,
            "project" => &c.project,
            _ => &c.database,
        };
        assert_eq!(got.as_deref(), Some("x"), "{adapter}");
    }
    for adapter in ["mysql", "clickhouse", "druid", "sqlite", "duckdb"] {
        let err = Connection { adapter: adapter.into(), ..Default::default() }.in_catalog("x").unwrap_err().to_string();
        assert!(err.contains("has no catalogs"), "{adapter}: {err}");
    }
}

/// A search means its characters: `hit` finds `expect`, and `miss`,
/// which would match it only if a `_` stood for any character, finds
/// nothing by that name. Called from each engine's own test, since
/// every one spells a literal underscore its own way.
fn a_search_means_its_underscores(src: &mut dyn Source, schema: Option<&str>, hit: &str, expect: &str, miss: &str) {
    let named = |src: &mut dyn Source, pattern: &str| -> Vec<String> {
        Catalog::list(src, &Filter::like(schema, pattern), 100, 0).expect("a search").tables.into_iter().map(|t| t.name).collect()
    };
    let found = named(src, hit);
    assert!(found.iter().any(|n| n.eq_ignore_ascii_case(expect)), "{hit:?} should find {expect}: {found:?}");
    let found = named(src, miss);
    assert!(!found.iter().any(|n| n.eq_ignore_ascii_case(expect)), "{miss:?} should not find {expect}: {found:?}");
    // The whole read takes the same filter.
    let read = Catalog::read_filtered(src, "x", &Filter::like(schema, hit)).expect("a filtered read");
    assert!(read.tables.iter().any(|t| t.name.eq_ignore_ascii_case(expect) && !t.columns.is_empty()), "{hit:?} read whole");
}

#[test]
fn a_search_is_spelled_for_the_engine_it_runs_on() {
    let like = |adapter: &str, pattern: &str| catalog::holding_for_test(adapter, pattern);
    assert_eq!(like("postgres", "CS_Contact"), "LOWER(t) LIKE '%cs!_contact%' ESCAPE '!'");
    assert_eq!(like("duckdb", "50%_o'k!"), "LOWER(t) LIKE '%50!%!_o''k!!%' ESCAPE '!'");
    assert_eq!(like("sqlserver", "a[b]_c"), "LOWER(t) LIKE '%a![b]!_c%' ESCAPE '!'");
    assert_eq!(like("clickhouse", r"cs_con\tact%"), r"LOWER(t) LIKE '%cs\_contact\%%'");
    assert_eq!(like("bigquery", "cs_contact"), r"LOWER(t) LIKE '%cs\\_contact%'");
    for adapter in ["redshift", "mysql", "trino", "athena", "snowflake", "databricks"] {
        assert_eq!(like(adapter, "a_b"), like("postgres", "a_b"), "{adapter}");
    }

    let path = duckdb_file("duckdb-search");
    ::duckdb::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE cs_contact_f (id INTEGER); CREATE TABLE csxcontact (id INTEGER);")
        .unwrap();
    let conn = Connection { uid: "local".into(), adapter: "duckdb".into(), file: Some(path), ..Default::default() };
    let mut src = open(&conn, Secrets::default()).unwrap();
    a_search_means_its_underscores(&mut *src, None, "CS_CONTACT", "cs_contact_f", "cs__ontact_f");
    let both = Catalog::list(&mut *src, &Filter::like(None, "contact"), 100, 0).unwrap();
    assert_eq!(both.tables.len(), 2);
    let one = Catalog::list(&mut *src, &Filter::like(None, "cs_contact"), 100, 0).unwrap();
    assert_eq!(one.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cs_contact_f"], "csxcontact is not cs_contact");
}

#[test]
fn one_statement_is_told_from_several() {
    for one in [
        "SELECT 1",
        "SELECT 1;",
        "  INSERT INTO t VALUES ('a;b')  ;\n",
        "SELECT 1 -- ; not this\n",
        "SELECT /* ; */ 1",
        "UPDATE t SET a = \"x;y\"",
    ] {
        assert!(single_statement(one), "{one}");
    }
    for several in ["SELECT 1; SELECT 2", "SELECT 1;; DROP TABLE t", "", "  ;  ", "-- only a comment"] {
        assert!(!single_statement(several), "{several:?}");
    }
}

/// A listing says a credential is at hand only where the adapter would
/// find one: a secret some other adapter takes is not this one's.
#[test]
fn a_credential_is_at_hand_only_when_the_adapter_takes_it() {
    let of = |adapter: &str| Connection { uid: "d".into(), adapter: adapter.into(), ..Default::default() };
    let secret = |s: &str| Some(Secret(s.into()));
    let key = Secrets { private_key: secret("-----BEGIN PRIVATE KEY-----"), ..Default::default() };
    let password = Secrets::password(secret("pw"));
    assert_eq!(credentials_at_hand(&of("duckdb"), &password), None);
    assert_eq!(credentials_at_hand(&of("postgres"), &password), Some(true));
    assert_eq!(credentials_at_hand(&of("postgres"), &key), Some(false));
    assert_eq!(credentials_at_hand(&of("snowflake"), &key), Some(true));
    assert_eq!(credentials_at_hand(&of("snowflake"), &password), Some(false));
    assert_eq!(credentials_at_hand(&of("databricks"), &Secrets { oauth_client_secret: secret("s"), ..Default::default() }), Some(true));
    let token = Secrets { oauth_token: secret("t"), ..Default::default() };
    assert_eq!((credentials_at_hand(&of("trino"), &token), credentials_at_hand(&of("mysql"), &token)), (Some(true), Some(false)));
    assert_eq!(credentials_at_hand(&of("bigquery"), &token), Some(true));
    // A Druid cluster without an authenticator takes nothing; naming a
    // user says it has one, and then the password or a token is wanted.
    assert_eq!(credentials_at_hand(&of("druid"), &Secrets::default()), Some(true));
    let named = Connection { username: Some("ada".into()), ..of("druid") };
    assert_eq!(credentials_at_hand(&named, &Secrets::default()), Some(false));
    assert_eq!((credentials_at_hand(&named, &password), credentials_at_hand(&named, &token)), (Some(true), Some(true)));
}
