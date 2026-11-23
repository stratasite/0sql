//! Where 0sql's log goes.
//!
//! Every crate reports what it did through `tracing` and none of them
//! prints. A binary calls [`init`] once, and this is the only code that
//! knows the level, the format and the destination. A process that
//! never calls it logs nothing and pays nothing, which is how a library
//! embedded somewhere else, and every test, runs.
//!
//! A log is not a command's output. What `zsql` prints for whoever ran
//! it, a result, a `warning:` line, an error, is the product and is
//! written by the command. The log is the account of what happened
//! inside: which transport answered, why a driver was given up on, how
//! long a statement took.
//!
//! Nothing here reads the environment. The caller hands over a filter
//! and a format, so a flag and a variable can only mean the same thing.

use std::fmt::Write as _;
use std::io::Write as _;
use std::str::FromStr;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// How a line is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `zsqld: listening on http://127.0.0.1:3699`: the name of the
    /// program, the message, then any fields as `key=value`. No time:
    /// the journal that collects the line stamps it.
    #[default]
    Plain,
    /// One JSON object a line, with `ts`, `level`, `target` and
    /// `message`, and each field under its own name.
    Json,
}

impl FromStr for Format {
    type Err = Error;
    fn from_str(s: &str) -> Result<Format, Error> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plain" | "text" => Ok(Format::Plain),
            "json" => Ok(Format::Json),
            _ => Err(Error::Format(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} is not a log format: plain or json")]
    Format(String),
    #[error(
        "{0:?} is not a log filter: a level (off, error, warn, info, debug, trace), or levels by crate as in `warn,zsql_source=debug`"
    )]
    Filter(String),
    #[error("a log is already installed in this process")]
    Installed,
}

/// What a binary decides about its log.
#[derive(Debug, Clone)]
pub struct Config {
    /// What a plain line starts with: `zsqld` for the server, `zsql`
    /// for a command.
    pub name: &'static str,
    /// A level, or levels by crate: `info`, `warn,zsql_source=debug`.
    /// A name matches by prefix, so `zsql=debug` is every crate here.
    pub filter: String,
    pub format: Format,
}

/// The filter a count of `-v` stands for when nothing spells one out:
/// none is `quiet`, one is what 0sql did, two is how, three is
/// everything from every library.
pub fn filter_for(verbosity: u8, quiet: &str) -> String {
    match verbosity {
        0 => quiet.to_string(),
        1 => "zsql=info".to_string(),
        2 => "zsql=debug".to_string(),
        _ => "trace".to_string(),
    }
}

/// Installs the log for this process, to stderr. Once.
pub fn init(config: &Config) -> Result<(), Error> {
    let filter = parse_filter(&config.filter)?;
    tracing_subscriber::registry().with(layer(config.name, config.format, stderr)).with(filter).try_init().map_err(|_| Error::Installed)
}

fn parse_filter(filter: &str) -> Result<Targets, Error> {
    let bad = || Error::Filter(filter.to_string());
    // A word on its own is a level. The parser below would take any
    // other word as a crate to log in full, so `inf` for `info` would
    // silence everything and say nothing.
    for part in filter.split(',').map(str::trim) {
        if !part.contains('=') && tracing::level_filters::LevelFilter::from_str(part).is_err() {
            return Err(bad());
        }
    }
    Targets::from_str(filter.trim()).map_err(|_| bad())
}

/// A line to stderr. Never a panic: a server whose stderr has gone away
/// (a pipe its reader closed) keeps serving, which `eprintln!` would
/// not allow.
fn stderr(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn layer<S: Subscriber>(name: &'static str, format: Format, sink: impl Fn(&str) + Send + Sync + 'static) -> impl Layer<S> {
    Lines { name, format, sink }
}

struct Lines<W> {
    name: &'static str,
    format: Format,
    sink: W,
}

impl<S: Subscriber, W: Fn(&str) + Send + Sync + 'static> Layer<S> for Lines<W> {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        let line = match self.format {
            Format::Plain => plain(self.name, *meta.level(), meta.target(), &fields),
            Format::Json => json(now(), *meta.level(), meta.target(), &fields),
        };
        (self.sink)(&line);
    }
}

/// An event's message and its fields, in the order they were given.
#[derive(Default)]
struct Fields {
    message: String,
    rest: Vec<(&'static str, serde_json::Value)>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: serde_json::Value) {
        match (field.name(), value) {
            ("message", serde_json::Value::String(s)) => self.message = s,
            (name, value) => self.rest.push((name, value)),
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}").into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.into());
    }
}

/// A plain line. What is routine reads as it always has, with nothing
/// in front of the message; a warning or an error says so; anything
/// finer names the crate it came from.
fn plain(name: &str, level: Level, target: &str, fields: &Fields) -> String {
    let mut line = format!("{name}: ");
    match level {
        Level::ERROR => line.push_str("error: "),
        Level::WARN => line.push_str("warning: "),
        Level::INFO => {}
        Level::DEBUG => {
            let _ = write!(line, "debug: {target}: ");
        }
        Level::TRACE => {
            let _ = write!(line, "trace: {target}: ");
        }
    }
    line.push_str(&one_line(&fields.message));
    for (key, value) in &fields.rest {
        let _ = match value {
            // A bare word stays bare; anything with a space in it is quoted.
            serde_json::Value::String(s) if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '=') => {
                write!(line, " {key}={s}")
            }
            other => write!(line, " {key}={other}"),
        };
    }
    line
}

/// One event is one line, whatever a message carried in.
fn one_line(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\n', '\r']) {
        s.replace("\r\n", "\\n").replace(['\n', '\r'], "\\n").into()
    } else {
        s.into()
    }
}

fn json(at: std::time::SystemTime, level: Level, target: &str, fields: &Fields) -> String {
    let mut o = serde_json::Map::new();
    o.insert("ts".into(), rfc3339(at).into());
    o.insert("level".into(), level.as_str().to_ascii_lowercase().into());
    o.insert("target".into(), target.into());
    o.insert("message".into(), fields.message.as_str().into());
    for (key, value) in &fields.rest {
        // A field cannot take the place of one of the four above.
        let key = if o.contains_key(*key) { format!("field.{key}") } else { key.to_string() };
        o.insert(key, value.clone());
    }
    serde_json::Value::Object(o).to_string()
}

fn now() -> std::time::SystemTime {
    std::time::SystemTime::now()
}

/// UTC, to the millisecond: `2026-11-23T18:04:05.123Z`.
fn rfc3339(at: std::time::SystemTime) -> String {
    let since = at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (days, secs) = (since.as_secs() / 86_400, since.as_secs() % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", secs / 3_600, secs % 3_600 / 60, secs % 60, since.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// What a filter and a format let through, as the lines written.
    fn logged(filter: &str, format: Format, emit: impl FnOnce()) -> Vec<String> {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = lines.clone();
        let subscriber = tracing_subscriber::registry()
            .with(layer("zsqld", format, move |l: &str| sink.lock().unwrap().push(l.to_string())))
            .with(parse_filter(filter).unwrap());
        tracing::subscriber::with_default(subscriber, emit);
        let got = lines.lock().unwrap().clone();
        got
    }

    #[test]
    fn a_routine_line_reads_as_it_always_has() {
        let lines = logged("info", Format::Plain, || tracing::info!("listening on http://127.0.0.1:3699"));
        assert_eq!(lines, ["zsqld: listening on http://127.0.0.1:3699"]);
    }

    #[test]
    fn a_warning_says_so_and_carries_its_fields() {
        let lines = logged("info", Format::Plain, || {
            tracing::warn!(datasource = "bq", reason = "no read session", rows = 3, "the driver was given up on");
        });
        assert_eq!(lines, [r#"zsqld: warning: the driver was given up on datasource=bq reason="no read session" rows=3"#]);
    }

    #[test]
    fn finer_lines_name_where_they_came_from() {
        let lines = logged("debug", Format::Plain, || tracing::debug!(target: "zsql_source", "opened"));
        assert_eq!(lines, ["zsqld: debug: zsql_source: opened"]);
    }

    #[test]
    fn one_event_is_one_line() {
        let lines = logged("info", Format::Plain, || tracing::info!("first\nsecond"));
        assert_eq!(lines, ["zsqld: first\\nsecond"]);
    }

    #[test]
    fn the_filter_decides_by_level_and_by_crate() {
        let emit = || {
            tracing::info!(target: "zsql_source", "ours, routine");
            tracing::debug!(target: "zsql_source::adbc", "ours, fine");
            tracing::debug!(target: "hyper", "theirs, fine");
            tracing::warn!(target: "hyper", "theirs, a warning");
        };
        assert_eq!(logged("off", Format::Plain, emit).len(), 0);
        assert_eq!(logged("warn", Format::Plain, emit).len(), 1);
        assert_eq!(logged("warn,zsql_source=debug", Format::Plain, emit).len(), 3);
        // `-v` and `-vv`: every crate here, by the prefix they share.
        assert_eq!(logged(&filter_for(1, "off"), Format::Plain, emit).len(), 1);
        assert_eq!(logged(&filter_for(2, "off"), Format::Plain, emit).len(), 2);
        assert_eq!(logged(&filter_for(3, "off"), Format::Plain, emit).len(), 4);
        assert_eq!(filter_for(0, "info"), "info");
    }

    #[test]
    fn json_is_an_object_a_line_with_typed_fields() {
        let lines = logged("info", Format::Json, || {
            tracing::warn!(target: "zsql_source", datasource = "bq", rows = 3, cached = false, level = "mine", "given up\non");
        });
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["level"], "warn");
        assert_eq!(v["target"], "zsql_source");
        assert_eq!(v["message"], "given up\non");
        assert_eq!(v["datasource"], "bq");
        assert_eq!(v["rows"], 3);
        assert_eq!(v["cached"], false);
        assert_eq!(v["field.level"], "mine", "a field does not replace the level");
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn what_cannot_be_read_is_refused_by_name() {
        assert!(matches!(parse_filter("loud"), Err(Error::Filter(_))));
        assert!(matches!(parse_filter("warn,zsql_source"), Err(Error::Filter(_))));
        assert!(matches!(parse_filter("zsql_source=loud"), Err(Error::Filter(_))));
        assert!(parse_filter(" warn , zsql_source=debug ").is_ok());
        assert!(matches!("yaml".parse::<Format>(), Err(Error::Format(_))));
        assert_eq!("JSON".parse::<Format>().unwrap(), Format::Json);
    }

    #[test]
    fn the_time_is_utc_to_the_millisecond() {
        let at = |secs: u64, millis: u32| std::time::UNIX_EPOCH + std::time::Duration::new(secs, millis * 1_000_000);
        assert_eq!(rfc3339(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(at(951_782_400, 0)), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(at(1_795_457_045, 123)), "2026-11-23T18:04:05.123Z");
    }
}
