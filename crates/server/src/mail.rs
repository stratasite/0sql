//! Sending the two links an account needs: verify this address, and set
//! a new password.
//!
//! There is no SMTP client here. The message is handed to a command on
//! stdin, the way `sendmail` has always been fed, and the command is the
//! operator's business: `msmtp -t` against a provider, `sendmail -t`, a
//! script that calls an HTTP API. That keeps a TLS stack and a provider's
//! API shape out of a binary that is built static against musl, keeps the
//! credential in the mail program's own configuration rather than this
//! process's environment, and makes changing provider a change to a
//! config file instead of a release.
//!
//! With no command configured the server still issues the tokens; the
//! links are written to stderr, so a self-hosted or development server
//! can be used without mail. They are never put in a response: a reset
//! is asked for by whoever types an address, who may not be its owner.
//!
//! See `docs/running-it.md` for the configuration on the live service.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Mailer {
    /// The shell command fed the message on stdin. `None` turns sending
    /// off, which is the default.
    command: Option<String>,
    /// The `From` header, e.g. `0sql <support@example.com>`.
    from: String,
    /// Where the app is reached, for building links.
    public_url: String,
    /// How long the command gets before it is killed.
    timeout: Duration,
}

/// What became of a message.
#[derive(Debug)]
pub enum Sent {
    Delivered,
    /// No command is configured, so the caller logs the link instead.
    NotConfigured,
}

impl Mailer {
    /// Reads the environment. `public_url` is where links point, which
    /// is `ZSQL_PUBLIC_URL` when set and the bind address otherwise.
    ///
    /// A command without a `From` is refused at start-up rather than at
    /// the first reset: the message would be rejected by any relay, and
    /// the first reset is exactly when nobody is watching.
    pub fn from_env(public_url: String) -> Result<Mailer, String> {
        let command = std::env::var("ZSQL_MAIL_COMMAND").ok().map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        let from = std::env::var("ZSQL_MAIL_FROM").unwrap_or_default().trim().to_string();
        if command.is_some() && from.is_empty() {
            return Err("ZSQL_MAIL_COMMAND is set but ZSQL_MAIL_FROM is not: a message needs a sender".into());
        }
        if header_unsafe(&from) {
            return Err("ZSQL_MAIL_FROM holds a line break".into());
        }
        let timeout = match std::env::var("ZSQL_MAIL_TIMEOUT") {
            Ok(v) => Duration::from_secs(v.trim().parse().map_err(|_| format!("ZSQL_MAIL_TIMEOUT: {v:?} is not a number of seconds"))?),
            Err(_) => Duration::from_secs(20),
        };
        Ok(Mailer { command, from, public_url: public_url.trim_end_matches('/').to_string(), timeout })
    }

    pub fn configured(&self) -> bool {
        self.command.is_some()
    }

    /// A link into the web app carrying a token. The base is trimmed
    /// here rather than only where it is read, so a trailing slash
    /// cannot put a second one in a link someone has to click.
    pub fn link(&self, path: &str, token: &str) -> String {
        format!("{}/app/{path}?token={}", self.public_url.trim_end_matches('/'), urlencode(token))
    }

    /// Hands one message to the command. Blocking: callers are already
    /// on a blocking thread.
    pub fn send(&self, to: &str, subject: &str, body: &str) -> Result<Sent, String> {
        let Some(command) = &self.command else { return Ok(Sent::NotConfigured) };
        // The address is checked where it enters the store, and the
        // subjects are ours; this is the belt to that braces, because
        // the cost of being wrong is a header written by someone else.
        if header_unsafe(to) || header_unsafe(subject) {
            return Err("the recipient or subject holds a line break".into());
        }
        let message = format!(
            "Date: {}\r\nFrom: {}\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{}@zsqld>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nAuto-Submitted: auto-generated\r\n\r\n{}",
            rfc2822_now(),
            self.from,
            message_id(),
            body.replace('\n', "\r\n")
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting the mail command: {e}"))?;
        child.stdin.take().ok_or("the mail command took no stdin")?.write_all(message.as_bytes()).map_err(|e| format!("writing the message: {e}"))?;
        // A wedged mail command must not hold a blocking thread for the
        // life of the process, so it is waited for with a deadline and
        // killed past it. `wait` has no timeout of its own.
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait().map_err(|e| format!("waiting for the mail command: {e}"))? {
                Some(status) if status.success() => return Ok(Sent::Delivered),
                Some(status) => return Err(format!("the mail command exited {}", status.code().map(|c| c.to_string()).unwrap_or_else(|| "on a signal".into()))),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("the mail command did not finish within {}s", self.timeout.as_secs()));
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// Whether a string would break out of the header it is put in.
fn header_unsafe(s: &str) -> bool {
    s.chars().any(|c| c == '\r' || c == '\n' || c == '\0')
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn message_id() -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{n:x}.{}", std::process::id())
}

/// `Fri, 03 Oct 2026 14:05:09 +0000`. A relay will add a `Date` when one
/// is missing, but a message without one looks enough like spam to be
/// worth the twenty lines.
fn rfc2822_now() -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant), as in the server's `humantime`.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    // 1970-01-01 was a Thursday, which is why the table starts there.
    let dow = DAYS[days.rem_euclid(7) as usize];
    let month = MONTHS[(m - 1) as usize];
    format!("{dow}, {d:02} {month} {y:04} {:02}:{:02}:{:02} +0000", rem / 3600, (rem % 3600) / 60, rem % 60)
}

// ------------------------------------------------------------------ messages

/// The verification link. Plain text: it is three lines and a URL, and a
/// password-reset mail that renders as a wall of HTML in a text client
/// is a password-reset mail nobody can use.
pub fn verification(link: &str) -> (String, String) {
    (
        "Verify your email address".into(),
        format!(
            "Confirm this address for your 0sql account by opening this link:\n\n  {link}\n\n\
             It works once, and expires in 24 hours.\n\n\
             If you did not create an account, you can ignore this message.\n"
        ),
    )
}

/// The reset link. It says plainly that nothing has changed yet, because
/// the commonest reader of this message is someone who did not ask for
/// it and wants to know whether to worry.
pub fn reset(email: &str, link: &str) -> (String, String) {
    (
        "Reset your 0sql password".into(),
        format!(
            "Open this link to set a new password for {email}:\n\n  {link}\n\n\
             It works once, and expires in an hour. Your current password keeps\n\
             working until you use it.\n\n\
             If you did not ask for this, you can ignore this message. Nothing has\n\
             changed, and no one can see your password.\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mail header ends at a line break, so anything interpolated into
    /// one decides where it ends. The store refuses addresses like this,
    /// and so does the sender.
    #[test]
    fn a_line_break_cannot_be_smuggled_into_a_header() {
        assert!(header_unsafe("ann@acme.test\r\nBcc: everyone@acme.test"));
        assert!(header_unsafe("ann@acme.test\nBcc: everyone@acme.test"));
        assert!(header_unsafe("ann@acme.test\0"));
        assert!(!header_unsafe("ann@acme.test"));
        assert!(!header_unsafe("0sql <support@acme.test>"));

        let m = Mailer { command: Some("true".into()), from: "0sql <s@t.test>".into(), public_url: "https://x.test".into(), timeout: Duration::from_secs(5) };
        assert!(m.send("ann@acme.test\r\nBcc: v@x.test", "hi", "body").is_err());
        assert!(m.send("ann@acme.test", "hi\r\nBcc: v@x.test", "body").is_err());
    }

    /// With nothing configured, sending is a no-op the caller is told
    /// about, rather than an error or a silent drop.
    #[test]
    fn with_no_command_nothing_is_sent_and_the_caller_is_told() {
        let m = Mailer { command: None, from: String::new(), public_url: "https://x.test".into(), timeout: Duration::from_secs(5) };
        assert!(!m.configured());
        assert!(matches!(m.send("ann@acme.test", "hi", "body"), Ok(Sent::NotConfigured)));
    }

    /// A command that hangs is killed rather than holding the thread.
    #[test]
    fn a_wedged_command_is_killed() {
        let m = Mailer { command: Some("sleep 30".into()), from: "0sql <s@t.test>".into(), public_url: "https://x.test".into(), timeout: Duration::from_millis(100) };
        let started = Instant::now();
        let err = m.send("ann@acme.test", "hi", "body").expect_err("the deadline passes");
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "it waited {:?}", started.elapsed());
    }

    /// A command that fails is reported, not swallowed: a reset nobody
    /// receives has to reach the journal.
    #[test]
    fn a_failing_command_is_an_error() {
        let m = Mailer { command: Some("exit 78".into()), from: "0sql <s@t.test>".into(), public_url: "https://x.test".into(), timeout: Duration::from_secs(5) };
        let err = m.send("ann@acme.test", "hi", "body").expect_err("it exits non-zero");
        assert!(err.contains("78"), "{err}");
    }

    #[test]
    fn links_point_into_the_app_and_escape_the_token() {
        let m = Mailer { command: None, from: String::new(), public_url: "https://0sql.com/".into(), timeout: Duration::from_secs(5) };
        assert_eq!(m.link("reset", "zrs_ab+cd"), "https://0sql.com/app/reset?token=zrs_ab%2Bcd");
    }

    #[test]
    fn the_date_header_is_well_formed() {
        let d = rfc2822_now();
        let parts: Vec<&str> = d.split(' ').collect();
        assert_eq!(parts.len(), 6, "{d}");
        assert!(parts[0].ends_with(','), "{d}");
        assert_eq!(parts[5], "+0000", "{d}");
    }
}
