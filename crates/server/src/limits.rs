//! Rate limiting and admission control.
//!
//! Three layers, in the order a request meets them:
//!
//! 1. **Per address**, for the routes that cost before anyone is
//!    identified: login, sign-up, invite acceptance, and the 401 path.
//!    A failed login also counts against the email, so stuffing one
//!    account from many addresses is slowed too.
//! 2. **Per principal**, a cost-weighted token bucket per account and a
//!    second per query key, so one tenant cannot crowd out the rest and
//!    one application cannot drain its account's allowance.
//! 3. **Admission**, a concurrency cap per cost class, so the process
//!    survives whatever gets past the first two. This is the layer that
//!    keeps the box alive, and it runs even with accounts off.
//!
//! Everything is in memory in this one process, like the deployments, so
//! limits reset on restart. A limiter shared between instances belongs
//! with the storage trait, not here. See `docs/rate-limiting.md`.

use crate::lock;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// What a request costs, which decides both its bucket and its
/// concurrency cap. One deploy is not one `sql`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Class {
    /// sql, explain, tables, values, summaries: microseconds each.
    Plan,
    /// explore and the fields search: a walk or a scoring pass over
    /// every visible field, milliseconds, and retriggered on keystrokes.
    Explore,
    /// deploy, validate, test: seconds of CPU.
    Heavy,
    /// the account endpoints: cheap, but not for applications to hammer.
    Manage,
    /// The credential routes: an argon2 hash or a mail send each, most
    /// of them spent before anyone is identified.
    Password,
    /// static assets and liveness: no limit, no admission.
    Exempt,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Plan => "plan",
            Class::Explore => "explore",
            Class::Heavy => "heavy",
            Class::Manage => "manage",
            Class::Password => "password",
            Class::Exempt => "exempt",
        }
    }
}

/// The files served at the site root; they take no limit.
const SITE_FILES: &[&str] = &[
    "favicon.ico",
    "favicon.svg",
    "apple-touch-icon.png",
    "site.webmanifest",
    "icon-192.png",
    "icon-512.png",
    "icon-maskable-192.png",
    "icon-maskable-512.png",
];

/// A request's class, from its method and path alone.
pub(crate) fn classify(method: &str, path: &str) -> Class {
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match (method, p.as_slice()) {
        (_, []) | (_, ["healthz"]) | (_, ["app", ..]) | (_, [".well-known", ..]) => Class::Exempt,
        (_, [one]) if SITE_FILES.contains(one) => Class::Exempt,
        ("POST", ["login"]) | ("POST", ["signup"]) | ("POST", ["invites", "accept"]) | ("POST", ["account", "password"]) => Class::Password,
        ("POST", ["password-reset"])
        | ("POST", ["password-reset", "accept"])
        | ("POST", ["verify-email"])
        | ("POST", ["account", "verify-email"]) => Class::Password,
        // One HTTP request carries one tool call, charged here as a plan;
        // the explore-class tools take an explore token too, in mcp.rs.
        ("POST", ["mcp"]) => Class::Plan,
        (_, ["projects", _, "branches", _, verb]) => match *verb {
            "deploy" | "validate" | "test" => Class::Heavy,
            "sql" | "explain" => Class::Plan,
            "explore" | "fields" => Class::Explore,
            _ => Class::Plan,
        },
        (_, ["projects", _, "branches", _, "values", _]) => Class::Plan,
        ("GET", ["projects", _, "branches", _]) => Class::Plan,
        ("GET", ["projects"]) => Class::Plan,
        _ => Class::Manage,
    }
}

/// Whether the route authenticates. The rest either have no caller yet
/// (login, sign-up, invite acceptance, and the reset and verification
/// links, where the token in the body is the credential), read the
/// cookie themselves (logout), or are public (assets, liveness).
///
/// `/account/verify-email` is not in this list: it sends the caller
/// their own link, so it needs to know who the caller is.
pub(crate) fn authenticates(path: &str) -> bool {
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // `.well-known` is where OAuth clients look for an authorization
    // server before they present anything; there is none here, and the
    // answer must be a plain 404, not a 401 asking for the key.
    !matches!(
        p.as_slice(),
        [] | ["healthz"]
            | ["app", ..]
            | [".well-known", ..]
            | ["login"]
            | ["signup"]
            | ["logout"]
            | ["invites", "accept"]
            | ["password-reset"]
            | ["password-reset", "accept"]
            | ["verify-email"]
    ) && !matches!(p.as_slice(), [one] if SITE_FILES.contains(one))
}

// ------------------------------------------------------------- token bucket

/// Tokens a second, and the most that can bank up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub per_sec: f64,
    pub burst: f64,
}

impl Rate {
    pub const UNLIMITED: Rate = Rate { per_sec: f64::INFINITY, burst: f64::INFINITY };

    /// `100/s,400` or `10/m,3`; `off` for no limit.
    pub fn parse(s: &str) -> Result<Rate, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("off") {
            return Ok(Rate::UNLIMITED);
        }
        let (rate, burst) = s.split_once(',').ok_or_else(|| format!("expected RATE,BURST, got {s:?}"))?;
        let (n, unit) = rate.trim().split_once('/').ok_or_else(|| format!("expected N/s or N/m, got {rate:?}"))?;
        let n: f64 = n.trim().parse().map_err(|_| format!("{n:?} is not a number"))?;
        let per_sec = match unit.trim() {
            "s" => n,
            "m" => n / 60.0,
            "h" => n / 3600.0,
            u => return Err(format!("unit must be s, m or h, got {u:?}")),
        };
        let burst: f64 = burst.trim().parse().map_err(|_| format!("{burst:?} is not a number"))?;
        if per_sec <= 0.0 || burst <= 0.0 {
            return Err(format!("{s:?} must be positive"));
        }
        Ok(Rate { per_sec, burst })
    }

    fn unlimited(&self) -> bool {
        self.per_sec.is_infinite()
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
    /// Kept so the sweeper knows when this one is full again.
    rate: Rate,
}

impl Bucket {
    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate.per_sec).min(rate.burst);
        self.updated = now;
        self.rate = rate;
    }
}

/// What a check decided, and what to tell the caller.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Decision {
    pub allowed: bool,
    /// Seconds to wait before retrying; 0 when allowed.
    pub retry_after: u64,
    pub limit: f64,
    pub remaining: f64,
    /// Seconds until the allowance is whole again.
    pub reset: u64,
}

impl Decision {
    fn ok() -> Decision {
        Decision { allowed: true, retry_after: 0, limit: 0.0, remaining: 0.0, reset: 0 }
    }
}

/// The routes an address is limited on separately. Each is its own
/// bucket, because each has its own rate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Gate {
    Login,
    Signup,
    /// Redeeming a one-time link: an invite, a reset, a verification.
    /// The tokens are unguessable, so this is about the cost of being
    /// asked, not about the guess succeeding.
    Invite,
    /// Asking for a password reset link. Its own low rate because it is
    /// the one unauthenticated route that makes the server send mail,
    /// and a sender's daily allowance is a finite thing someone else
    /// can spend.
    Reset,
    /// Keys that are not valid: cheap to answer, but not free, and a
    /// stream of them is someone guessing.
    BadKey,
    /// The MCP handshake without a key: initialize, ping, tools/list,
    /// which a hosted client sends before its user's key is applied.
    /// Cheap, and it carries no principal to charge, so the address
    /// pays for it.
    Handshake,
}

/// Whose bucket this is.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum BKey {
    Account(i64, Class),
    Key(i64, Class),
    Addr(IpAddr, Gate),
}

const SHARDS: usize = 16;

fn shard_of(k: &BKey) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    k.hash(&mut h);
    (h.finish() as usize) % SHARDS
}

// -------------------------------------------------------------------- mode

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Refuse what is over the limit.
    On,
    /// Decide and log, refuse nothing. For reading real traffic before
    /// turning the numbers on.
    Shadow,
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Mode::On,
            "shadow" | "log" | "dry-run" => Mode::Shadow,
            _ => Mode::Off,
        }
    }
}

// ------------------------------------------------------------------ config

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub mode: Mode,
    /// Per account, by class.
    pub plan: Rate,
    pub explore: Rate,
    pub heavy: Rate,
    pub manage: Rate,
    /// Per query key: plan and explore only, since keys are read only.
    pub key_plan: Rate,
    pub key_explore: Rate,
    /// Per client address, for the routes with no caller yet.
    pub login: Rate,
    pub signup: Rate,
    pub invite: Rate,
    pub reset: Rate,
    pub bad_key: Rate,
    pub handshake: Rate,
    /// Failed logins for one email before it is refused for a while,
    /// whichever address they came from, which is what working through
    /// a password list looks like. `0` turns it off: it is the one
    /// limit here that someone else can trip for you, since knowing an
    /// address's email is enough to keep them out of the web app for
    /// the window. Their keys keep working; only the password does not.
    pub email_failures: u32,
    pub email_window: Duration,
    /// Read the last hop of X-Forwarded-For instead of the socket peer.
    pub trusted_proxy: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            mode: Mode::Off,
            plan: Rate { per_sec: 100.0, burst: 400.0 },
            explore: Rate { per_sec: 20.0, burst: 60.0 },
            heavy: Rate { per_sec: 10.0 / 60.0, burst: 3.0 },
            manage: Rate { per_sec: 1.0, burst: 120.0 },
            key_plan: Rate { per_sec: 50.0, burst: 200.0 },
            key_explore: Rate { per_sec: 10.0, burst: 30.0 },
            login: Rate { per_sec: 10.0 / 60.0, burst: 10.0 },
            signup: Rate { per_sec: 5.0 / 3600.0, burst: 5.0 },
            invite: Rate { per_sec: 10.0 / 60.0, burst: 10.0 },
            reset: Rate { per_sec: 10.0 / 3600.0, burst: 5.0 },
            bad_key: Rate { per_sec: 1.0, burst: 60.0 },
            handshake: Rate { per_sec: 0.5, burst: 30.0 },
            email_failures: 5,
            email_window: Duration::from_secs(15 * 60),
            trusted_proxy: false,
        }
    }
}

impl Config {
    fn account_rate(&self, class: Class) -> Rate {
        match class {
            Class::Plan => self.plan,
            Class::Explore => self.explore,
            Class::Heavy => self.heavy,
            Class::Manage | Class::Password => self.manage,
            Class::Exempt => Rate::UNLIMITED,
        }
    }

    fn key_rate(&self, class: Class) -> Rate {
        match class {
            Class::Plan => self.key_plan,
            Class::Explore => self.key_explore,
            _ => Rate::UNLIMITED,
        }
    }
}

// ----------------------------------------------------------------- limiter

pub(crate) struct Limiter {
    pub cfg: Config,
    shards: [Mutex<HashMap<BKey, Bucket>>; SHARDS],
    /// Failed logins per email: how many, and when the window started.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Limiter {
    pub fn new(cfg: Config) -> Limiter {
        Limiter { cfg, shards: std::array::from_fn(|_| Mutex::new(HashMap::new())), failures: Mutex::new(HashMap::new()) }
    }

    pub fn on(&self) -> bool {
        self.cfg.mode != Mode::Off
    }

    /// Takes `cost` from a bucket, or says how long to wait. Allowing in
    /// shadow mode is the caller's business: this answers what the limit
    /// says, and `Limiter::enforced` folds the mode in.
    fn take(&self, key: BKey, rate: Rate, cost: f64, now: Instant) -> Decision {
        if rate.unlimited() {
            return Decision::ok();
        }
        let mut map = self.shards[shard_of(&key)].lock().unwrap();
        let b = map.entry(key).or_insert(Bucket { tokens: rate.burst, updated: now, rate });
        b.refill(rate, now);
        let reset = ((rate.burst - b.tokens) / rate.per_sec).ceil().max(0.0) as u64;
        if b.tokens >= cost {
            b.tokens -= cost;
            Decision { allowed: true, retry_after: 0, limit: rate.burst, remaining: b.tokens.floor(), reset }
        } else {
            let wait = (cost - b.tokens) / rate.per_sec;
            Decision { allowed: false, retry_after: wait.ceil().max(1.0) as u64, limit: rate.burst, remaining: 0.0, reset }
        }
    }

    /// Gives a token back, for when a later bucket in the same check
    /// refuses and the earlier one should not have paid.
    fn refund(&self, key: BKey, cost: f64) {
        let mut map = self.shards[shard_of(&key)].lock().unwrap();
        if let Some(b) = map.get_mut(&key) {
            b.tokens += cost;
        }
    }

    /// The caller's allowance for this class: the account's bucket, and
    /// for an application also its own key's.
    pub fn principal(&self, account_id: i64, query_key_id: Option<i64>, class: Class) -> Decision {
        self.principal_at(account_id, query_key_id, class, Instant::now())
    }

    fn principal_at(&self, account_id: i64, query_key_id: Option<i64>, class: Class, now: Instant) -> Decision {
        if self.cfg.mode == Mode::Off || class == Class::Exempt {
            return Decision::ok();
        }
        let account = self.take(BKey::Account(account_id, class), self.cfg.account_rate(class), 1.0, now);
        if !account.allowed {
            return account;
        }
        let Some(id) = query_key_id else { return account };
        let key = self.take(BKey::Key(id, class), self.cfg.key_rate(class), 1.0, now);
        if !key.allowed {
            // The account should not pay for a request its key refused.
            self.refund(BKey::Account(account_id, class), 1.0);
            return key;
        }
        // Report whichever allowance is nearer its end.
        if key.remaining < account.remaining {
            key
        } else {
            account
        }
    }

    /// An address's allowance on a route that costs before anyone is
    /// identified.
    pub fn address(&self, addr: IpAddr, gate: Gate) -> Decision {
        if self.cfg.mode == Mode::Off {
            return Decision::ok();
        }
        let rate = match gate {
            Gate::Login => self.cfg.login,
            Gate::Signup => self.cfg.signup,
            Gate::Invite => self.cfg.invite,
            Gate::Reset => self.cfg.reset,
            Gate::BadKey => self.cfg.bad_key,
            Gate::Handshake => self.cfg.handshake,
        };
        self.take(BKey::Addr(addr, gate), rate, 1.0, Instant::now())
    }

    /// Whether a decision actually refuses: in shadow mode it never does,
    /// it only says what it would have done.
    pub fn enforced(&self, d: &Decision, what: &str) -> bool {
        if d.allowed {
            return false;
        }
        if self.cfg.mode == Mode::Shadow {
            eprintln!("zsqld: rate limit (shadow) would refuse {what}: retry after {}s", d.retry_after);
            return false;
        }
        true
    }

    // --------------------------------------------- failed logins per email

    /// Records a failed login. True once the email is over its limit and
    /// should be refused for the rest of the window.
    pub fn login_failed(&self, email: &str) -> bool {
        self.login_failed_at(email, Instant::now())
    }

    fn login_failed_at(&self, email: &str, now: Instant) -> bool {
        if self.cfg.mode == Mode::Off || self.cfg.email_failures == 0 {
            return false;
        }
        let mut map = lock(&self.failures);
        let e = map.entry(email.to_ascii_lowercase()).or_insert((0, now));
        if now.saturating_duration_since(e.1) > self.cfg.email_window {
            *e = (0, now);
        }
        e.0 += 1;
        e.0 > self.cfg.email_failures
    }

    /// Whether this email is locked out, and for how long.
    pub fn login_blocked(&self, email: &str) -> Option<u64> {
        self.login_blocked_at(email, Instant::now())
    }

    fn login_blocked_at(&self, email: &str, now: Instant) -> Option<u64> {
        if self.cfg.mode == Mode::Off || self.cfg.email_failures == 0 {
            return None;
        }
        let over = {
            let map = lock(&self.failures);
            let (n, since) = map.get(&email.to_ascii_lowercase())?;
            let age = now.saturating_duration_since(*since);
            (*n > self.cfg.email_failures && age <= self.cfg.email_window).then(|| (self.cfg.email_window - age).as_secs().max(1))
        };
        match (over, self.cfg.mode) {
            (Some(secs), Mode::Shadow) => {
                eprintln!("zsqld: rate limit (shadow) would refuse logins for this email for {secs}s");
                None
            }
            (other, _) => other,
        }
    }

    /// A good password clears the email's failures.
    pub fn login_ok(&self, email: &str) {
        lock(&self.failures).remove(&email.to_ascii_lowercase());
    }

    // ------------------------------------------------------------- upkeep

    /// Drops buckets that are full again and failure windows that have
    /// passed, so an idle server does not hold a row per address seen.
    pub fn sweep(&self) {
        let now = Instant::now();
        for shard in &self.shards {
            let mut map = lock(shard);
            map.retain(|_, b| {
                let rate = b.rate;
                b.refill(rate, now);
                b.tokens < rate.burst
            });
        }
        let window = self.cfg.email_window;
        lock(&self.failures).retain(|_, (_, since)| now.saturating_duration_since(*since) <= window);
    }
}

// --------------------------------------------------------------- admission

/// How many requests of each class may be inside the server at once.
/// This is what keeps the process alive: past the cap a request waits a
/// moment and is then refused rather than queueing without bound.
pub(crate) struct Admission {
    plan: Arc<Semaphore>,
    explore: Arc<Semaphore>,
    heavy: Arc<Semaphore>,
    manage: Arc<Semaphore>,
    password: Arc<Semaphore>,
}

/// How long a class waits for a permit before the server says it is busy.
fn patience(class: Class) -> Duration {
    match class {
        Class::Heavy => Duration::from_secs(5),
        Class::Password => Duration::from_secs(2),
        _ => Duration::from_secs(1),
    }
}

/// How long a request may run before it is abandoned.
///
/// Heavy work gets ninety seconds rather than the couple of minutes a
/// deploy might want, because Cloudflare gives an origin a hundred
/// seconds and then answers 524 itself. A 524 carries no retry hint, so
/// the CLI cannot act on it and the caller is left guessing about a
/// deploy that may well have landed. Better that the server always gets
/// to give its own answer first.
pub(crate) fn deadline(class: Class) -> Duration {
    match class {
        Class::Heavy => Duration::from_secs(90),
        _ => Duration::from_secs(30),
    }
}

/// How many of each class may be in the server at once.
#[derive(Debug, Clone)]
pub struct AdmissionCounts {
    pub plan: usize,
    pub explore: usize,
    pub heavy: usize,
    pub manage: usize,
    pub password: usize,
}

impl Default for AdmissionCounts {
    /// Sized from the cores: planning is quick and parallel, heavy work
    /// and password hashing are neither.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        AdmissionCounts { plan: cores * 4, explore: cores, heavy: 2, manage: cores * 2, password: 2 }
    }
}

impl Admission {
    pub fn new(counts: &AdmissionCounts) -> Admission {
        Admission {
            plan: Arc::new(Semaphore::new(counts.plan)),
            explore: Arc::new(Semaphore::new(counts.explore)),
            heavy: Arc::new(Semaphore::new(counts.heavy)),
            manage: Arc::new(Semaphore::new(counts.manage)),
            password: Arc::new(Semaphore::new(counts.password)),
        }
    }

    fn semaphore(&self, class: Class) -> Option<&Arc<Semaphore>> {
        Some(match class {
            Class::Plan => &self.plan,
            Class::Explore => &self.explore,
            Class::Heavy => &self.heavy,
            Class::Manage => &self.manage,
            Class::Password => &self.password,
            Class::Exempt => return None,
        })
    }

    /// Waits a bounded moment for room to run. `None` means the class has
    /// no cap; `Err` means the server is busy.
    pub async fn admit(&self, class: Class) -> Result<Option<OwnedSemaphorePermit>, u64> {
        let Some(sem) = self.semaphore(class) else { return Ok(None) };
        let wait = patience(class);
        match tokio::time::timeout(wait, sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            // The semaphore is never closed; a closed one would mean the
            // server is shutting down, which is also "come back later".
            Ok(Err(_)) | Err(_) => Err(wait.as_secs().max(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn rates_parse() {
        assert_eq!(Rate::parse("100/s,400").unwrap(), Rate { per_sec: 100.0, burst: 400.0 });
        assert_eq!(Rate::parse("30/m,5").unwrap().per_sec, 0.5);
        assert_eq!(Rate::parse(" 10 / h , 2 ").unwrap().burst, 2.0);
        assert!(Rate::parse("off").unwrap().unlimited());
        assert!(Rate::parse("100").is_err());
        assert!(Rate::parse("100/d,1").is_err());
        assert!(Rate::parse("0/s,1").is_err());
    }

    #[test]
    fn a_bucket_spends_its_burst_then_refills() {
        let cfg = Config { mode: Mode::On, plan: Rate { per_sec: 10.0, burst: 5.0 }, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(l.principal_at(1, None, Class::Plan, t0).allowed, "token {i} should be there");
        }
        let refused = l.principal_at(1, None, Class::Plan, t0);
        assert!(!refused.allowed);
        assert_eq!(refused.retry_after, 1);
        // A tenth of a second buys exactly one token back.
        assert!(l.principal_at(1, None, Class::Plan, at(t0, 0.1)).allowed);
        assert!(!l.principal_at(1, None, Class::Plan, at(t0, 0.1)).allowed);
        // Idle long enough and the whole burst is back, but no more.
        for _ in 0..5 {
            assert!(l.principal_at(1, None, Class::Plan, at(t0, 100.0)).allowed);
        }
        assert!(!l.principal_at(1, None, Class::Plan, at(t0, 100.0)).allowed);
    }

    #[test]
    fn accounts_do_not_share_a_bucket() {
        let cfg = Config { mode: Mode::On, plan: Rate { per_sec: 1.0, burst: 1.0 }, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        assert!(l.principal_at(1, None, Class::Plan, t0).allowed);
        assert!(!l.principal_at(1, None, Class::Plan, t0).allowed);
        assert!(l.principal_at(2, None, Class::Plan, t0).allowed, "another account has its own allowance");
    }

    #[test]
    fn a_key_is_bounded_inside_its_account() {
        let cfg = Config {
            mode: Mode::On,
            plan: Rate { per_sec: 100.0, burst: 100.0 },
            key_plan: Rate { per_sec: 2.0, burst: 2.0 },
            ..Config::default()
        };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        assert!(l.principal_at(1, Some(7), Class::Plan, t0).allowed);
        assert!(l.principal_at(1, Some(7), Class::Plan, t0).allowed);
        assert!(!l.principal_at(1, Some(7), Class::Plan, t0).allowed, "the key runs out first");
        assert!(l.principal_at(1, Some(8), Class::Plan, t0).allowed, "a sibling key still has its own");
        // The account paid only for what it served: 3 allowed, not the
        // refused one.
        let left = l.take(BKey::Account(1, Class::Plan), Rate { per_sec: 100.0, burst: 100.0 }, 0.0, t0);
        assert_eq!(left.remaining, 97.0);
    }

    #[test]
    fn classes_have_their_own_allowances() {
        let cfg = Config { mode: Mode::On, explore: Rate { per_sec: 1.0, burst: 1.0 }, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        assert!(l.principal_at(1, None, Class::Explore, t0).allowed);
        assert!(!l.principal_at(1, None, Class::Explore, t0).allowed);
        assert!(l.principal_at(1, None, Class::Plan, t0).allowed, "planning is not spent by exploring");
    }

    #[test]
    fn shadow_decides_but_refuses_nothing() {
        let cfg = Config { mode: Mode::Shadow, plan: Rate { per_sec: 1.0, burst: 1.0 }, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        assert!(l.principal_at(1, None, Class::Plan, t0).allowed);
        let d = l.principal_at(1, None, Class::Plan, t0);
        assert!(!d.allowed, "the decision still says it is over");
        assert!(!l.enforced(&d, "test"), "but nothing is refused");
    }

    #[test]
    fn off_means_off() {
        let l = Limiter::new(Config { mode: Mode::Off, plan: Rate { per_sec: 1.0, burst: 1.0 }, ..Config::default() });
        let t0 = Instant::now();
        for _ in 0..100 {
            assert!(l.principal_at(1, None, Class::Plan, t0).allowed);
        }
    }

    #[test]
    fn a_few_wrong_passwords_lock_one_email_not_the_rest() {
        let cfg = Config { mode: Mode::On, email_failures: 3, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(!l.login_failed_at("ann@acme.test", t0));
        }
        assert!(l.login_failed_at("ann@acme.test", t0), "the fourth is over the limit");
        assert!(l.login_blocked_at("ann@acme.test", t0).is_some());
        assert!(l.login_blocked_at("ANN@acme.test", t0).is_some(), "the email is matched without case");
        assert!(l.login_blocked_at("bob@acme.test", t0).is_none());
        // The window passes and the slate is clean.
        assert!(l.login_blocked_at("ann@acme.test", at(t0, 16.0 * 60.0)).is_none());
        // A good password clears it at once.
        for _ in 0..4 {
            l.login_failed_at("cas@acme.test", t0);
        }
        assert!(l.login_blocked_at("cas@acme.test", t0).is_some());
        l.login_ok("cas@acme.test");
        assert!(l.login_blocked_at("cas@acme.test", t0).is_none());
    }

    #[test]
    fn sweeping_keeps_what_is_still_spent() {
        let cfg = Config { mode: Mode::On, plan: Rate { per_sec: 1.0, burst: 10.0 }, ..Config::default() };
        let l = Limiter::new(cfg);
        let t0 = Instant::now();
        l.principal_at(1, None, Class::Plan, t0);
        let before: usize = l.shards.iter().map(|s| lock(s).len()).sum();
        assert_eq!(before, 1);
        l.sweep();
        let after: usize = l.shards.iter().map(|s| lock(s).len()).sum();
        assert_eq!(after, 1, "a bucket mid-refill is kept");
    }

    #[test]
    fn routes_land_in_the_right_class() {
        use Class::*;
        for (m, p, want) in [
            ("POST", "/projects/a/branches/main/sql", Plan),
            ("POST", "/projects/a/branches/main/explain", Plan),
            ("POST", "/projects/a/branches/main/explore", Explore),
            ("GET", "/projects/a/branches/main/fields", Explore),
            ("POST", "/projects/a/branches/main/deploy", Heavy),
            ("POST", "/projects/a/branches/main/validate", Heavy),
            ("POST", "/projects/a/branches/main/test", Heavy),
            ("GET", "/projects/a/branches/main/values/dim", Plan),
            ("GET", "/projects/a/branches/main", Plan),
            ("DELETE", "/projects/a/branches/main", Manage),
            ("GET", "/projects", Plan),
            ("POST", "/mcp", Plan),
            ("POST", "/login", Password),
            ("POST", "/signup", Password),
            ("POST", "/invites/accept", Password),
            ("POST", "/account/password", Password),
            ("GET", "/account/keys", Manage),
            ("GET", "/me", Manage),
            ("GET", "/healthz", Exempt),
            ("GET", "/", Exempt),
            ("GET", "/app/", Exempt),
            ("GET", "/app/app.js", Exempt),
            ("GET", "/favicon.ico", Exempt),
        ] {
            assert_eq!(classify(m, p), want, "{m} {p}");
        }
    }

    #[test]
    fn only_the_routes_with_a_caller_authenticate() {
        for p in ["/projects", "/me", "/account/keys", "/projects/a/branches/main/sql"] {
            assert!(authenticates(p), "{p}");
        }
        for p in ["/", "/healthz", "/app/", "/app/app.css", "/favicon.svg", "/login", "/signup", "/logout", "/invites/accept"] {
            assert!(!authenticates(p), "{p}");
        }
    }

    #[tokio::test]
    async fn admission_refuses_when_there_is_no_room() {
        let a = Admission::new(&AdmissionCounts { heavy: 1, ..AdmissionCounts::default() });
        let held = a.admit(Class::Heavy).await.expect("the first one gets in");
        // The second waits its patience out and is told to come back.
        let t0 = Instant::now();
        let refused = a.admit(Class::Heavy).await;
        assert!(refused.is_err());
        assert!(t0.elapsed() >= Duration::from_secs(5));
        drop(held);
        assert!(a.admit(Class::Heavy).await.is_ok(), "room again once the first is done");
    }

    #[tokio::test]
    async fn exempt_requests_need_no_permit() {
        let a = Admission::new(&AdmissionCounts::default());
        assert!(a.admit(Class::Exempt).await.unwrap().is_none());
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn each_gate_counts_separately_per_address() {
        let cfg = Config {
            mode: Mode::On,
            login: Rate { per_sec: 0.01, burst: 2.0 },
            signup: Rate { per_sec: 0.01, burst: 1.0 },
            ..Config::default()
        };
        let l = Limiter::new(cfg);
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "203.0.113.8".parse().unwrap();
        assert!(l.address(a, Gate::Login).allowed);
        assert!(l.address(a, Gate::Login).allowed);
        assert!(!l.address(a, Gate::Login).allowed, "two logins is this address's lot");
        assert!(l.address(a, Gate::Signup).allowed, "signing up is a different allowance");
        assert!(!l.address(a, Gate::Signup).allowed);
        assert!(l.address(b, Gate::Login).allowed, "another address is unaffected");
        let h = Limiter::new(Config { mode: Mode::On, handshake: Rate { per_sec: 0.01, burst: 1.0 }, ..Config::default() });
        assert!(h.address(a, Gate::Handshake).allowed);
        assert!(!h.address(a, Gate::Handshake).allowed, "a keyless handshake has its own allowance per address");
        assert!(h.address(a, Gate::BadKey).allowed, "and does not spend the bad-key one");
    }
}

#[cfg(test)]
mod lockout_tests {
    use super::*;

    #[test]
    fn the_email_lockout_can_be_turned_off() {
        let l = Limiter::new(Config { mode: Mode::On, email_failures: 0, ..Config::default() });
        for _ in 0..50 {
            assert!(!l.login_failed("ann@acme.test"));
        }
        assert!(l.login_blocked("ann@acme.test").is_none());
    }
}
