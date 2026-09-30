//! Opt-in phase timing. With `ZSQL_PROFILE` set, every [`span`] adds its
//! wall time to a per-thread table that [`report`] reads; otherwise a span
//! costs one branch. The bench command prints the table.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

thread_local! {
    static TABLE: RefCell<BTreeMap<&'static str, (Duration, u64)>> = RefCell::new(BTreeMap::new());
}

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ZSQL_PROFILE").is_some())
}

/// A timed phase; accumulates when dropped.
pub struct Span {
    name: &'static str,
    start: Option<Instant>,
}

pub fn span(name: &'static str) -> Span {
    Span { name, start: enabled().then(Instant::now) }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            let elapsed = start.elapsed();
            TABLE.with(|t| {
                let mut t = t.borrow_mut();
                let e = t.entry(self.name).or_insert((Duration::ZERO, 0));
                e.0 += elapsed;
                e.1 += 1;
            });
        }
    }
}

/// Every phase seen on this thread: name, total time, count.
pub fn report() -> Vec<(&'static str, Duration, u64)> {
    TABLE.with(|t| t.borrow().iter().map(|(k, (d, n))| (*k, *d, *n)).collect())
}

pub fn reset() {
    TABLE.with(|t| t.borrow_mut().clear());
}
