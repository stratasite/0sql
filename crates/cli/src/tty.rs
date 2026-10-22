//! Whether a command may ask, and the few ways it asks. One rule: a
//! prompt is offered only when stdin and stderr are terminals, `--no-input`
//! was not given and `CI` is not set; every prompt has a flag form, and
//! the refusal names it, so a script or an agent never waits on a
//! question nobody will answer.

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};

static NO_INPUT: AtomicBool = AtomicBool::new(false);
static AGENT: AtomicBool = AtomicBool::new(false);

pub fn set_no_input(no: bool) {
    NO_INPUT.store(no, Ordering::Relaxed);
}

/// `--agent`: JSON wherever a command has a JSON shape, and never a
/// prompt. One flag, one promise, for a program driving the tool.
pub fn set_agent(on: bool) {
    AGENT.store(on, Ordering::Relaxed);
    if on {
        NO_INPUT.store(true, Ordering::Relaxed);
    }
}

pub fn agent() -> bool {
    AGENT.load(Ordering::Relaxed)
}

/// Whether a prompt may be shown.
pub fn interactive() -> bool {
    if NO_INPUT.load(Ordering::Relaxed) || std::env::var_os("CI").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 }
}

/// A picker or a question the user left: exit 130, nothing written.
#[derive(Debug)]
pub struct Cancelled(pub &'static str);

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Cancelled {}

/// A yes-or-no question, no by default. Without a terminal the answer is
/// a refusal that names the flag that would have said yes.
pub fn confirm(question: &str, flag: &str) -> Result<bool> {
    if !interactive() {
        bail!("{question} add {flag} to say yes without asking");
    }
    match inquire::Confirm::new(question).with_default(false).prompt() {
        Ok(yes) => Ok(yes),
        Err(inquire::InquireError::OperationCanceled) | Err(inquire::InquireError::OperationInterrupted) => {
            Err(Cancelled("nothing done").into())
        }
        Err(e) => Err(e.into()),
    }
}

/// Several of a list, filtered by typing, space to select, enter to
/// confirm. `preselected` are the indexes checked at the start. Without a
/// terminal this must not be reached: callers refuse first with the flag
/// form, which is why it takes the refusal text.
pub fn pick_many(question: &str, options: Vec<String>, preselected: Vec<usize>, flag_form: &str) -> Result<Vec<usize>> {
    if !interactive() {
        bail!("{flag_form}");
    }
    if options.is_empty() {
        return Ok(vec![]);
    }
    let indexed: Vec<String> = options;
    let n = indexed.len();
    let select = inquire::MultiSelect::new(question, indexed)
        .with_default(&preselected)
        .with_page_size(20.min(n.max(1)))
        .with_help_message("type to filter, space selects, enter confirms, esc leaves")
        .raw_prompt();
    match select {
        Ok(chosen) => Ok(chosen.into_iter().map(|o| o.index).collect()),
        Err(inquire::InquireError::OperationCanceled) | Err(inquire::InquireError::OperationInterrupted) => {
            Err(Cancelled("nothing selected; nothing written").into())
        }
        Err(e) => Err(e.into()),
    }
}

/// One of a list, or none on escape.
pub fn pick_one(question: &str, options: Vec<String>, flag_form: &str) -> Result<usize> {
    if !interactive() {
        bail!("{flag_form}");
    }
    match inquire::Select::new(question, options).with_page_size(20).raw_prompt() {
        Ok(chosen) => Ok(chosen.index),
        Err(inquire::InquireError::OperationCanceled) | Err(inquire::InquireError::OperationInterrupted) => {
            Err(Cancelled("nothing chosen").into())
        }
        Err(e) => Err(e.into()),
    }
}

/// A line of text, with a default. Without a terminal: the default when
/// there is one, else the refusal.
pub fn ask(question: &str, default: Option<&str>, flag_form: &str) -> Result<String> {
    if !interactive() {
        return match default {
            Some(d) => Ok(d.to_string()),
            None => bail!("{flag_form}"),
        };
    }
    let mut text = inquire::Text::new(question);
    if let Some(d) = default {
        text = text.with_default(d);
    }
    match text.prompt() {
        Ok(s) => Ok(s),
        Err(inquire::InquireError::OperationCanceled) | Err(inquire::InquireError::OperationInterrupted) => {
            Err(Cancelled("nothing done").into())
        }
        Err(e) => Err(e.into()),
    }
}

/// A secret, typed without echo.
pub fn ask_secret(question: &str, flag_form: &str) -> Result<String> {
    if !interactive() {
        bail!("{flag_form}");
    }
    match inquire::Password::new(question).without_confirmation().prompt() {
        Ok(s) => Ok(s),
        Err(inquire::InquireError::OperationCanceled) | Err(inquire::InquireError::OperationInterrupted) => {
            Err(Cancelled("nothing done").into())
        }
        Err(e) => Err(e.into()),
    }
}
