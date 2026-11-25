//! ADBC, for the warehouses whose own driver is better than their REST
//! API: Snowflake and BigQuery. Their REST endpoints answer in JSON
//! strings, a page at a time; the drivers their vendors' clients are
//! built on answer in Arrow, in parallel streams, with every column's
//! type. So a result somebody keeps comes through the driver, typed.
//!
//! A driver is a shared library, found where `dbc install <name>` puts
//! it: the manifest directories every ADBC driver manager searches
//! (`ADBC_DRIVER_PATH`, `~/.config/adbc/drivers`, `/etc/adbc/drivers`).
//! Nothing is chosen in `datasources.yml`. When the driver is there it
//! is used; when it is not, the REST transport answers, as text. A
//! static musl build cannot load a library at all, so this module is
//! not compiled into one. No release is such a build any more; one
//! made by hand still works, over REST.
//!
//! Text reads, the catalog and sampled values, stay on REST whether or
//! not a driver is installed. A sampled value is a string a consumer
//! matches against, and it must not change spelling with what happens
//! to be installed on the machine that sampled it.
//!
//! `ZSQL_ADBC=off` keeps every read on REST, for telling the two apart
//! when one of them misbehaves.

use crate::batch::Batches;
use crate::{Access, Connection, Error, Result, Rows, Secrets, Sink, Source};
use adbc_core::error::Status;
use adbc_core::options::{AdbcVersion, OptionDatabase, OptionValue};
use adbc_core::{Connection as _, Database as _, Driver as _, Statement as _};
use adbc_core::{LOAD_FLAG_SEARCH_ENV, LOAD_FLAG_SEARCH_SYSTEM, LOAD_FLAG_SEARCH_USER};
use adbc_driver_manager::{ManagedConnection, ManagedDatabase, ManagedDriver};
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::SchemaRef;
use std::time::Duration;

/// What to open: a driver by its registry name and the options it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub driver: &'static str,
    pub options: Vec<(String, String)>,
    /// The warehouse's decimals carry a fixed scale that says nothing
    /// about the column (BigQuery's NUMERIC is always nine places), so
    /// a value prints without its trailing zeros, as the warehouse's own
    /// REST API spells it.
    pub scale_is_nominal: bool,
    /// How far the connection is trusted with a statement.
    pub access: Access,
}

/// Whether drivers are looked for at all.
pub(crate) fn enabled() -> bool {
    !matches!(std::env::var("ZSQL_ADBC").ok().as_deref().map(str::trim), Some("off" | "0" | "false" | "no"))
}

/// The driver of this name, or `None` when none is installed. A driver
/// that is installed and will not load is an error: falling back then
/// would hide a broken install behind a slower answer.
///
/// The search never takes a relative path, so a project directory
/// cannot supply the library that is handed the warehouse's credentials.
pub fn load(name: &str) -> Result<Option<ManagedDriver>> {
    let flags = LOAD_FLAG_SEARCH_ENV | LOAD_FLAG_SEARCH_USER | LOAD_FLAG_SEARCH_SYSTEM;
    match ManagedDriver::load_from_name(name, None, AdbcVersion::V110, flags, None) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.status == Status::NotFound => Ok(None),
        Err(e) => Err(Error::Connect(format!("the ADBC driver {name}"), e.message)),
    }
}

/// One connection through a loaded driver.
pub struct Adbc {
    connection: ManagedConnection,
    // Dropped after the connection that came from it.
    _database: ManagedDatabase,
    uid: String,
    timeout: Duration,
    scale_is_nominal: bool,
    access: Access,
}

impl Adbc {
    pub fn open(mut driver: ManagedDriver, plan: &Plan, uid: &str, timeout: Duration) -> Result<Adbc> {
        let fail = |e: adbc_core::error::Error| Error::Connect(uid.to_string(), e.message);
        let options = plan.options.iter().map(|(k, v)| (OptionDatabase::from(k.as_str()), OptionValue::String(v.clone())));
        let database = driver.new_database_with_opts(options).map_err(fail)?;
        let connection = database.new_connection().map_err(fail)?;
        Ok(Adbc {
            connection,
            _database: database,
            uid: uid.to_string(),
            timeout,
            scale_is_nominal: plan.scale_is_nominal,
            access: plan.access,
        })
    }

    /// One read, under the row cap and the statement timeout.
    pub fn batches(&mut self, sql: &str, max_rows: usize) -> Result<Batches> {
        let mut kept = Kept { uid: self.uid.clone(), max_rows, rows: 0, got: Batches::empty() };
        self.stream(sql, &mut kept)?;
        Ok(kept.got)
    }

    /// One read handed to `sink` a batch at a time, under the statement
    /// timeout.
    ///
    /// The statement runs on a thread of its own, which hands over each
    /// batch as the driver produces it and waits for it to be taken
    /// before reading the next, so nothing piles up between the two. The
    /// caller waits no longer than the timeout for the whole read. Past
    /// it, or when the sink has had enough, the statement is cancelled
    /// through the driver's handle. A driver is free not to implement
    /// that, and the SQLite one does not, so the answer never depends on
    /// it: the caller is told either way, and told which. A statement
    /// that could not be cancelled is abandoned, still running wherever
    /// it runs, which is what the error says.
    pub fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        self.access.check(sql)?;
        let uid = self.uid.clone();
        let fail = |e: adbc_core::error::Error| Error::Query(uid.clone(), e.message);
        let mut statement = self.connection.new_statement().map_err(fail)?;
        statement.set_sql_query(sql).map_err(fail)?;
        let cancel = statement.get_cancel_handle();
        // Room for one message: the worker reads no further ahead than
        // the batch that is waiting to be taken.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Message>(1);
        let worker_uid = uid.clone();
        std::thread::spawn(move || {
            let outcome = (|| {
                let failed = |why: String| Error::Query(worker_uid.clone(), why);
                let reader = statement.execute().map_err(|e| failed(e.message))?;
                // A send that fails means nobody is listening any more.
                if tx.send(Message::Schema(reader.schema())).is_err() {
                    return Ok(());
                }
                for batch in reader {
                    if tx.send(Message::Batch(batch.map_err(|e| failed(e.to_string()))?)).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            })();
            let _ = tx.send(Message::Done(outcome));
        });

        let deadline = std::time::Instant::now() + self.timeout;
        let mut schema: Option<SchemaRef> = None;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(Message::Done(outcome)) => return outcome,
                Ok(Message::Schema(arrived)) => {
                    // A warehouse whose scale is nominal has its decimal
                    // columns marked, in the schema every batch carries.
                    let marked = if self.scale_is_nominal {
                        Batches { schema: arrived, batches: Vec::new() }.printing_decimals_trimmed().schema
                    } else {
                        arrived
                    };
                    let taken = sink.schema(&marked);
                    schema = Some(marked);
                    if let Err(e) = taken {
                        let _ = cancel.try_cancel();
                        return Err(e);
                    }
                }
                Ok(Message::Batch(batch)) => {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let taken = match &schema {
                        Some(marked) => batch
                            .with_schema(marked.clone())
                            .map_err(|e| Error::Query(uid.clone(), e.to_string()))
                            .and_then(|b| sink.batch(b)),
                        None => Err(Error::Query(uid.clone(), "the driver sent rows before their schema".into())),
                    };
                    if let Err(e) = taken {
                        // Whoever takes the rows has had enough: stop the
                        // statement, and stop listening, which stops the
                        // worker at its next batch.
                        let _ = cancel.try_cancel();
                        return Err(e);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::Query(uid, "the driver stopped without an answer".into()));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let secs = self.timeout.as_secs();
                    let stopped = || {
                        let until = std::time::Instant::now() + CANCEL_GRACE;
                        loop {
                            match rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
                                Ok(Message::Done(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return true,
                                Ok(_) => continue,
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return false,
                            }
                        }
                    };
                    let why = match cancel.try_cancel() {
                        // A cancelled statement returns promptly; wait for it
                        // so the connection is free for the next one.
                        Ok(()) if stopped() => format!("no answer within {secs}s; cancelled (query_timeout)"),
                        Ok(()) => {
                            format!("no answer within {secs}s; cancellation was sent and the statement has not stopped (query_timeout)")
                        }
                        // Some drivers refuse with a status and no words.
                        Err(e) => format!(
                            "no answer within {secs}s, and the driver could not cancel it ({}); it may still be running (query_timeout)",
                            if e.message.trim().is_empty() { format!("{:?}", e.status) } else { e.message }
                        ),
                    };
                    return Err(Error::Query(uid, why));
                }
            }
        }
    }
}

/// What the statement's thread hands over.
enum Message {
    Schema(SchemaRef),
    Batch(RecordBatch),
    Done(Result<()>),
}

/// A read that keeps its batches, and refuses past the cap.
struct Kept {
    uid: String,
    max_rows: usize,
    rows: usize,
    got: Batches,
}

impl Sink for Kept {
    fn schema(&mut self, schema: &SchemaRef) -> Result<()> {
        self.got.schema = schema.clone();
        Ok(())
    }

    fn batch(&mut self, batch: RecordBatch) -> Result<()> {
        self.rows += batch.num_rows();
        if self.rows > self.max_rows {
            return Err(Error::TooManyRows(self.uid.clone(), self.max_rows));
        }
        self.got.batches.push(batch);
        Ok(())
    }
}

/// A sink that says nothing until there is something to say: the schema
/// is passed on with the first batch, or at the end of a result with no
/// rows. A read that fails before then has told the real sink nothing,
/// so another transport can still answer in its place.
struct Deferred<'a> {
    sink: &'a mut dyn Sink,
    schema: Option<SchemaRef>,
    spoken: bool,
}

impl Deferred<'_> {
    fn speak(&mut self) -> Result<()> {
        if !self.spoken {
            self.spoken = true;
            if let Some(schema) = &self.schema {
                self.sink.schema(schema)?;
            }
        }
        Ok(())
    }
}

impl Sink for Deferred<'_> {
    fn schema(&mut self, schema: &SchemaRef) -> Result<()> {
        self.schema = Some(schema.clone());
        Ok(())
    }

    fn batch(&mut self, batch: RecordBatch) -> Result<()> {
        self.speak()?;
        self.sink.batch(batch)
    }
}

/// How long a cancelled statement is given to come back.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// A warehouse with two ways in: its REST transport for text, and its
/// ADBC driver for a typed result when one is installed.
pub struct WithAdbc {
    rest: Box<dyn Source>,
    plan: Plan,
    uid: String,
    timeout: Duration,
    state: State,
    /// Set once the driver has been given up on: the REST transport's
    /// warnings with the reason in front.
    gave_up: Option<Vec<String>>,
}

/// What the BigQuery driver's reader needs and its REST API does not:
/// the Storage Read API, by the permission Google names it with. An
/// account that may run a query may well not hold it, and such a
/// datasource answered before the driver was installed.
const NO_READ_SESSION: &str = "bigquery.readsessions.create";

enum State {
    /// Nobody has asked for batches yet, so nothing has been loaded: a
    /// catalog read never pays for a sixty-megabyte library.
    Unopened,
    Open(Box<Adbc>),
    /// No driver is installed, drivers are switched off, or this
    /// account cannot use the one there is.
    Absent,
}

impl WithAdbc {
    pub fn new(rest: Box<dyn Source>, plan: Plan, conn: &Connection) -> WithAdbc {
        WithAdbc { rest, plan, uid: conn.uid.clone(), timeout: conn.timeout(), state: State::Unopened, gave_up: None }
    }

    fn driver(&mut self) -> Result<Option<&mut Adbc>> {
        if matches!(self.state, State::Unopened) {
            self.state = match if enabled() { load(self.plan.driver)? } else { None } {
                Some(driver) => {
                    let open = Adbc::open(driver, &self.plan, &self.uid, self.timeout)?;
                    tracing::debug!(datasource = %self.uid, driver = self.plan.driver, "the ADBC driver was loaded; typed results come through it");
                    State::Open(Box::new(open))
                }
                None => {
                    tracing::debug!(
                        datasource = %self.uid,
                        driver = self.plan.driver,
                        switched_off = !enabled(),
                        "no ADBC driver is used; results are read over REST, as text"
                    );
                    State::Absent
                }
            };
        }
        Ok(match &mut self.state {
            State::Open(a) => Some(a),
            _ => None,
        })
    }
}

impl WithAdbc {
    /// The account cannot open a read session, so the driver is not
    /// used again on this connection: said in the log and in the
    /// connection's warnings.
    fn give_up_on_the_driver(&mut self) {
        tracing::warn!(
            datasource = %self.uid,
            driver = self.plan.driver,
            missing_permission = NO_READ_SESSION,
            role = "roles/bigquery.readSessionUser",
            "the ADBC driver was given up on for this connection: the account cannot open a read session; results are read over REST, as text, and this statement ran twice"
        );
        self.state = State::Absent;
        self.gave_up = Some(vec![format!(
            "{}: the ADBC driver was not used: this account lacks {NO_READ_SESSION} (the role roles/bigquery.readSessionUser), so results are read over REST, as text",
            self.uid
        )]);
    }

    /// The reason the driver was given up on, then whatever the REST
    /// transport has to say.
    fn keep_rest_warnings(&mut self) {
        if let Some(notes) = &mut self.gave_up {
            notes.truncate(1);
            notes.extend_from_slice(self.rest.warnings());
        }
    }
}

impl Source for WithAdbc {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        self.rest.query(sql, max_rows)
    }

    fn batches(&mut self, sql: &str, max_rows: usize) -> Result<Batches> {
        if let Some(adbc) = self.driver()? {
            match adbc.batches(sql, max_rows) {
                // The statement ran and only its result could not be
                // fetched, so REST is asked instead, for this statement
                // and every later one on the connection.
                Err(Error::Query(_, why)) if why.contains(NO_READ_SESSION) => self.give_up_on_the_driver(),
                other => return other,
            }
        }
        let got = self.rest.batches(sql, max_rows);
        self.keep_rest_warnings();
        got
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        if let Some(adbc) = self.driver()? {
            let mut deferred = Deferred { sink: &mut *sink, schema: None, spoken: false };
            match adbc.stream(sql, &mut deferred) {
                Ok(()) => return deferred.speak(),
                // As for a whole read: REST answers instead, which it can
                // only when the sink has been told nothing yet.
                Err(Error::Query(_, why)) if why.contains(NO_READ_SESSION) && !deferred.spoken => self.give_up_on_the_driver(),
                Err(e) => return Err(e),
            }
        }
        let done = self.rest.stream(sql, sink);
        self.keep_rest_warnings();
        done
    }

    fn adapter(&self) -> &str {
        self.rest.adapter()
    }

    fn transport(&self) -> &str {
        match self.state {
            State::Open(_) => "adbc",
            _ => self.rest.transport(),
        }
    }

    fn warnings(&self) -> &[String] {
        self.gave_up.as_deref().unwrap_or_else(|| self.rest.warnings())
    }

    fn connection(&self) -> Option<&Connection> {
        self.rest.connection()
    }
}

// ------------------------------------------------------------- snowflake

const SF: &str = "adbc.snowflake.sql.";
const SF_CLIENT: &str = "adbc.snowflake.sql.client_option.";

/// The options the Snowflake driver is opened with. The credentials are
/// the REST transport's, in its order, so one datasource answers the
/// same way to the same secrets whichever transport runs: an OAuth
/// token, a programmatic access token, a key pair.
pub fn snowflake(conn: &Connection, secrets: &Secrets) -> Plan {
    let mut o: Vec<(String, String)> = Vec::new();
    let mut set = |key: String, value: &str| o.push((key, value.to_string()));
    if let Some(account) = conn.account.as_deref().or_else(|| conn.url.as_deref().and_then(host_label)) {
        set(format!("{SF}account"), account.trim());
    }
    // A url stands in for the account's own host: a private link, a proxy.
    if let Some((protocol, host, port)) = conn.url.as_deref().and_then(split_url) {
        set(format!("{SF}uri.protocol"), protocol);
        set(format!("{SF}uri.host"), host);
        if let Some(port) = port {
            set(format!("{SF}uri.port"), port);
        }
    }
    for (key, value) in [("db", &conn.database), ("schema", &conn.schema), ("warehouse", &conn.warehouse), ("role", &conn.role)] {
        if let Some(v) = value {
            set(format!("{SF}{key}"), &crate::adapters::snowflake::ident(v));
        }
    }
    if let Some(user) = conn.username.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        set("username".into(), user);
    }
    if let Some(token) = &secrets.oauth_token {
        set(format!("{SF}auth_type"), "auth_oauth");
        set(format!("{SF_CLIENT}auth_token"), token.expose());
    } else if let Some(token) = &secrets.personal_access_token {
        set(format!("{SF}auth_type"), "auth_pat");
        set(format!("{SF_CLIENT}auth_token"), token.expose());
    } else {
        // A key pair. The driver reads the file itself, or takes the PEM.
        match (&secrets.private_key, &conn.private_key_file) {
            (Some(s), _) if s.expose().contains("-----BEGIN") => {
                set(format!("{SF}auth_type"), "auth_jwt");
                set(format!("{SF_CLIENT}jwt_private_key_pkcs8_value"), s.expose());
            }
            (Some(s), _) => {
                set(format!("{SF}auth_type"), "auth_jwt");
                set(format!("{SF_CLIENT}jwt_private_key"), s.expose());
            }
            (None, Some(path)) => {
                set(format!("{SF}auth_type"), "auth_jwt");
                set(format!("{SF_CLIENT}jwt_private_key"), &path.display().to_string());
            }
            (None, None) => {}
        }
    }
    let secs = format!("{}s", conn.timeout().as_secs());
    set(format!("{SF_CLIENT}login_timeout"), &secs);
    set(format!("{SF_CLIENT}request_timeout"), &secs);
    set(format!("{SF_CLIENT}app_name"), "zsql");
    Plan { driver: "snowflake", options: o, scale_is_nominal: false, access: conn.access }
}

/// `https://host:8443/path` as its scheme, host and port.
fn split_url(url: &str) -> Option<(&str, &str, Option<&str>)> {
    let (protocol, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().filter(|a| !a.is_empty())?;
    Some(match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => (protocol, host, Some(port)),
        _ => (protocol, authority, None),
    })
}

/// The first label of a url's host, for a `url` that stands in for `account`.
fn host_label(url: &str) -> Option<&str> {
    split_url(url)?.1.split('.').next().filter(|s| !s.is_empty())
}

// -------------------------------------------------------------- bigquery

/// The options the BigQuery driver is opened with, or `None` when REST
/// is the only way in: the driver takes a credentials file or a refresh
/// token and mints its own access tokens, and is given neither a token
/// somebody else already obtained nor a key held in `.zsql`, which the
/// REST transport signs with itself.
pub fn bigquery(conn: &Connection, secrets: &Secrets, env: &dyn Fn(&str) -> Option<String>) -> Option<Plan> {
    if secrets.oauth_token.is_some() || crate::adapters::bigquery::has_inline(conn, secrets) {
        return None;
    }
    let mut o: Vec<(String, String)> = Vec::new();
    let mut set = |key: &str, value: &str| o.push((key.to_string(), value.to_string()));
    set("bigquery.project_id", conn.project.as_deref()?);
    if let Some(dataset) = &conn.schema {
        set("bigquery.dataset_id", dataset);
    }
    if let Some(location) = &conn.location {
        set("bigquery.location", location);
    }
    // The file the REST transport would read, in its order; with none
    // named, the driver walks the application-default chain itself.
    let file = conn
        .key_file
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| env("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.trim().is_empty()));
    match file {
        Some(path) => {
            set("bigquery.auth_type", "json_credential_file");
            set("bigquery.auth.credentials", &path);
        }
        None => set("bigquery.auth_type", "app_default_credentials"),
    }
    if let Some(url) = &conn.url {
        set("bigquery.endpoint", url);
    }
    Some(Plan { driver: "bigquery", options: o, scale_is_nominal: true, access: conn.access })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Secret;

    fn get<'a>(plan: &'a Plan, key: &str) -> Option<&'a str> {
        plan.options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn secret(s: &str) -> Option<Secret> {
        Some(Secret::new(s))
    }

    #[test]
    fn snowflake_takes_the_rest_transports_credentials_in_its_order() {
        let conn = Connection {
            uid: "wh".into(),
            adapter: "snowflake".into(),
            account: Some("xy12345.us-east-2.aws".into()),
            database: Some("analytics".into()),
            schema: Some("\"Mixed\"".into()),
            warehouse: Some("compute_wh".into()),
            username: Some("svc".into()),
            private_key_file: Some("/keys/rsa.p8".into()),
            query_timeout: Some(90),
            ..Default::default()
        };
        let pair = snowflake(&conn, &Secrets::default());
        assert_eq!(get(&pair, "adbc.snowflake.sql.account"), Some("xy12345.us-east-2.aws"));
        assert_eq!(get(&pair, "adbc.snowflake.sql.db"), Some("ANALYTICS"), "folded as Snowflake reads it unquoted");
        assert_eq!(get(&pair, "adbc.snowflake.sql.schema"), Some("Mixed"), "a quoted name keeps its case");
        assert_eq!(get(&pair, "adbc.snowflake.sql.warehouse"), Some("COMPUTE_WH"));
        assert_eq!(get(&pair, "username"), Some("svc"));
        assert_eq!(get(&pair, "adbc.snowflake.sql.auth_type"), Some("auth_jwt"));
        assert_eq!(get(&pair, "adbc.snowflake.sql.client_option.jwt_private_key"), Some("/keys/rsa.p8"));
        assert_eq!(get(&pair, "adbc.snowflake.sql.client_option.request_timeout"), Some("90s"));

        let pem = Secrets { private_key: secret("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"), ..Default::default() };
        let inline = snowflake(&conn, &pem);
        assert!(get(&inline, "adbc.snowflake.sql.client_option.jwt_private_key_pkcs8_value").is_some());
        assert_eq!(get(&inline, "adbc.snowflake.sql.client_option.jwt_private_key"), None);

        let pat = Secrets { personal_access_token: secret("pat"), ..pem.clone() };
        let with_pat = snowflake(&conn, &pat);
        assert_eq!(get(&with_pat, "adbc.snowflake.sql.auth_type"), Some("auth_pat"), "a token wins over a key pair");
        assert_eq!(get(&with_pat, "adbc.snowflake.sql.client_option.auth_token"), Some("pat"));

        let oauth = Secrets { oauth_token: secret("at"), ..pat };
        let with_oauth = snowflake(&conn, &oauth);
        assert_eq!(get(&with_oauth, "adbc.snowflake.sql.auth_type"), Some("auth_oauth"), "an OAuth token wins over everything");
        assert_eq!(get(&with_oauth, "adbc.snowflake.sql.client_option.auth_token"), Some("at"));
    }

    #[test]
    fn a_snowflake_url_becomes_the_host_and_names_the_account() {
        let conn =
            Connection { uid: "wh".into(), url: Some("https://acme.privatelink.snowflakecomputing.com:8443".into()), ..Default::default() };
        let plan = snowflake(&conn, &Secrets::default());
        assert_eq!(get(&plan, "adbc.snowflake.sql.account"), Some("acme"));
        assert_eq!(get(&plan, "adbc.snowflake.sql.uri.host"), Some("acme.privatelink.snowflakecomputing.com"));
        assert_eq!(get(&plan, "adbc.snowflake.sql.uri.port"), Some("8443"));
        assert_eq!(get(&plan, "adbc.snowflake.sql.uri.protocol"), Some("https"));
    }

    #[test]
    fn bigquery_names_a_file_or_leaves_the_default_chain_to_the_driver() {
        let conn = Connection {
            uid: "bq".into(),
            project: Some("acme-prod".into()),
            schema: Some("sales".into()),
            location: Some("EU".into()),
            ..Default::default()
        };
        let none = |_: &str| None;
        let adc = bigquery(&conn, &Secrets::default(), &none).unwrap();
        assert_eq!(get(&adc, "bigquery.project_id"), Some("acme-prod"));
        assert_eq!(get(&adc, "bigquery.dataset_id"), Some("sales"));
        assert_eq!(get(&adc, "bigquery.location"), Some("EU"));
        assert_eq!(get(&adc, "bigquery.auth_type"), Some("app_default_credentials"));

        let env = |k: &str| (k == "GOOGLE_APPLICATION_CREDENTIALS").then(|| "/env/key.json".to_string());
        assert_eq!(get(&bigquery(&conn, &Secrets::default(), &env).unwrap(), "bigquery.auth.credentials"), Some("/env/key.json"));

        let named = Connection { key_file: Some("/keys/sa.json".into()), ..conn.clone() };
        let file = bigquery(&named, &Secrets::default(), &env).unwrap();
        assert_eq!(get(&file, "bigquery.auth_type"), Some("json_credential_file"));
        assert_eq!(get(&file, "bigquery.auth.credentials"), Some("/keys/sa.json"), "datasources.yml wins over the environment");
    }

    #[test]
    fn bigquery_with_a_token_from_upstream_stays_on_rest() {
        let conn = Connection { uid: "bq".into(), project: Some("acme-prod".into()), ..Default::default() };
        let secrets = Secrets { oauth_token: secret("at"), ..Default::default() };
        assert_eq!(bigquery(&conn, &secrets, &|_| None), None);
    }

    // The three below need drivers on this machine: `dbc install sqlite
    // snowflake bigquery`, or ADBC_DRIVER_PATH naming a directory of
    // manifests. None of them needs an account.

    fn local(options: &[(&str, &str)], timeout: Duration) -> Adbc {
        let driver = load("sqlite").unwrap().expect("the sqlite ADBC driver is installed");
        let plan = Plan {
            driver: "sqlite",
            options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            scale_is_nominal: false,
            access: Access::ReadOnly,
        };
        Adbc::open(driver, &plan, "local", timeout).unwrap()
    }

    #[test]
    #[ignore = "needs the sqlite ADBC driver"]
    fn a_driver_answers_in_typed_batches_under_the_cap() {
        let mut db = local(&[("uri", ":memory:")], Duration::from_secs(30));
        let got = db.batches("SELECT 1 AS n, 'a' AS s, 2.5 AS x UNION ALL SELECT 2, NULL, 0.5", 10).unwrap();
        assert_eq!(got.columns(), vec!["n", "s", "x"]);
        let types: Vec<String> = got.schema.fields().iter().map(|f| f.data_type().to_string()).collect();
        assert_eq!(types, vec!["Int64", "Utf8", "Float64"], "the driver says what each column is");
        assert_eq!(
            got.to_text().unwrap().rows,
            vec![vec![Some("1".into()), Some("a".into()), Some("2.5".into())], vec![Some("2".into()), None, Some("0.5".into())]]
        );
        assert!(matches!(db.batches("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3", 2).unwrap_err(), Error::TooManyRows(_, 2)));
        assert!(matches!(db.batches("DELETE FROM t", 2).unwrap_err(), Error::NotARead(_)));
        assert!(matches!(db.batches("SELECT * FROM no_such_table", 2).unwrap_err(), Error::Query(_, _)));

        // Streamed: the schema first, then the driver's batches as it
        // produces them; a sink that has had enough ends the read, and
        // the connection answers the next statement.
        struct Counting {
            columns: usize,
            rows: usize,
            stop_at: usize,
        }
        impl Sink for Counting {
            fn schema(&mut self, schema: &SchemaRef) -> Result<()> {
                self.columns = schema.fields().len();
                Ok(())
            }
            fn batch(&mut self, batch: RecordBatch) -> Result<()> {
                self.rows += batch.num_rows();
                if self.rows >= self.stop_at {
                    return Err(Error::Query("sink".into(), "enough".into()));
                }
                Ok(())
            }
        }
        let many = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 50000) SELECT x, x * 2 AS y FROM c";
        let mut all = Counting { columns: 0, rows: 0, stop_at: usize::MAX };
        db.stream(many, &mut all).unwrap();
        assert_eq!((all.columns, all.rows), (2, 50_000));
        let mut none = Counting { columns: 0, rows: 0, stop_at: usize::MAX };
        db.stream("SELECT 1 AS n WHERE 1 = 0", &mut none).unwrap();
        assert_eq!((none.columns, none.rows), (1, 0), "the columns of a result with no rows");
        let mut some = Counting { columns: 0, rows: 0, stop_at: 1 };
        let err = db.stream(many, &mut some).unwrap_err().to_string();
        assert!(err.contains("enough"), "{err}");
        assert!(some.rows < 50_000, "it stopped before the end: {}", some.rows);
        assert_eq!(db.batches("SELECT 7 AS n", 10).unwrap().num_rows(), 1, "after a read stopped early");
    }

    /// The SQLite driver cannot cancel, which makes it the case worth
    /// having: the caller still gets an answer at the timeout, and the
    /// answer says the statement was not stopped.
    #[test]
    #[ignore = "needs the sqlite ADBC driver"]
    fn a_statement_past_the_timeout_is_given_up_on_even_when_it_cannot_be_cancelled() {
        let mut db = local(&[("uri", ":memory:")], Duration::from_secs(1));
        let started = std::time::Instant::now();
        let forever = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c";
        let err = db.batches(forever, 10).unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(20), "it came back: {:?}", started.elapsed());
        assert!(err.contains("no answer within 1s") && err.contains("query_timeout"), "{err}");
        eprintln!("sqlite, past the timeout: {err}");
    }

    /// Opens each warehouse driver with the options this module builds
    /// and fake values. No account answers, so it fails; what is checked
    /// is that it fails at the login and not at an option the driver
    /// does not know, which is how a renamed key would show.
    #[test]
    #[ignore = "needs the snowflake and bigquery ADBC drivers"]
    fn the_warehouse_drivers_know_every_option_they_are_sent() {
        // "failed to auth for unknown reason" is a login failing, which
        // is the expected end; only a complaint about an option counts.
        let unknown = |e: &Error| {
            let m = e.to_string().to_lowercase();
            m.contains("not implemented")
                || (m.contains("option") && ["unknown", "unrecognized", "invalid", "unsupported"].iter().any(|w| m.contains(w)))
        };
        let key = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../harness/sources/snowflake/rsa_key.p8");
        let conn = Connection {
            uid: "wh".into(),
            account: Some("zsql-test-no-such-account".into()),
            database: Some("db".into()),
            schema: Some("public".into()),
            warehouse: Some("wh".into()),
            role: Some("r".into()),
            username: Some("svc".into()),
            query_timeout: Some(5),
            ..Default::default()
        };
        let logins = [
            ("key pair file", Connection { private_key_file: Some(key.clone()), ..conn.clone() }, Secrets::default()),
            ("key pair pem", conn.clone(), Secrets { private_key: secret(&std::fs::read_to_string(&key).unwrap()), ..Default::default() }),
            ("pat", conn.clone(), Secrets { personal_access_token: secret("not-a-token"), ..Default::default() }),
            ("oauth", conn.clone(), Secrets { oauth_token: secret("not-a-token"), ..Default::default() }),
            (
                "url",
                Connection { url: Some("https://zsql-test-no-such-account.snowflakecomputing.com:443".into()), ..conn.clone() },
                Secrets { oauth_token: secret("not-a-token"), ..Default::default() },
            ),
        ];
        for (what, conn, secrets) in logins {
            let driver = load("snowflake").unwrap().expect("the snowflake ADBC driver is installed");
            let plan = snowflake(&conn, &secrets);
            match Adbc::open(driver, &plan, "wh", conn.timeout()).and_then(|mut a| a.batches("SELECT 1", 1)) {
                Ok(_) => panic!("{what}: a fake account answered"),
                Err(e) => {
                    eprintln!("snowflake, {what}: {e}");
                    assert!(!unknown(&e), "{what}: {e}");
                }
            }
        }

        let conn = Connection {
            uid: "bq".into(),
            project: Some("zsql-test-no-such-project".into()),
            schema: Some("ds".into()),
            location: Some("EU".into()),
            key_file: Some("/nonexistent/key.json".into()),
            query_timeout: Some(5),
            ..Default::default()
        };
        for (what, conn) in [("key file", conn.clone()), ("default chain", Connection { key_file: None, ..conn.clone() })] {
            let driver = load("bigquery").unwrap().expect("the bigquery ADBC driver is installed");
            let plan = bigquery(&conn, &Secrets::default(), &|_| None).unwrap();
            match Adbc::open(driver, &plan, "bq", conn.timeout()).and_then(|mut a| a.batches("SELECT 1", 1)) {
                Ok(_) => panic!("{what}: a fake project answered"),
                Err(e) => {
                    eprintln!("bigquery, {what}: {e}");
                    assert!(!unknown(&e), "{what}: {e}");
                }
            }
        }
    }

    // ------------------------------------------------------ live: bigquery
    //
    // Against a real project. ZSQL_TEST_BQ_PROJECT, _DATASET, _LOCATION
    // and _KEY_FILE (a service account's JSON key) say which. The
    // statements read no table but the dataset's own listing, so they
    // bill nothing by the byte.

    fn live_bigquery() -> Connection {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        Connection {
            uid: "bq".into(),
            adapter: "bigquery".into(),
            project: Some(var("ZSQL_TEST_BQ_PROJECT")),
            schema: Some(var("ZSQL_TEST_BQ_DATASET")),
            location: Some(var("ZSQL_TEST_BQ_LOCATION")),
            key_file: Some(var("ZSQL_TEST_BQ_KEY_FILE").into()),
            query_timeout: Some(120),
            ..Default::default()
        }
    }

    /// For an account that may run queries and may not open a read
    /// session: the datasource still answers, over REST, and says why.
    #[test]
    #[ignore = "needs the bigquery ADBC driver and an account without roles/bigquery.readSessionUser"]
    fn live_bigquery_without_the_read_permission_falls_back_to_rest() {
        let mut src = crate::open(&live_bigquery(), Secrets::default()).unwrap();
        let got = src.batches(EVERY_TYPE, 10).unwrap();
        assert_eq!(got.num_rows(), 1);
        assert_eq!(src.transport(), "rest");
        eprintln!("warnings: {:?}", src.warnings());
        assert!(src.warnings()[0].contains(NO_READ_SESSION), "{:?}", src.warnings());
        // The second statement goes straight to REST and repeats nothing.
        assert_eq!(src.batches("SELECT 1 AS i", 10).unwrap().num_rows(), 1);
        assert_eq!(src.warnings().iter().filter(|w| w.contains(NO_READ_SESSION)).count(), 1);
    }

    /// One of everything a result can hold.
    const EVERY_TYPE: &str = "SELECT 1 AS i, CAST('12.50' AS NUMERIC) AS n, CAST('1.5' AS BIGNUMERIC) AS bn, 1.5 AS f, TRUE AS b, \
        DATE '2026-01-02' AS d, TIMESTAMP '2026-01-02 03:04:05.123456+00' AS ts, DATETIME '2026-01-02 03:04:05' AS dt, \
        TIME '03:04:05' AS t, 'x' AS s, CAST(NULL AS STRING) AS z, [1, 2] AS arr, STRUCT(1 AS a, 'b' AS b) AS st";

    fn show(what: &str, got: &Batches) {
        let types: Vec<String> = got.schema.fields().iter().map(|f| format!("{}: {}", f.name(), f.data_type())).collect();
        eprintln!("{what}: types  {}", types.join(", "));
        for row in got.to_text().unwrap().rows.iter().take(5) {
            let cells: Vec<String> = row.iter().map(|c| c.clone().unwrap_or_else(|| "∅".into())).collect();
            eprintln!("{what}: row    {}", cells.join(" | "));
        }
    }

    #[test]
    #[ignore = "needs a BigQuery project: ZSQL_TEST_BQ_*"]
    fn live_bigquery_answers_over_rest() {
        let conn = live_bigquery();
        let mut rest = crate::adapters::bigquery::BigQuery::open(&conn, Secrets::default()).unwrap();
        let got = rest.batches(EVERY_TYPE, 10).unwrap();
        assert_eq!(got.num_rows(), 1);
        assert_eq!(rest.transport(), "rest");
        show("rest", &got);
        let tables = rest
            .query(
                &format!("SELECT table_name FROM `{}`.INFORMATION_SCHEMA.TABLES ORDER BY 1 LIMIT 20", conn.schema.as_deref().unwrap()),
                20,
            )
            .unwrap();
        eprintln!("rest: tables {:?}", tables.rows.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>());
    }

    /// A token the service account minted for itself stands in for one
    /// an upstream would send: a second connection is opened with the
    /// token alone, no key file, and must answer.
    #[test]
    #[ignore = "needs a BigQuery project: ZSQL_TEST_BQ_*"]
    fn live_bigquery_takes_a_token_obtained_elsewhere() {
        let conn = live_bigquery();
        let token = crate::adapters::bigquery::BigQuery::open(&conn, Secrets::default()).unwrap().bearer().unwrap();
        let bare = Connection { key_file: None, ..conn };
        let secrets = Secrets { oauth_token: secret(&token), ..Default::default() };
        assert_eq!(bigquery(&bare, &secrets, &|_| None), None, "a token from upstream is REST's");
        let mut src = crate::adapters::bigquery::BigQuery::open_with(&bare, secrets, &|_| None, None).unwrap();
        let rows = src.query("SELECT SESSION_USER() AS who", 1).unwrap();
        eprintln!("oauth: answered as {:?}", rows.rows[0][0]);
        let wrong = Secrets { oauth_token: secret("not-a-token"), ..Default::default() };
        let err = crate::adapters::bigquery::BigQuery::open_with(&bare, wrong, &|_| None, None).unwrap().query("SELECT 1", 1).unwrap_err();
        eprintln!("oauth: a bad token is refused: {err}");
    }

    #[test]
    #[ignore = "needs a BigQuery project and the bigquery ADBC driver"]
    fn live_bigquery_answers_through_the_driver() {
        let conn = live_bigquery();
        let plan = bigquery(&conn, &Secrets::default(), &|_| None).unwrap();
        let open = |timeout: Duration| {
            Adbc::open(load("bigquery").unwrap().expect("the bigquery ADBC driver is installed"), &plan, "bq", timeout).unwrap()
        };
        let mut adbc = open(conn.timeout());
        let started = std::time::Instant::now();
        let got = adbc.batches(EVERY_TYPE, 10).unwrap();
        eprintln!("adbc: first statement took {:?}", started.elapsed());
        assert_eq!(got.num_rows(), 1);
        show("adbc", &got);

        // The same statement over REST, to see the two spellings side by side.
        let mut rest = crate::adapters::bigquery::BigQuery::open(&conn, Secrets::default()).unwrap();
        show("rest", &rest.batches(EVERY_TYPE, 10).unwrap());

        // A table of the dataset, whatever is first: its real column types.
        let dataset = conn.schema.clone().unwrap();
        let tables = rest
            .query(
                &format!("SELECT table_name FROM `{dataset}`.INFORMATION_SCHEMA.TABLES WHERE table_type = 'BASE TABLE' ORDER BY 1 LIMIT 1"),
                1,
            )
            .unwrap();
        if let Some(Some(table)) = tables.rows.first().map(|r| r[0].clone()) {
            let started = std::time::Instant::now();
            let got = adbc.batches(&format!("SELECT * FROM `{dataset}`.`{table}` LIMIT 1000"), 1000).unwrap();
            eprintln!("adbc: {table}: {} rows in {} batches, {:?}", got.num_rows(), got.batches.len(), started.elapsed());
            show(&format!("adbc {table}"), &got);
        }

        // The cap refuses.
        let err = adbc.batches("SELECT x FROM UNNEST(GENERATE_ARRAY(1, 100)) AS x", 10).unwrap_err();
        assert!(matches!(err, Error::TooManyRows(_, 10)), "{err}");

        // The timeout: a statement that scans nothing and takes minutes.
        let mut hasty = open(Duration::from_secs(3));
        let started = std::time::Instant::now();
        let slow =
            "SELECT COUNT(*) FROM UNNEST(GENERATE_ARRAY(1, 1000000)) AS a, UNNEST(GENERATE_ARRAY(1, 100000)) AS b WHERE MOD(a * b, 7) = 3";
        match hasty.batches(slow, 10) {
            Ok(got) => eprintln!(
                "adbc: the slow statement finished inside the timeout after {:?}: {:?}",
                started.elapsed(),
                got.to_text().unwrap().rows
            ),
            Err(e) => eprintln!("adbc: past the timeout after {:?}: {e}", started.elapsed()),
        }
    }

    #[test]
    fn a_driver_nobody_installed_is_absent_not_an_error() {
        assert!(load("zsql-no-such-driver").unwrap().is_none());
    }
}
