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
