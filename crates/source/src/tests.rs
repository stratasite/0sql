//! The parts of reaching a datasource that can be tested without one: the
//! connection a project declares, where a secret comes from, and what
//! counts as a read. The transports themselves need a server, and phase
//! B2's containers are where they get one.

use super::*;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zsql-source-{name}-{}", std::process::id()));
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
    assert_eq!(wh.adapter, "postgres", "the adapter is lower-cased, as the dialect catalogue spells it");
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
    let conn = Connection { uid: "wh".into(), adapter: "snowflake".into(), ..Default::default() };
    let err = open(&conn, None).err().unwrap().to_string();
    assert!(err.contains("snowflake is not an adapter this build can reach"), "{err}");
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
}

/// The transport against a real server. Ignored, because it needs one:
/// `ZSQL_TEST_PG=host=… user=… password=… dbname=…` style settings as
/// separate variables, then `cargo test -p zsql-source -- --ignored`.
/// Phase B2 replaces this with a container CI runs for itself.
#[test]
#[ignore]
fn a_real_postgres_answers_a_read_and_refuses_a_write() {
    let conn = Connection {
        uid: "test".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD");
    let mut src = postgres::Postgres::open(&conn, Some(Secret(password))).expect("connects");

    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three"]);
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);

    assert!(src.query("DROP TABLE IF EXISTS zsql_should_not_exist", 1).is_err(), "a write is refused before it is sent");

    let err = src.query("SELECT * FROM generate_series(1, 100)", 10).unwrap_err().to_string();
    assert!(err.contains("more than the 10 rows"), "the cap refuses rather than truncates: {err}");
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
    let mut src = open(&conn, None).expect("opens");
    assert_eq!(src.adapter(), "sqlite");

    let rows = src.query("SELECT id, name, note FROM t ORDER BY id", 10).unwrap();
    assert_eq!(rows.columns, ["id", "name", "note"]);
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.get(&rows.rows[0], "name"), Some("a"));
    assert_eq!(rows.get(&rows.rows[0], "note"), None, "null is none, not the word null");
    assert_eq!(rows.get(&rows.rows[1], "id"), Some("2"), "numbers come back as text");

    let err = src.query("SELECT id FROM t", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "the cap refuses rather than trims: {err}");

    // Refused by the guard, and if it ever were not, by the file.
    assert!(src.query("DELETE FROM t", 1).is_err());
    let missing = Connection { uid: "none".into(), adapter: "sqlite".into(), ..Default::default() };
    let err = open(&missing, None).err().unwrap().to_string();
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
    let mut src = open(&conn, secret).expect("opens");

    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three, 1.5 AS four", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three", "four"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert_eq!(rows.get(&rows.rows[0], "four"), Some("1.5"));

    // Past the cap the server throws, which is what result_overflow_mode asks.
    let err = src.query("SELECT number FROM system.numbers LIMIT 100", 10).err().unwrap().to_string();
    assert!(err.contains("ch"), "named: {err}");

    // The guard refuses first; the server's readonly mode would refuse second.
    assert!(src.query("CREATE TABLE zsql_no (a UInt8) ENGINE = Memory", 1).is_err());
}

#[test]
fn a_sqlite_catalogue_has_its_tables_views_and_columns_in_order() {
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
    let mut src = open(&conn, None).unwrap();
    let cat = Catalogue::read(src.as_mut(), "local", None).unwrap();

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
    let back: Catalogue = serde_json::from_str(&json).unwrap();
    assert_eq!(back.tables.len(), 2, "the file round-trips");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Postgres catalogue, scoped to `pg_catalog` because that schema is
/// in every server and needs no fixture written: its tables have a row
/// estimate, typed columns in order, and the odd comment.
#[test]
#[ignore]
fn a_real_postgres_catalogue_reads_pg_catalog() {
    let conn = Connection {
        uid: "test".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let password = std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD");
    let mut src = open(&conn, Some(Secret(password))).expect("connects");
    let cat = Catalogue::read(src.as_mut(), "test", Some("pg_catalog")).expect("reads");

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

/// The ClickHouse catalogue, scoped to the playground's `blogs` database
/// by default: real tables with row counts from `system.tables`, and
/// ClickHouse's own type spellings kept as they are.
#[test]
#[ignore]
fn a_real_clickhouse_catalogue_reads_a_database() {
    let conn = Connection {
        uid: "ch".into(),
        adapter: "clickhouse".into(),
        url: Some(std::env::var("ZSQL_TEST_CLICKHOUSE_URL").expect("ZSQL_TEST_CLICKHOUSE_URL")),
        username: std::env::var("ZSQL_TEST_CLICKHOUSE_USER").ok(),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let secret = std::env::var("ZSQL_TEST_CLICKHOUSE_PASSWORD").ok().map(Secret);
    let mut src = open(&conn, secret).expect("opens");
    // The playground has `blogs`; a container has `default`, empty but
    // real. Either way the read works and the scope holds.
    let scope = std::env::var("ZSQL_TEST_CLICKHOUSE_DB").unwrap_or_else(|_| "blogs".into());
    let cat = Catalogue::read(src.as_mut(), "ch", Some(&scope)).expect("reads");
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
    let all = Catalogue::read(src.as_mut(), "ch", None).expect("reads");
    assert!(all
        .tables
        .iter()
        .all(|t| !["system", "INFORMATION_SCHEMA", "information_schema"].contains(&t.schema.as_deref().unwrap_or(""))));
}

/// MySQL against a server the environment names, as CI's container does:
/// `ZSQL_TEST_MYSQL_HOST`, `_USER`, `_PASSWORD`, `_DB`.
#[test]
#[ignore]
fn a_real_mysql_answers_and_its_catalogue_reads() {
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
    let mut src = open(&conn, Some(Secret(password))).expect("connects");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three, 1.5 AS four", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three", "four"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert!(src.query("CREATE TABLE zsql_no (a INT)", 1).is_err());
    let err = src.query("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    // The container makes itself a self-signed certificate, which is
    // exactly what `require` accepts and `verify-full` refuses.
    let tls = |mode: SslMode| Connection { sslmode: mode, ..conn.clone() };
    let pw = || Some(Secret(std::env::var("ZSQL_TEST_MYSQL_PASSWORD").unwrap()));
    assert!(open(&tls(SslMode::Require), pw()).is_ok(), "require accepts a certificate signed by nobody");
    assert!(open(&tls(SslMode::VerifyFull), pw()).is_err(), "verify-full refuses a self-signed certificate");
    let p = open(&tls(SslMode::Prefer), pw()).expect("prefer connects");
    assert!(p.warnings().is_empty(), "TLS was on offer, so nothing to warn about: {:?}", p.warnings());
    // The catalogue of mysql's own schema: always there, never written to.
    let cat = Catalogue::read(src.as_mut(), "my", Some("mysql")).expect("reads");
    let user = cat.table(Some("mysql"), "user").expect("mysql.user is in every server");
    assert!(user.columns.iter().any(|c| c.name == "Host"), "typed columns");
    assert!(user.columns.windows(2).all(|w| w[0].position < w[1].position));
    assert!(user.columns.iter().any(|c| c.max_length.is_some()), "character_maximum_length is read");
}

/// SQL Server against a server the environment names: `ZSQL_TEST_MSSQL_HOST`,
/// `_USER`, `_PASSWORD`, `_DB`. Encryption off, as the transport is built.
#[test]
#[ignore]
fn a_real_sqlserver_answers_and_its_catalogue_reads() {
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
    let mut src = open(&conn, Some(Secret(password))).expect("connects");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "two"), Some("two"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    assert!(src.query("CREATE TABLE zsql_no (a INT)", 1).is_err());
    // TOP, since this is the dialect with no LIMIT, and the cap still refuses.
    let err = src.query("SELECT TOP (5) object_id FROM sys.objects", 2).err().unwrap().to_string();
    assert!(err.contains("more than the 2 rows"), "{err}");
    // SQL Server also makes itself a self-signed certificate.
    let tls = |mode: SslMode| Connection { sslmode: mode, ..conn.clone() };
    let pw = || Some(Secret(std::env::var("ZSQL_TEST_MSSQL_PASSWORD").unwrap()));
    assert!(open(&tls(SslMode::Require), pw()).is_ok(), "require accepts a certificate signed by nobody");
    assert!(open(&tls(SslMode::VerifyFull), pw()).is_err(), "verify-full refuses a self-signed certificate");
    let p = open(&tls(SslMode::Prefer), pw()).expect("prefer connects");
    assert!(p.warnings().is_empty(), "TLS was on offer, so nothing to warn about: {:?}", p.warnings());
    // sys.objects lists user objects, so the catalogue views themselves
    // are not in it, which is right for a model. master.dbo always holds
    // the spt_ tables setup leaves behind, and spt_values is a view.
    let cat = Catalogue::read(src.as_mut(), "ms", Some("dbo")).expect("reads");
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
fn a_real_trino_answers_follows_next_uri_and_its_catalogue_reads() {
    let conn = Connection {
        uid: "tr".into(),
        adapter: "trino".into(),
        url: Some(std::env::var("ZSQL_TEST_TRINO_URL").expect("ZSQL_TEST_TRINO_URL")),
        database: std::env::var("ZSQL_TEST_TRINO_DB").ok().or(Some("tpch".into())),
        username: Some("zsql".into()),
        sslmode: SslMode::Disable,
        ..Default::default()
    };
    let mut src = open(&conn, None).expect("opens");
    let rows = src.query("SELECT 1 AS one, 'two' AS two, NULL AS three", 10).expect("reads");
    assert_eq!(rows.columns, ["one", "two", "three"]);
    assert_eq!(rows.get(&rows.rows[0], "one"), Some("1"));
    assert_eq!(rows.get(&rows.rows[0], "three"), None);
    // tiny.nation is 25 rows and arrives across pages; the cap cancels and refuses.
    let err = src.query("SELECT name FROM tpch.tiny.nation", 10).err().unwrap().to_string();
    assert!(err.contains("more than the 10 rows"), "{err}");
    let all = src.query("SELECT name FROM tpch.tiny.nation", 100).expect("reads across pages");
    assert_eq!(all.rows.len(), 25);
    assert!(src.query("CREATE TABLE memory.default.zsql_no (a INT)", 1).is_err());
    // The catalog is a header, so information_schema alone means tpch's.
    let cat = Catalogue::read(src.as_mut(), "tr", Some("tiny")).expect("reads");
    let nation = cat.table(Some("tiny"), "nation").expect("tpch.tiny.nation");
    assert_eq!(nation.kind, "table");
    let name = nation.columns.iter().find(|c| c.name == "name").expect("name");
    assert!(name.data_type.starts_with("varchar"), "{}", name.data_type);
    assert!(nation.columns.windows(2).all(|w| w[0].position < w[1].position));
}

fn test_cert() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../harness/sources/tls/server.crt")
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
    let err = open(&conn(SslMode::VerifyFull, "http://ch.internal:8123"), None).err().unwrap().to_string();
    assert!(err.contains("requires TLS") && err.contains("plain http"), "{err}");
    let err = open(&conn(SslMode::Require, "HTTP://ch.internal:8123"), None).err().unwrap().to_string();
    assert!(err.contains("sslmode: disable"), "it says the way out: {err}");
    // disable allows it and says nothing; prefer allows it and says so.
    // (Neither reaches a server: ClickHouse's open probes, so they fail
    // on connect, which is past the rule being tested.)
    let e = open(&conn(SslMode::Disable, "http://127.0.0.1:1"), None).err().unwrap().to_string();
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
    if std::env::var("ZSQL_TEST_PG_TLS").is_err() {
        eprintln!("ZSQL_TEST_PG_TLS unset; skipping");
        return;
    }
    let base = Connection {
        uid: "pg".into(),
        adapter: "postgres".into(),
        host: std::env::var("ZSQL_TEST_PG_HOST").ok().or(Some("localhost".into())),
        port: std::env::var("ZSQL_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: std::env::var("ZSQL_TEST_PG_DB").ok().or(Some("postgres".into())),
        username: std::env::var("ZSQL_TEST_PG_USER").ok().or(Some("postgres".into())),
        ..Default::default()
    };
    let secret = || Some(Secret(std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD")));
    let ca = std::env::var("ZSQL_TEST_PG_CA").map(PathBuf::from).unwrap_or_else(|_| test_cert());
    let with = |mode: SslMode, ca_file: Option<PathBuf>| Connection { sslmode: mode, ca_file, ..base.clone() };
    let ssl_of = |src: &mut Box<dyn Source>| {
        let r = src.query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", 1).unwrap();
        r.rows[0][0].clone()
    };

    let mut s = open(&with(SslMode::Disable, None), secret()).expect("disable connects");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("f"));
    let mut s = open(&with(SslMode::Prefer, None), secret()).expect("prefer connects");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"), "the server offers TLS, so prefer takes it");
    assert!(s.warnings().is_empty(), "nothing to warn about: {:?}", s.warnings());
    let mut s = open(&with(SslMode::Require, None), secret()).expect("require accepts a certificate signed by nobody");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
    let err = open(&with(SslMode::VerifyFull, None), secret()).err().expect("verify-full refuses a self-signed certificate").to_string();
    assert!(err.contains("certificate") || err.contains("Certificate") || err.contains("tls") || err.contains("TLS"), "{err}");
    let mut s = open(&with(SslMode::VerifyFull, Some(ca.clone())), secret()).expect("verify-full with the pair as the CA");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
    let mut s = open(&with(SslMode::VerifyCa, Some(ca)), secret()).expect("verify-ca with the pair as the CA");
    assert_eq!(ssl_of(&mut s).as_deref(), Some("t"));
}
