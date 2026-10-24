//! Accounts, users, keys and project access, in one SQLite file.
//!
//! The model:
//!
//! - An **account** is the tenant. It has users through memberships with a
//!   role, `admin` or `developer`, and it owns projects.
//! - A **user** acts through personal **user keys**, which do exactly what
//!   the user may do. Every developer reads every project in the account
//!   (unless the project is `restricted`); the creator of a project owns
//!   it; owners grant `read`, `write` or `owner` to other users; admins
//!   own everything.
//! - A **query key** is a principal of its own, for applications: read
//!   only, with grants on projects, each grant naming a branch (the
//!   production branch by default, `*` for every branch).
//!
//! Secrets are shown once and stored hashed; a visible prefix names a key
//! in logs. Every change is written to an audit table with its actor.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Denied(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Developer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Read,
    Write,
    Owner,
}

/// The two kinds of emailed link. Both are one-time, both are stored
/// hashed like every other secret here, and both are bound to the
/// address they were sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Proves the address is read by its owner.
    Verify,
    /// Sets a new password without knowing the old one.
    Reset,
}

impl TokenKind {
    fn as_str(self) -> &'static str {
        match self {
            TokenKind::Verify => "verify",
            TokenKind::Reset => "reset",
        }
    }

    /// The prefix of the secret, so a token is recognisable in a log
    /// line and cannot be redeemed at the wrong endpoint.
    fn secret_kind(self) -> &'static str {
        match self {
            TokenKind::Verify => "zvf",
            TokenKind::Reset => "zrs",
        }
    }

    /// How long the link works. A reset is short because it is a
    /// password in waiting; a verification is long because it is often
    /// read the next morning.
    fn minutes(self) -> i64 {
        match self {
            TokenKind::Verify => 24 * 60,
            TokenKind::Reset => 60,
        }
    }
}

/// How long after issuing one link the same user is refused another.
/// The per-address rate limit stops someone asking for many resets; this
/// stops them mailbombing one victim from many addresses.
const TOKEN_COOLDOWN_SECONDS: i64 = 60;

/// What an action needs on a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// sql, explain, explore, fields, tables, values, status
    Read,
    /// deploy, validate, test, remove a branch, upload samples
    Write,
    /// grant access, manage query key grants, delete the project
    Owner,
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: i64,
    pub uid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: String,
    /// Whether the address has been proven: a verification link was
    /// followed, or a password was reset through one. Nothing is denied
    /// for want of it; it says whether the address can be trusted for
    /// support and billing.
    pub email_verified: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub account_id: i64,
    pub uid: String,
    pub name: String,
    pub production_branch: String,
    /// `account`: every developer reads it; `restricted`: granted users only.
    pub visibility: String,
    /// Deploying to the production branch needs an owner.
    pub protected_production: bool,
    pub created_by: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserKey {
    pub id: i64,
    pub user_id: i64,
    /// The account the key acts in. A user who belongs to several
    /// accounts has keys in each; one key never crosses over.
    pub account_id: Option<i64>,
    pub name: String,
    pub prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryKey {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub prefix: String,
    pub created_by: Option<i64>,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// A query key's right to read one project: on its production branch
/// when `branch` is `None`, on every branch when `*`, else on that branch.
///
/// `context` is the security context the key plans with on this project
/// when the project's owner has bound one. It is the owner's claim about
/// who the key acts as, so a bound context cannot be overridden by the
/// caller; an unbound grant leaves the context to the application, as
/// before. Stored as the JSON the owner sent; the server validates its
/// shape before it gets here.
#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    pub project_id: i64,
    pub project_uid: String,
    pub branch: Option<String>,
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Membership {
    pub user: User,
    pub role: Role,
}

/// Who is calling.
#[derive(Debug, Clone)]
pub enum Principal {
    User { user: User, account: Account, role: Role, key_id: i64 },
    QueryKey { key: QueryKey, account: Account, grants: Vec<Grant> },
}

impl Principal {
    pub fn account(&self) -> &Account {
        match self {
            Principal::User { account, .. } | Principal::QueryKey { account, .. } => account,
        }
    }
    pub fn user_id(&self) -> Option<i64> {
        match self {
            Principal::User { user, .. } => Some(user.id),
            Principal::QueryKey { .. } => None,
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Principal::User { user, role, .. } => format!("{} ({:?})", user.email, role).to_lowercase(),
            Principal::QueryKey { key, .. } => format!("query key {} ({})", key.name, key.prefix),
        }
    }
}

/// Decisions worth not making twice. Every request with a key asks the
/// same three questions (who is this, what project, what level), and the
/// answers change only when someone changes them, which is rare. Holding
/// them for a moment takes the store's lock off the hot path.
///
/// Correctness rests on two things: every change calls [`Store::bump`],
/// which empties this and moves the generation on; and a read that
/// raced a change notices the generation moved and does not cache what
/// it saw. The TTL is only a backstop for a change that somehow does
/// neither.
#[derive(Default)]
struct Cache {
    /// secret hash -> who it is
    principals: HashMap<String, (Principal, Instant)>,
    /// session token hash -> whose session
    sessions: HashMap<String, (Principal, Instant)>,
    projects: HashMap<(i64, String), (Project, Instant)>,
    /// (project, user) -> the user's level on it
    levels: HashMap<(i64, i64), (Option<Level>, Instant)>,
}

pub struct Store {
    conn: Mutex<Connection>,
    cache: RwLock<Cache>,
    /// Moved on by every change, so a read that overlapped one throws
    /// its answer away rather than caching it.
    generation: AtomicU64,
    ttl: Duration,
    /// A real argon2 hash of nothing anyone knows, verified against when
    /// a sign-in names an address with no password to check, so that a
    /// miss takes as long as a wrong password.
    decoy: String,
}

/// How long an invite token stays redeemable.
pub const INVITE_DAYS: i64 = 7;

// A lock is never poisoned here on purpose: a panic in one request
// must not take the lock, and with it every later request, down with
// it. The data behind these locks is a connection, a cache or a map
// that a half-finished request leaves consistent.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How long an authentication decision may be reused when the caller
/// does not say: long enough to take the store off the hot path, short
/// enough that a change which somehow skipped invalidation is forgotten
/// quickly. Zero turns the cache off, which is also how to prove a bug
/// is not the cache's.
pub const DEFAULT_AUTH_CACHE_TTL: Duration = Duration::from_secs(60);

/// The schema as numbered steps, oldest first. `PRAGMA user_version`
/// holds how many of them a file has seen, so the length of this list is
/// the current schema version. A fresh database and an upgraded one end
/// up identical because both ran this same list, not because anyone
/// remembered to describe a change in two places.
///
/// Append only. A step that has shipped is never edited: a database that
/// already applied it will not apply it again, so editing one changes
/// what new files get and nothing else, which is the divergence the
/// version exists to prevent. Correcting a shipped step means adding
/// another step.
///
/// Plain `CREATE TABLE`, not `CREATE TABLE IF NOT EXISTS`. A table that
/// unexpectedly exists means the file is not the shape its version
/// claims, which is worth failing loudly over rather than skipping.
const MIGRATIONS: &[&str] = &[
    // 1: the schema as the service first shipped, through 0.2.1. The
    // `email_verified_at` column is inline here; the database on the
    // live service gained it from an `ALTER` that this list replaces.
    "
CREATE TABLE accounts (
  id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
CREATE TABLE users (
  id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT NOT NULL DEFAULT '',
  password_hash TEXT, email_verified_at TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
CREATE TABLE memberships (
  account_id INTEGER NOT NULL REFERENCES accounts(id), user_id INTEGER NOT NULL REFERENCES users(id),
  role TEXT NOT NULL CHECK (role IN ('admin','developer')),
  PRIMARY KEY (account_id, user_id));
CREATE TABLE projects (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),
  uid TEXT NOT NULL, name TEXT NOT NULL DEFAULT '', production_branch TEXT NOT NULL DEFAULT 'main',
  visibility TEXT NOT NULL DEFAULT 'account' CHECK (visibility IN ('account','restricted')),
  protected_production INTEGER NOT NULL DEFAULT 0,
  created_by INTEGER REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  UNIQUE (account_id, uid));
CREATE TABLE project_access (
  project_id INTEGER NOT NULL REFERENCES projects(id), user_id INTEGER NOT NULL REFERENCES users(id),
  level TEXT NOT NULL CHECK (level IN ('read','write','owner')),
  PRIMARY KEY (project_id, user_id));
CREATE TABLE user_keys (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), name TEXT NOT NULL,
  prefix TEXT NOT NULL, hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  last_used_at TEXT, revoked_at TEXT);
CREATE TABLE query_keys (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id), name TEXT NOT NULL,
  prefix TEXT NOT NULL, hash TEXT NOT NULL UNIQUE, created_by INTEGER REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  last_used_at TEXT, revoked_at TEXT);
CREATE TABLE query_key_grants (
  query_key_id INTEGER NOT NULL REFERENCES query_keys(id), project_id INTEGER NOT NULL REFERENCES projects(id),
  branch TEXT,
  PRIMARY KEY (query_key_id, project_id));
CREATE TABLE invites (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id), user_id INTEGER NOT NULL REFERENCES users(id),
  hash TEXT NOT NULL UNIQUE, created_by TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')), accepted_at TEXT);
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), account_id INTEGER NOT NULL REFERENCES accounts(id),
  hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  expires_at TEXT NOT NULL);
CREATE TABLE email_tokens (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id),
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  kind TEXT NOT NULL CHECK (kind IN ('verify','reset')),
  hash TEXT NOT NULL UNIQUE,
  email TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  expires_at TEXT NOT NULL, used_at TEXT);
CREATE INDEX email_tokens_user ON email_tokens (user_id, kind);
CREATE TABLE audit (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  subject TEXT NOT NULL DEFAULT '',
  at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));",
    // 2. A personal key belongs to the account that issued it, and a
    // user has a home account that a login lands in. Both are filled
    // from the first membership, which is exactly what authentication
    // and login resolved to before, so nothing changes for anyone
    // already here.
    "ALTER TABLE user_keys ADD COLUMN account_id INTEGER REFERENCES accounts(id);
UPDATE user_keys SET account_id = (SELECT m.account_id FROM memberships m WHERE m.user_id = user_keys.user_id ORDER BY m.account_id LIMIT 1);
ALTER TABLE users ADD COLUMN home_account_id INTEGER REFERENCES accounts(id);
UPDATE users SET home_account_id = (SELECT m.account_id FROM memberships m WHERE m.user_id = users.id ORDER BY m.account_id LIMIT 1);",
    // 3. A grant may carry the security context the key plans with on
    // that project, bound by the project's owner. NULL is the state
    // every existing grant is in: the application supplies the context.
    "ALTER TABLE query_key_grants ADD COLUMN context TEXT;",
];

/// Brings a database up to the last step in [`MIGRATIONS`], applying
/// only the steps it has not seen.
///
/// Each step commits together with the version it reaches, so an
/// interrupted upgrade leaves the file on the step before it rather than
/// half-changed, and running again carries on from there. `user_version`
/// lives in the file header and is journalled with everything else,
/// which is what lets the two share a transaction.
///
/// A later step that has to rebuild a table — SQLite cannot alter a
/// CHECK constraint, so changing one means create, copy, drop, rename —
/// will need `foreign_keys` off while it runs. That pragma is a no-op
/// inside a transaction, so it has to be turned off around this call
/// rather than within a step.
fn migrate(conn: &Connection) -> Result<()> {
    let have: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let have = have.max(0) as usize;
    let want = MIGRATIONS.len();
    if have > want {
        return Err(Error::Invalid(format!(
            "this accounts database is at schema version {have}, newer than the {want} this build knows. \
             Run the build it came from, or restore a backup."
        )));
    }
    if have == 0 && !is_empty(conn)? {
        return Err(Error::Invalid(
            "this accounts database holds tables but no schema version, so it predates versioning. \
             Set `PRAGMA user_version = 1` on a file that already has every table of version 1, or start a new one."
                .into(),
        ));
    }
    for (i, step) in MIGRATIONS.iter().enumerate().skip(have) {
        let version = i + 1;
        conn.execute_batch(&format!("BEGIN; {step} PRAGMA user_version = {version}; COMMIT;"))
            .map_err(|e| Error::Invalid(format!("applying schema version {version}: {e}")))?;
    }
    Ok(())
}

/// Whether a database holds nothing of ours. Step 1 creates every table
/// the service started with, so it can only run on an empty file.
fn is_empty(conn: &Connection) -> Result<bool> {
    let n: i64 =
        conn.query_row("SELECT count(*) FROM sqlite_master WHERE type IN ('table','index') AND name NOT LIKE 'sqlite_%'", [], |r| {
            r.get(0)
        })?;
    Ok(n == 0)
}

/// Records a key's use, at most once a minute: a key in steady use
/// would otherwise commit a write on every request.
fn touch(conn: &Connection, table: &str, id: i64, last: Option<&str>) -> rusqlite::Result<()> {
    let now: String = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| r.get(0))?;
    if let Some(last) = last {
        if last.len() == now.len() && last[..16] == now[..16] {
            return Ok(());
        }
    }
    conn.execute(&format!("UPDATE {table} SET last_used_at = ?1 WHERE id = ?2"), params![now, id])?;
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::PasswordHasher;
    if password.chars().count() < 8 {
        return Err(Error::Invalid("a password needs at least 8 characters".into()));
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2::Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| Error::Invalid(e.to_string()))?.to_string())
}

/// Whether the password is the one the argon2 hash was made from. A
/// hash that does not parse matches nothing.
fn password_matches(stored: &str, password: &str) -> bool {
    use argon2::{PasswordHash, PasswordVerifier};
    match PasswordHash::new(stored) {
        Ok(parsed) => argon2::Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn new_secret(kind: &str) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let body: String = (0..40).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    format!("{kind}_{body}")
}

/// The part of a secret safe to show: its kind and first eight characters.
pub fn prefix_of(secret: &str) -> String {
    secret.chars().take(12).collect()
}

/// A bound context as stored. A row that does not parse is treated as
/// unbound rather than failing every request on the key; it cannot be
/// written by the server, which validates before storing.
fn context_of(text: Option<String>) -> Option<serde_json::Value> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// ActiveSupport-style slug, the same the CLI derives project uids with.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            dash = ch == '-';
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn role_of(s: &str) -> Role {
    if s == "admin" {
        Role::Admin
    } else {
        Role::Developer
    }
}

fn level_of(s: &str) -> Level {
    match s {
        "owner" => Level::Owner,
        "write" => Level::Write,
        _ => Level::Read,
    }
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Owner => "owner",
        Level::Write => "write",
        Level::Read => "read",
    }
}

pub fn parse_level(s: &str) -> Result<Level> {
    match s.trim().to_lowercase().as_str() {
        "read" => Ok(Level::Read),
        "write" => Ok(Level::Write),
        "owner" => Ok(Level::Owner),
        other => Err(Error::Invalid(format!("'{other}' is not an access level (read, write, owner)"))),
    }
}

pub fn parse_role(s: &str) -> Result<Role> {
    match s.trim().to_lowercase().as_str() {
        "admin" => Ok(Role::Admin),
        "developer" => Ok(Role::Developer),
        other => Err(Error::Invalid(format!("'{other}' is not a role (admin, developer)"))),
    }
}

/// A name someone typed, for an account, a user or a key: trimmed,
/// one line, no control characters, at most eighty characters, so it
/// can be shown and logged as it is.
fn name_of(s: &str, what: &str) -> Result<String> {
    let s = s.trim();
    if s.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("{what} holds a control character")));
    }
    if s.chars().count() > 80 {
        return Err(Error::Invalid(format!("{what} is longer than 80 characters")));
    }
    Ok(s.to_string())
}

fn email_of(s: &str) -> Result<String> {
    let e = s.trim().to_lowercase();
    // 254 is the longest address that fits in a mail envelope; anything
    // longer is not one, and the failed-login table keys on addresses.
    if !e.contains('@') || e.len() < 3 || e.len() > 254 {
        return Err(Error::Invalid(format!("'{s}' is not an email address")));
    }
    // Addresses end up in mail headers, where a control character or a
    // space would let the rest of a header be written by whoever typed
    // the address. Trimming is not enough: the break can be in the
    // middle. Refused here, at the one door every address comes through.
    if e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Invalid(format!("'{s}' is not an email address")));
    }
    Ok(e)
}

const USER_COLS: &str = "id, email, name, email_verified_at";
const PROJECT_COLS: &str = "id, account_id, uid, name, production_branch, visibility, protected_production, created_by";

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User { id: r.get(0)?, email: r.get(1)?, name: r.get(2)?, email_verified: r.get::<_, Option<String>>(3)?.is_some() })
}

fn project_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        account_id: r.get(1)?,
        uid: r.get(2)?,
        name: r.get(3)?,
        production_branch: r.get(4)?,
        visibility: r.get(5)?,
        protected_production: r.get::<_, i64>(6)? != 0,
        created_by: r.get(7)?,
    })
}

/// What a sign-up makes: the account, its first admin, and that user's
/// first key, the secret shown once.
#[derive(Debug, Clone, Serialize)]
pub struct Signup {
    pub account: Account,
    pub user: User,
    pub key: UserKey,
    pub secret: String,
    /// The verification link's token, for the caller to mail. Never
    /// serialized: it is a credential for the address, and the sign-up
    /// response goes to whoever typed the address, who may not own it.
    #[serde(skip)]
    pub verify_token: Option<String>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Self::open_with(path, DEFAULT_AUTH_CACHE_TTL)
    }

    /// [`Store::open`] with how long an authentication decision may be
    /// reused.
    pub fn open_with(path: &Path, auth_cache_ttl: Duration) -> Result<Store> {
        let conn = Connection::open(path)?;
        // WAL with synchronous=NORMAL: a commit does not fsync, the
        // checkpoint does, so a power cut can lose the last moments of
        // writes but never corrupts the file. Right for key metadata.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
        migrate(&conn)?;
        Ok(Store::over(conn, auth_cache_ttl))
    }

    pub fn in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrate(&conn)?;
        Ok(Store::over(conn, DEFAULT_AUTH_CACHE_TTL))
    }

    fn over(conn: Connection, ttl: Duration) -> Store {
        let decoy = hash_password(&new_secret("decoy")).expect("a generated secret is long enough");
        Store { conn: Mutex::new(conn), cache: RwLock::new(Cache::default()), generation: AtomicU64::new(0), ttl, decoy }
    }

    // --------------------------------------------------------- the cache

    /// A cached answer, if one is there and still fresh.
    fn hit<T>(&self, pick: impl FnOnce(&Cache) -> Option<(T, Instant)>) -> Option<T> {
        if self.ttl.is_zero() {
            return None;
        }
        let (value, at) = pick(&read(&self.cache))?;
        (at.elapsed() < self.ttl).then_some(value)
    }

    /// Caches an answer, unless a change landed while it was being read.
    fn store_if_current(&self, generation: u64, put: impl FnOnce(&mut Cache)) {
        if self.ttl.is_zero() || self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        put(&mut write(&self.cache));
    }

    /// Throws every cached decision away. Called by the audit row that
    /// every change writes, so a new kind of change cannot forget to,
    /// and by the two changes that write no audit row.
    ///
    /// Emptying all of it rather than the entries that changed is what
    /// keeps "the old secret stops at once" true without tracking which
    /// decision depended on what. Changes are rare; a burst of misses
    /// after one costs nothing.
    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        let mut c = write(&self.cache);
        c.principals.clear();
        c.sessions.clear();
        c.projects.clear();
        c.levels.clear();
    }

    /// Writes the audit row for a change, and marks the cache stale.
    /// The two belong together: everything that changes an account
    /// writes one, so this is the one place invalidation cannot be
    /// forgotten. Called while the connection is held, so a read that
    /// overlapped it sees the generation move.
    fn audit(&self, conn: &Connection, account_id: i64, actor: &str, action: &str, subject: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO audit (account_id, actor, action, subject) VALUES (?1, ?2, ?3, ?4)",
            params![account_id, actor, action, subject],
        )?;
        self.bump();
        Ok(())
    }

    // ------------------------------------------------------------ sign-up

    /// A new account with its first admin and key. The account uid is the
    /// slug of its name, made unique with a suffix when taken.
    pub fn signup(&self, account_name: &str, email: &str, user_name: &str, password: Option<&str>) -> Result<Signup> {
        let email = email_of(email)?;
        let password_hash = password.map(hash_password).transpose()?;
        let name = name_of(account_name, "the account name")?;
        let user_name = name_of(user_name, "the user name")?;
        if name.is_empty() {
            return Err(Error::Invalid("an account name is required".into()));
        }
        // The address may already be someone's. Sign-up is open to the
        // world, so whoever typed it has to show they are that someone:
        // the user's own password. The check runs with no lock held,
        // since a password hash takes a while and everything else that
        // needs the store would wait behind it.
        let existing: Option<(User, Option<String>)> = {
            let conn = lock(&self.conn);
            conn.query_row(&format!("SELECT {USER_COLS}, password_hash FROM users WHERE email = ?1"), params![email], |r| {
                Ok((user_row(r)?, r.get::<_, Option<String>>(4)?))
            })
            .optional()?
        };
        let proven_hash: Option<String> = match (&existing, password) {
            (Some((_, Some(h))), Some(p)) if password_matches(h, p) => Some(h.clone()),
            (Some((_, Some(_))), _) => {
                return Err(Error::Denied(
                    "this email already belongs to a user; to open another account, sign up with that user's password, or reset it first"
                        .into(),
                ));
            }
            (Some((_, None)), _) => {
                return Err(Error::Denied("this email already belongs to a user who has no password yet; request a password reset link, set one, then sign up with it".into()));
            }
            (None, _) => None,
        };
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let base = slug(&name);
        let base = if base.is_empty() { "account".to_string() } else { base };
        let mut uid = base.clone();
        let mut n = 1;
        while tx.query_row("SELECT 1 FROM accounts WHERE uid = ?1", params![uid], |_| Ok(())).optional()?.is_some() {
            n += 1;
            uid = format!("{base}-{n}");
        }
        tx.execute("INSERT INTO accounts (uid, name) VALUES (?1, ?2)", params![uid, name])?;
        let account = Account { id: tx.last_insert_rowid(), uid, name: name.to_string() };
        let user = match tx
            .query_row(&format!("SELECT {USER_COLS}, password_hash FROM users WHERE email = ?1"), params![email], |r| {
                Ok((user_row(r)?, r.get::<_, Option<String>>(4)?))
            })
            .optional()?
        {
            // The row as it is now, inside the transaction: the password
            // proven a moment ago must still be the one stored, and a
            // user who appeared meanwhile is not ours to claim.
            Some((u, stored)) if stored.is_some() && stored == proven_hash => u,
            Some(_) => return Err(Error::Denied("this email's user changed while signing up; try again".into())),
            None if proven_hash.is_some() => return Err(Error::Denied("this email's user changed while signing up; try again".into())),
            None => {
                tx.execute("INSERT INTO users (email, name, password_hash) VALUES (?1, ?2, ?3)", params![email, user_name, password_hash])?;
                User { id: tx.last_insert_rowid(), email: email.clone(), name: user_name.clone(), email_verified: false }
            }
        };
        tx.execute("INSERT INTO memberships (account_id, user_id, role) VALUES (?1, ?2, 'admin')", params![account.id, user.id])?;
        // The account someone opens themselves is where a login lands.
        tx.execute("UPDATE users SET home_account_id = ?1 WHERE id = ?2", params![account.id, user.id])?;
        let secret = new_secret("zsk");
        tx.execute(
            "INSERT INTO user_keys (user_id, account_id, name, prefix, hash) VALUES (?1, ?2, 'default', ?3, ?4)",
            params![user.id, account.id, prefix_of(&secret), hash(&secret)],
        )?;
        let key_id = tx.last_insert_rowid();
        self.audit(&tx, account.id, &user.email, "signup", &account.uid)?;
        let key = Self::user_key_in(&tx, key_id)?;
        // A new account's address is unproven until someone follows the
        // link; an address already verified through another account
        // needs no second proof.
        let verify_token = if user.email_verified { None } else { self.issue_token(&tx, &user, account.id, TokenKind::Verify)? };
        tx.commit()?;
        Ok(Signup { account, user, key, secret, verify_token })
    }

    // ------------------------------------------------------ authentication

    /// The principal a secret stands for, or none. Touches last-used.
    ///
    /// A hit answers without taking the store's lock at all, which is
    /// what keeps authentication off the hot path. Only a hit answers
    /// from memory: a secret that matches nothing is never cached, so a
    /// stream of made-up keys cannot fill it.
    pub fn authenticate(&self, secret: &str) -> Result<Option<Principal>> {
        let secret = secret.trim();
        let h = hash(secret);
        if let Some(p) = self.hit(|c| c.principals.get(&h).cloned()) {
            return Ok(Some(p));
        }
        let (found, generation) = self.authenticate_uncached(secret, &h)?;
        if let Some(p) = found.clone() {
            self.store_if_current(generation, move |c| {
                c.principals.insert(h, (p, Instant::now()));
            });
        }
        Ok(found)
    }

    /// The read behind [`Store::authenticate`], with the generation it
    /// read at: the caller caches only if that has not moved since.
    /// Also records the key's use, at most once a minute, which now
    /// happens on a cache miss rather than on every request.
    fn authenticate_uncached(&self, secret: &str, h: &str) -> Result<(Option<Principal>, u64)> {
        let conn = lock(&self.conn);
        let generation = self.generation.load(Ordering::Acquire);
        if secret.starts_with("zsk_") {
            let row = conn
                .query_row(
                    "SELECT k.id, k.user_id, k.account_id, k.name, k.prefix, k.created_at, k.last_used_at FROM user_keys k WHERE k.hash = ?1 AND k.revoked_at IS NULL",
                    params![h],
                    |r| Ok(UserKey { id: r.get(0)?, user_id: r.get(1)?, account_id: r.get(2)?, name: r.get(3)?, prefix: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? }),
                )
                .optional()?;
            let Some(key) = row else { return Ok((None, generation)) };
            // A key acts in the account that issued it, and only while
            // its user is still a member there. A key with no account
            // belonged to a user with no membership when the column was
            // added, and answers for nothing.
            let Some(account_id) = key.account_id else { return Ok((None, generation)) };
            touch(&conn, "user_keys", key.id, key.last_used_at.as_deref())?;
            let user = conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![key.user_id], user_row)?;
            let membership = conn
                .query_row(
                    "SELECT a.id, a.uid, a.name, m.role FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 AND m.account_id = ?2",
                    params![user.id, account_id],
                    |r| Ok((Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }, role_of(&r.get::<_, String>(3)?))),
                )
                .optional()?;
            let Some((account, role)) = membership else { return Ok((None, generation)) };
            return Ok((Some(Principal::User { user, account, role, key_id: key.id }), generation));
        }
        if secret.starts_with("zqk_") {
            let row = conn
                .query_row(
                    "SELECT id, account_id, name, prefix, created_by, created_at, last_used_at FROM query_keys WHERE hash = ?1 AND revoked_at IS NULL",
                    params![h],
                    |r| Ok(QueryKey { id: r.get(0)?, account_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_by: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? }),
                )
                .optional()?;
            let Some(key) = row else { return Ok((None, generation)) };
            touch(&conn, "query_keys", key.id, key.last_used_at.as_deref())?;
            let account = conn.query_row("SELECT id, uid, name FROM accounts WHERE id = ?1", params![key.account_id], |r| {
                Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? })
            })?;
            let grants = Self::grants_in(&conn, key.id)?;
            return Ok((Some(Principal::QueryKey { key, account, grants }), generation));
        }
        Ok((None, generation))
    }

    // -------------------------------------------------------- authorization

    /// Whether the principal may do `verb` on the project's branch.
    pub fn authorize(&self, who: &Principal, project: &Project, branch: &str, verb: Verb) -> Result<()> {
        if project.account_id != who.account().id {
            return Err(Error::NotFound(format!("no project {} in this account", project.uid)));
        }
        match who {
            Principal::User { role: Role::Admin, .. } => Ok(()),
            Principal::User { user, .. } => {
                let level = self.level_for(project, user.id)?;
                let needed = match verb {
                    Verb::Read => Level::Read,
                    Verb::Write if project.protected_production && branch == project.production_branch => Level::Owner,
                    Verb::Write => Level::Write,
                    Verb::Owner => Level::Owner,
                };
                match level {
                    Some(l) if l >= needed => Ok(()),
                    Some(_) if needed == Level::Owner && verb == Verb::Write => Err(Error::Denied(format!(
                        "deploying to the protected production branch '{branch}' of {} needs an owner",
                        project.uid
                    ))),
                    Some(l) => Err(Error::Denied(format!(
                        "{} has {} access to {}; this needs {}",
                        user.email,
                        level_name(l),
                        project.uid,
                        level_name(needed)
                    ))),
                    None => Err(Error::Denied(format!("{} has no access to {}", user.email, project.uid))),
                }
            }
            Principal::QueryKey { key, grants, .. } => {
                if verb != Verb::Read {
                    return Err(Error::Denied(format!("query key {} is read only", key.name)));
                }
                let ok = grants.iter().any(|g| {
                    g.project_id == project.id
                        && match g.branch.as_deref() {
                            None => branch == project.production_branch,
                            Some("*") => true,
                            Some(b) => b == branch,
                        }
                });
                if ok {
                    Ok(())
                } else {
                    Err(Error::Denied(format!("query key {} is not granted {}/{}", key.name, project.uid, branch)))
                }
            }
        }
    }

    /// The user's level on a project: granted, else read when the project
    /// is visible to the account.
    pub fn level_for(&self, project: &Project, user_id: i64) -> Result<Option<Level>> {
        let key = (project.id, user_id);
        if let Some(l) = self.hit(|c| c.levels.get(&key).copied()) {
            return Ok(l);
        }
        let (granted, generation) = {
            let conn = lock(&self.conn);
            let generation = self.generation.load(Ordering::Acquire);
            let granted: Option<String> = conn
                .query_row("SELECT level FROM project_access WHERE project_id = ?1 AND user_id = ?2", params![project.id, user_id], |r| {
                    r.get(0)
                })
                .optional()?;
            (granted, generation)
        };
        let level = match granted {
            Some(l) => Some(level_of(&l)),
            None if project.visibility == "account" => Some(Level::Read),
            None => None,
        };
        self.store_if_current(generation, |c| {
            c.levels.insert(key, (level, Instant::now()));
        });
        Ok(level)
    }

    // ------------------------------------------------------------ members

    pub fn members(&self, account_id: i64) -> Result<Vec<Membership>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare("SELECT u.id, u.email, u.name, u.email_verified_at, m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = ?1 ORDER BY u.email")?;
        let rows = st.query_map(params![account_id], |r| Ok(Membership { user: user_row(r)?, role: role_of(&r.get::<_, String>(4)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a user to the account (creating the user by email when new)
    /// with a role, or changes the role of a member.
    pub fn add_member(&self, account_id: i64, actor: &str, email: &str, name: &str, role: Role) -> Result<Membership> {
        let email = email_of(email)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let user = match tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE email = ?1"), params![email], user_row).optional()? {
            Some(u) => u,
            None => {
                tx.execute("INSERT INTO users (email, name) VALUES (?1, ?2)", params![email, name.trim()])?;
                User { id: tx.last_insert_rowid(), email: email.clone(), name: name.trim().to_string(), email_verified: false }
            }
        };
        let role_s = if role == Role::Admin { "admin" } else { "developer" };
        tx.execute(
            "INSERT INTO memberships (account_id, user_id, role) VALUES (?1, ?2, ?3) ON CONFLICT (account_id, user_id) DO UPDATE SET role = excluded.role",
            params![account_id, user.id, role_s],
        )?;
        if Self::admin_count(&tx, account_id)? == 0 {
            return Err(Error::Invalid("an account needs at least one admin".into()));
        }
        self.audit(&tx, account_id, actor, "member", &format!("{} {role_s}", user.email))?;
        tx.commit()?;
        Ok(Membership { user, role })
    }

    pub fn remove_member(&self, account_id: i64, actor: &str, email: &str) -> Result<()> {
        let email = email_of(email)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let user = tx
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE email = ?1"), params![email], user_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no user {email}")))?;
        let n = tx.execute("DELETE FROM memberships WHERE account_id = ?1 AND user_id = ?2", params![account_id, user.id])?;
        if n == 0 {
            return Err(Error::NotFound(format!("{email} is not a member")));
        }
        if Self::admin_count(&tx, account_id)? == 0 {
            return Err(Error::Invalid("an account needs at least one admin".into()));
        }
        // Their keys stop working; their project access goes; projects they
        // owned alone fall to the admins, who own everything anyway.
        tx.execute("UPDATE user_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE user_id = ?1 AND account_id = ?2 AND revoked_at IS NULL", params![user.id, account_id])?;
        tx.execute(
            "DELETE FROM project_access WHERE user_id = ?1 AND project_id IN (SELECT id FROM projects WHERE account_id = ?2)",
            params![user.id, account_id],
        )?;
        self.audit(&tx, account_id, actor, "member_removed", &user.email)?;
        tx.commit()?;
        Ok(())
    }

    /// A one-time token (`zin_...`) the admin hands to a new member, who
    /// redeems it with [`Store::accept_invite`] for a first personal key.
    pub fn create_invite(&self, account_id: i64, actor: &str, email: &str) -> Result<String> {
        let email = email_of(email)?;
        let conn = lock(&self.conn);
        let user = conn
            .query_row(
                &format!("SELECT {USER_COLS} FROM users u WHERE u.email = ?1 AND EXISTS (SELECT 1 FROM memberships m WHERE m.user_id = u.id AND m.account_id = ?2)"),
                params![email, account_id],
                user_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("{email} is not a member")))?;
        let token = new_secret("zin");
        conn.execute(
            "INSERT INTO invites (account_id, user_id, hash, created_by) VALUES (?1, ?2, ?3, ?4)",
            params![account_id, user.id, hash(&token), actor],
        )?;
        self.audit(&conn, account_id, actor, "invite", &user.email)?;
        Ok(token)
    }

    /// Redeems an invite: the member's name is set when given, and a
    /// first personal key is issued. The token stops working.
    ///
    /// An invite is for a newcomer: someone with no password and no key
    /// anywhere. The token passes through the admin who asked for it,
    /// so for a user who can already sign in it proves nothing, and it
    /// is refused rather than let whoever holds it set that user's
    /// password. Such a member is already in the account; they sign in.
    pub fn accept_invite(
        &self,
        token: &str,
        name: &str,
        key_name: &str,
        password: Option<&str>,
    ) -> Result<(Account, User, UserKey, String)> {
        let password_hash = password.map(hash_password).transpose()?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let (invite_id, account_id, user_id): (i64, i64, i64) = tx
            .query_row(
                "SELECT id, account_id, user_id FROM invites WHERE hash = ?1 AND accepted_at IS NULL AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ','now', ?2)",
                params![hash(token.trim()), format!("-{INVITE_DAYS} days")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("this invite is not valid, was already used, or has expired".into()))?;
        if Self::has_credentials(&tx, user_id)? {
            return Err(Error::Denied(
                "this invite is for someone who can already sign in; the membership is in place, so sign in with your own password or key"
                    .into(),
            ));
        }
        tx.execute("UPDATE invites SET accepted_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![invite_id])?;
        if !name.trim().is_empty() {
            tx.execute("UPDATE users SET name = ?1 WHERE id = ?2", params![name.trim(), user_id])?;
        }
        if let Some(h) = &password_hash {
            tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![h, user_id])?;
        }
        tx.execute("UPDATE users SET home_account_id = ?1 WHERE id = ?2 AND home_account_id IS NULL", params![account_id, user_id])?;
        let user = tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let account = tx.query_row("SELECT id, uid, name FROM accounts WHERE id = ?1", params![account_id], |r| {
            Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? })
        })?;
        let key_name = if key_name.trim().is_empty() { "default" } else { key_name.trim() };
        let secret = new_secret("zsk");
        tx.execute(
            "INSERT INTO user_keys (user_id, account_id, name, prefix, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user.id, account_id, key_name, prefix_of(&secret), hash(&secret)],
        )?;
        let key = Self::user_key_in(&tx, tx.last_insert_rowid())?;
        self.audit(&tx, account.id, &user.email, "invite_accepted", &account.uid)?;
        tx.commit()?;
        Ok((account, user, key, secret))
    }

    fn admin_count(conn: &Connection, account_id: i64) -> rusqlite::Result<i64> {
        conn.query_row("SELECT count(*) FROM memberships WHERE account_id = ?1 AND role = 'admin'", params![account_id], |r| r.get(0))
    }

    // ----------------------------------------------------------- user keys

    fn user_key_in(conn: &Connection, id: i64) -> Result<UserKey> {
        Ok(conn.query_row(
            "SELECT id, user_id, account_id, name, prefix, created_at, last_used_at FROM user_keys WHERE id = ?1",
            params![id],
            |r| {
                Ok(UserKey {
                    id: r.get(0)?,
                    user_id: r.get(1)?,
                    account_id: r.get(2)?,
                    name: r.get(3)?,
                    prefix: r.get(4)?,
                    created_at: r.get(5)?,
                    last_used_at: r.get(6)?,
                })
            },
        )?)
    }

    pub fn user_key(&self, id: i64) -> Result<UserKey> {
        let conn = lock(&self.conn);
        Self::user_key_in(&conn, id)
    }

    /// The user's live keys in this account.
    pub fn user_keys(&self, account_id: i64, user_id: i64) -> Result<Vec<UserKey>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare("SELECT id, user_id, account_id, name, prefix, created_at, last_used_at FROM user_keys WHERE user_id = ?1 AND account_id = ?2 AND revoked_at IS NULL ORDER BY id")?;
        let rows = st.query_map(params![user_id, account_id], |r| {
            Ok(UserKey {
                id: r.get(0)?,
                user_id: r.get(1)?,
                account_id: r.get(2)?,
                name: r.get(3)?,
                prefix: r.get(4)?,
                created_at: r.get(5)?,
                last_used_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the user can already sign in some way: a password, or a
    /// live key in any account. Such a user is not a newcomer, needs no
    /// invite token, and may not be set up by one.
    pub fn can_sign_in(&self, user_id: i64) -> Result<bool> {
        let conn = lock(&self.conn);
        Ok(Self::has_credentials(&conn, user_id)?)
    }

    fn has_credentials(conn: &Connection, user_id: i64) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT (password_hash IS NOT NULL) OR EXISTS (SELECT 1 FROM user_keys k WHERE k.user_id = users.id AND k.revoked_at IS NULL) FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get::<_, bool>(0),
        )
    }

    /// A new personal key; the secret is returned once.
    pub fn create_user_key(&self, account_id: i64, user: &User, name: &str) -> Result<(UserKey, String)> {
        let name = name_of(name, "the key name")?;
        let name = name.as_str();
        if name.is_empty() {
            return Err(Error::Invalid("a key name is required".into()));
        }
        let secret = new_secret("zsk");
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO user_keys (user_id, account_id, name, prefix, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user.id, account_id, name, prefix_of(&secret), hash(&secret)],
        )?;
        let id = conn.last_insert_rowid();
        self.audit(&conn, account_id, &user.email, "user_key_created", &format!("{name} {}", prefix_of(&secret)))?;
        Ok((Self::user_key_in(&conn, id)?, secret))
    }

    pub fn revoke_user_key(&self, account_id: i64, user: &User, key_id: i64) -> Result<()> {
        let conn = lock(&self.conn);
        let n = conn.execute(
            "UPDATE user_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL",
            params![key_id, user.id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("no active key {key_id} for {}", user.email)));
        }
        self.audit(&conn, account_id, &user.email, "user_key_revoked", &key_id.to_string())?;
        Ok(())
    }

    // ---------------------------------------------------------- query keys

    fn query_key_in(conn: &Connection, account_id: i64, id: i64) -> Result<QueryKey> {
        conn.query_row(
            "SELECT id, account_id, name, prefix, created_by, created_at, last_used_at FROM query_keys WHERE id = ?1 AND account_id = ?2 AND revoked_at IS NULL",
            params![id, account_id],
            |r| Ok(QueryKey { id: r.get(0)?, account_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_by: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? }),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("no query key {id}")))
    }

    pub fn query_key(&self, account_id: i64, id: i64) -> Result<QueryKey> {
        let conn = lock(&self.conn);
        Self::query_key_in(&conn, account_id, id)
    }

    pub fn query_keys(&self, account_id: i64) -> Result<Vec<QueryKey>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare("SELECT id, account_id, name, prefix, created_by, created_at, last_used_at FROM query_keys WHERE account_id = ?1 AND revoked_at IS NULL ORDER BY id")?;
        let rows = st.query_map(params![account_id], |r| {
            Ok(QueryKey {
                id: r.get(0)?,
                account_id: r.get(1)?,
                name: r.get(2)?,
                prefix: r.get(3)?,
                created_by: r.get(4)?,
                created_at: r.get(5)?,
                last_used_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A new query key with no grants; the secret is returned once.
    pub fn create_query_key(&self, account_id: i64, actor: &User, name: &str) -> Result<(QueryKey, String)> {
        let name = name_of(name, "the key name")?;
        let name = name.as_str();
        if name.is_empty() {
            return Err(Error::Invalid("a key name is required".into()));
        }
        let secret = new_secret("zqk");
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO query_keys (account_id, name, prefix, hash, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, name, prefix_of(&secret), hash(&secret), actor.id],
        )?;
        let id = conn.last_insert_rowid();
        self.audit(&conn, account_id, &actor.email, "query_key_created", &format!("{name} {}", prefix_of(&secret)))?;
        Ok((Self::query_key_in(&conn, account_id, id)?, secret))
    }

    /// A new secret for the same key and grants; the old secret stops working.
    pub fn rotate_query_key(&self, account_id: i64, actor: &User, id: i64) -> Result<(QueryKey, String)> {
        let secret = new_secret("zqk");
        let conn = lock(&self.conn);
        let key = Self::query_key_in(&conn, account_id, id)?;
        conn.execute("UPDATE query_keys SET prefix = ?1, hash = ?2 WHERE id = ?3", params![prefix_of(&secret), hash(&secret), key.id])?;
        self.audit(&conn, account_id, &actor.email, "query_key_rotated", &format!("{} {}", key.name, prefix_of(&secret)))?;
        Ok((Self::query_key_in(&conn, account_id, id)?, secret))
    }

    pub fn revoke_query_key(&self, account_id: i64, actor: &User, id: i64) -> Result<()> {
        let conn = lock(&self.conn);
        let key = Self::query_key_in(&conn, account_id, id)?;
        conn.execute("UPDATE query_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![key.id])?;
        self.audit(&conn, account_id, &actor.email, "query_key_revoked", &key.name)?;
        Ok(())
    }

    fn grants_in(conn: &Connection, key_id: i64) -> rusqlite::Result<Vec<Grant>> {
        let mut st = conn.prepare("SELECT g.project_id, p.uid, g.branch, g.context FROM query_key_grants g JOIN projects p ON p.id = g.project_id WHERE g.query_key_id = ?1 ORDER BY p.uid")?;
        let rows = st.query_map(params![key_id], |r| {
            Ok(Grant {
                project_id: r.get(0)?,
                project_uid: r.get(1)?,
                branch: r.get(2)?,
                context: context_of(r.get::<_, Option<String>>(3)?),
            })
        })?;
        rows.collect()
    }

    pub fn grants(&self, account_id: i64, key_id: i64) -> Result<Vec<Grant>> {
        let conn = lock(&self.conn);
        Self::query_key_in(&conn, account_id, key_id)?;
        Ok(Self::grants_in(&conn, key_id)?)
    }

    /// Grants the key read on a project: its production branch when
    /// `branch` is `None`, every branch for `*`, else the named branch.
    /// With a `context`, the key plans on this project as that context
    /// and nothing the caller sends can change it; without one the
    /// grant is unbound, which also clears a context set earlier, so a
    /// grant is always exactly what its last call said.
    pub fn grant(
        &self,
        account_id: i64,
        actor: &User,
        key_id: i64,
        project: &Project,
        branch: Option<&str>,
        context: Option<&serde_json::Value>,
    ) -> Result<Grant> {
        let conn = lock(&self.conn);
        let key = Self::query_key_in(&conn, account_id, key_id)?;
        let branch = branch.map(str::trim).filter(|b| !b.is_empty()).map(str::to_string);
        let context_text = context.map(serde_json::Value::to_string);
        conn.execute(
            "INSERT INTO query_key_grants (query_key_id, project_id, branch, context) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (query_key_id, project_id) DO UPDATE SET branch = excluded.branch, context = excluded.context",
            params![key.id, project.id, branch, context_text],
        )?;
        let bound = if context.is_some() { " with a bound context" } else { "" };
        self.audit(
            &conn,
            account_id,
            &actor.email,
            "grant",
            &format!("{} on {}/{}{bound}", key.name, project.uid, branch.as_deref().unwrap_or(&project.production_branch)),
        )?;
        Ok(Grant { project_id: project.id, project_uid: project.uid.clone(), branch, context: context.cloned() })
    }

    pub fn revoke_grant(&self, account_id: i64, actor: &User, key_id: i64, project: &Project) -> Result<()> {
        let conn = lock(&self.conn);
        let key = Self::query_key_in(&conn, account_id, key_id)?;
        let n = conn.execute("DELETE FROM query_key_grants WHERE query_key_id = ?1 AND project_id = ?2", params![key.id, project.id])?;
        if n == 0 {
            return Err(Error::NotFound(format!("{} is not granted {}", key.name, project.uid)));
        }
        self.audit(&conn, account_id, &actor.email, "grant_revoked", &format!("{} on {}", key.name, project.uid))?;
        Ok(())
    }

    /// Every key granted on a project, for its owners to see.
    pub fn keys_granted_on(&self, project: &Project) -> Result<Vec<(QueryKey, Grant)>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare(
            "SELECT k.id, k.account_id, k.name, k.prefix, k.created_by, k.created_at, k.last_used_at, g.branch, g.context FROM query_key_grants g JOIN query_keys k ON k.id = g.query_key_id WHERE g.project_id = ?1 AND k.revoked_at IS NULL ORDER BY k.name",
        )?;
        let rows = st.query_map(params![project.id], |r| {
            Ok((
                QueryKey {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    name: r.get(2)?,
                    prefix: r.get(3)?,
                    created_by: r.get(4)?,
                    created_at: r.get(5)?,
                    last_used_at: r.get(6)?,
                },
                Grant {
                    project_id: project.id,
                    project_uid: project.uid.clone(),
                    branch: r.get(7)?,
                    context: context_of(r.get::<_, Option<String>>(8)?),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------ projects

    pub fn project(&self, account_id: i64, uid: &str) -> Result<Option<Project>> {
        let uid = uid.trim().to_lowercase();
        let key = (account_id, uid);
        if let Some(p) = self.hit(|c| c.projects.get(&key).cloned()) {
            return Ok(Some(p));
        }
        let (found, generation) = {
            let conn = lock(&self.conn);
            let generation = self.generation.load(Ordering::Acquire);
            let found = conn
                .query_row(
                    &format!("SELECT {PROJECT_COLS} FROM projects WHERE account_id = ?1 AND uid = ?2"),
                    params![key.0, key.1],
                    project_row,
                )
                .optional()?;
            (found, generation)
        };
        // A uid that names nothing is not cached: the caller chooses it,
        // so caching misses would let one fill the map with nonsense.
        if let Some(p) = found.clone() {
            self.store_if_current(generation, move |c| {
                c.projects.insert(key, (p, Instant::now()));
            });
        }
        Ok(found)
    }

    pub fn projects(&self, account_id: i64) -> Result<Vec<Project>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare(&format!("SELECT {PROJECT_COLS} FROM projects WHERE account_id = ?1 ORDER BY uid"))?;
        let rows = st.query_map(params![account_id], project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A project the creator owns. The uid must be a slug.
    pub fn create_project(&self, account_id: i64, creator: &User, uid: &str, name: &str, production_branch: &str) -> Result<Project> {
        let uid = uid.trim().to_lowercase();
        if uid.is_empty() || slug(&uid) != uid {
            return Err(Error::Invalid(format!("'{uid}' is not a project uid (lower-case letters, digits, - and _)")));
        }
        let production_branch = production_branch.trim();
        let production_branch = if production_branch.is_empty() { "main" } else { production_branch };
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO projects (account_id, uid, name, production_branch, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, uid, name.trim(), production_branch, creator.id],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO project_access (project_id, user_id, level) VALUES (?1, ?2, 'owner')", params![id, creator.id])?;
        self.audit(&tx, account_id, &creator.email, "project_created", &uid)?;
        let p = tx.query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"), params![id], project_row)?;
        tx.commit()?;
        Ok(p)
    }

    pub fn update_project(
        &self,
        actor: &User,
        project: &Project,
        name: Option<&str>,
        production_branch: Option<&str>,
        visibility: Option<&str>,
        protected_production: Option<bool>,
    ) -> Result<Project> {
        if let Some(v) = visibility {
            if !matches!(v, "account" | "restricted") {
                return Err(Error::Invalid(format!("'{v}' is not a visibility (account, restricted)")));
            }
        }
        let conn = lock(&self.conn);
        conn.execute(
            "UPDATE projects SET name = coalesce(?1, name), production_branch = coalesce(?2, production_branch), visibility = coalesce(?3, visibility), protected_production = coalesce(?4, protected_production) WHERE id = ?5",
            params![name.map(str::trim), production_branch.map(str::trim).filter(|b| !b.is_empty()), visibility, protected_production.map(|b| b as i64), project.id],
        )?;
        self.audit(&conn, project.account_id, &actor.email, "project_updated", &project.uid)?;
        Ok(conn.query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"), params![project.id], project_row)?)
    }

    pub fn delete_project(&self, actor: &User, project: &Project) -> Result<()> {
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM query_key_grants WHERE project_id = ?1", params![project.id])?;
        tx.execute("DELETE FROM project_access WHERE project_id = ?1", params![project.id])?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![project.id])?;
        self.audit(&tx, project.account_id, &actor.email, "project_deleted", &project.uid)?;
        tx.commit()?;
        Ok(())
    }

    pub fn project_access(&self, project: &Project) -> Result<Vec<(User, Level)>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare("SELECT u.id, u.email, u.name, u.email_verified_at, a.level FROM project_access a JOIN users u ON u.id = a.user_id WHERE a.project_id = ?1 ORDER BY u.email")?;
        let rows = st.query_map(params![project.id], |r| Ok((user_row(r)?, level_of(&r.get::<_, String>(4)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sets a member's level on a project. The user must be a member of
    /// the account; a project keeps at least one owner.
    pub fn set_access(&self, actor: &User, project: &Project, email: &str, level: Level) -> Result<(User, Level)> {
        let email = email_of(email)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let user = tx
            .query_row(
                &format!("SELECT {USER_COLS} FROM users u WHERE u.email = ?1 AND EXISTS (SELECT 1 FROM memberships m WHERE m.user_id = u.id AND m.account_id = ?2)"),
                params![email, project.account_id],
                user_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("{email} is not a member of the account")))?;
        tx.execute(
            "INSERT INTO project_access (project_id, user_id, level) VALUES (?1, ?2, ?3) ON CONFLICT (project_id, user_id) DO UPDATE SET level = excluded.level",
            params![project.id, user.id, level_name(level)],
        )?;
        if Self::owner_count(&tx, project.id)? == 0 {
            return Err(Error::Invalid(format!("{} needs at least one owner", project.uid)));
        }
        self.audit(&tx, project.account_id, &actor.email, "access", &format!("{} {} on {}", user.email, level_name(level), project.uid))?;
        tx.commit()?;
        Ok((user, level))
    }

    pub fn remove_access(&self, actor: &User, project: &Project, email: &str) -> Result<()> {
        let email = email_of(email)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM project_access WHERE project_id = ?1 AND user_id = (SELECT id FROM users WHERE email = ?2)",
            params![project.id, email],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("{email} has no granted access to {}", project.uid)));
        }
        if Self::owner_count(&tx, project.id)? == 0 {
            return Err(Error::Invalid(format!("{} needs at least one owner", project.uid)));
        }
        self.audit(&tx, project.account_id, &actor.email, "access_removed", &format!("{email} on {}", project.uid))?;
        tx.commit()?;
        Ok(())
    }

    fn owner_count(conn: &Connection, project_id: i64) -> rusqlite::Result<i64> {
        conn.query_row("SELECT count(*) FROM project_access WHERE project_id = ?1 AND level = 'owner'", params![project_id], |r| r.get(0))
    }

    // ----------------------------------------------------- passwords, sessions

    /// Sets the password and ends the user's other sessions: someone
    /// changing a password they think is known keeps only the session
    /// they are doing it from, `keep`, when the caller has one.
    /// `current` is the password as it is, which a user who has one must
    /// give: a personal key alone may not take the web app over.
    pub fn set_password(&self, account_id: i64, user: &User, password: &str, current: Option<&str>, keep: Option<&str>) -> Result<()> {
        let stored: Option<String> = {
            let conn = lock(&self.conn);
            conn.query_row("SELECT password_hash FROM users WHERE id = ?1", params![user.id], |r| r.get(0)).optional()?.flatten()
        };
        if let Some(h) = &stored {
            match current {
                Some(c) if password_matches(h, c) => {}
                _ => return Err(Error::Denied("the current password is needed to set a new one".into())),
            }
        }
        let h = hash_password(password)?;
        let conn = lock(&self.conn);
        conn.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![h, user.id])?;
        conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND hash != ?2",
            params![user.id, keep.map(|t| hash(t.trim())).unwrap_or_default()],
        )?;
        self.audit(&conn, account_id, &user.email, "password_set", "")?;
        Ok(())
    }

    /// The user when the email and password match; `None` otherwise,
    /// including for users who never set a password.
    pub fn verify_password(&self, email: &str, password: &str) -> Result<Option<User>> {
        let Ok(email) = email_of(email) else { return Ok(None) };
        // The row is read under the lock; the hash is checked without
        // it, since that takes a while and everyone else waits behind it.
        let row: Option<(User, Option<String>)> = {
            let conn = lock(&self.conn);
            conn.query_row(&format!("SELECT {USER_COLS}, password_hash FROM users WHERE email = ?1"), params![email], |r| {
                Ok((user_row(r)?, r.get(4)?))
            })
            .optional()?
        };
        // An unknown address, or one with no password, is answered in
        // the time a wrong password takes, so the clock does not say
        // which addresses are registered.
        let Some((user, Some(h))) = row else {
            password_matches(&self.decoy, password);
            return Ok(None);
        };
        Ok(password_matches(&h, password).then_some(user))
    }

    /// A browser session for the user on an account; the token is the
    /// cookie value, stored hashed, good for `days`.
    pub fn create_session(&self, user: &User, days: i64) -> Result<(String, Account)> {
        let conn = lock(&self.conn);
        // The home account when the user is still a member of it: the
        // one they opened, or were first invited into. Else the oldest
        // membership, which is what it always was.
        let account = conn
            .query_row(
                "SELECT a.id, a.uid, a.name FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 ORDER BY (a.id = (SELECT home_account_id FROM users WHERE id = ?1)) DESC, a.id LIMIT 1",
                params![user.id],
                |r| Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }),
            )
            .optional()?
            .ok_or_else(|| Error::Denied(format!("{} belongs to no account", user.email)))?;
        let token = new_secret("zss");
        conn.execute("DELETE FROM sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%SZ','now')", [])?;
        conn.execute(
            "INSERT INTO sessions (user_id, account_id, hash, expires_at) VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?4))",
            params![user.id, account.id, hash(&token), format!("+{days} days")],
        )?;
        self.audit(&conn, account.id, &user.email, "login", "")?;
        Ok((token, account))
    }

    /// The principal a session cookie stands for, if it is live. Cached
    /// like a key, and dropped by logout and by every change.
    pub fn session(&self, token: &str) -> Result<Option<Principal>> {
        let h = hash(token.trim());
        if let Some(p) = self.hit(|c| c.sessions.get(&h).cloned()) {
            return Ok(Some(p));
        }
        let (found, generation) = self.session_uncached(&h)?;
        if let Some(p) = found.clone() {
            self.store_if_current(generation, move |c| {
                c.sessions.insert(h, (p, Instant::now()));
            });
        }
        Ok(found)
    }

    fn session_uncached(&self, h: &str) -> Result<(Option<Principal>, u64)> {
        let conn = lock(&self.conn);
        let generation = self.generation.load(Ordering::Acquire);
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT user_id, account_id FROM sessions WHERE hash = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((user_id, account_id)) = row else { return Ok((None, generation)) };
        let user = conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let membership = conn
            .query_row(
                "SELECT a.id, a.uid, a.name, m.role FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 AND m.account_id = ?2",
                params![user_id, account_id],
                |r| Ok((Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }, role_of(&r.get::<_, String>(3)?))),
            )
            .optional()?;
        let Some((account, role)) = membership else { return Ok((None, generation)) };
        Ok((Some(Principal::User { user, account, role, key_id: 0 }), generation))
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute("DELETE FROM sessions WHERE hash = ?1", params![hash(token.trim())])?;
        self.bump();
        Ok(())
    }

    // ------------------------------------------- verification and reset

    /// The account a user acts in: the first membership, as elsewhere
    /// here while a user belongs to one account at a time.
    fn first_account(conn: &Connection, user_id: i64) -> Result<Account> {
        conn.query_row(
            "SELECT a.id, a.uid, a.name FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 ORDER BY a.id LIMIT 1",
            params![user_id],
            |r| Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }),
        )
        .optional()?
        .ok_or_else(|| Error::Denied("this user belongs to no account".into()))
    }

    /// A fresh link token, or `None` when one was issued moments ago.
    ///
    /// Issuing invalidates every unused token of the same kind for that
    /// user, so the newest link is always the only one that works: a
    /// second request does not leave the first link live.
    fn issue_token(&self, conn: &Connection, user: &User, account_id: i64, kind: TokenKind) -> Result<Option<String>> {
        // Expired rows are of no use to anyone; clearing them here keeps
        // the table the size of the links actually in flight.
        conn.execute("DELETE FROM email_tokens WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%SZ','now') OR used_at IS NOT NULL", [])?;
        let recent: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM email_tokens WHERE user_id = ?1 AND kind = ?2
                   AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ','now', ?3) LIMIT 1",
                params![user.id, kind.as_str(), format!("-{TOKEN_COOLDOWN_SECONDS} seconds")],
                |r| r.get(0),
            )
            .optional()?;
        if recent.is_some() {
            return Ok(None);
        }
        conn.execute("DELETE FROM email_tokens WHERE user_id = ?1 AND kind = ?2", params![user.id, kind.as_str()])?;
        let token = new_secret(kind.secret_kind());
        conn.execute(
            "INSERT INTO email_tokens (user_id, account_id, kind, hash, email, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?6))",
            params![user.id, account_id, kind.as_str(), hash(&token), user.email, format!("+{} minutes", kind.minutes())],
        )?;
        Ok(Some(token))
    }

    /// Redeems a link: its row, if the token is of that kind, unused,
    /// unexpired, and still names the user's current address. Marks it
    /// used, so a link works exactly once.
    fn take_token(tx: &Connection, kind: TokenKind, token: &str) -> Result<(i64, i64, String)> {
        let stale = || Error::NotFound("this link is not valid, was already used, or has expired".into());
        let (id, user_id, account_id, email): (i64, i64, i64, String) = tx
            .query_row(
                "SELECT id, user_id, account_id, email FROM email_tokens
                  WHERE hash = ?1 AND kind = ?2 AND used_at IS NULL
                    AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')",
                params![hash(token.trim()), kind.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(stale)?;
        // The link is for the address it was sent to. If the account's
        // address has changed since, the old mailbox no longer speaks
        // for it.
        let current: String = tx.query_row("SELECT email FROM users WHERE id = ?1", params![user_id], |r| r.get(0))?;
        if current != email {
            return Err(stale());
        }
        tx.execute("UPDATE email_tokens SET used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![id])?;
        Ok((user_id, account_id, email))
    }

    /// A verification link for a user's own address: `None` when it is
    /// already verified, or when one was issued moments ago.
    pub fn start_verification(&self, account_id: i64, user: &User) -> Result<Option<String>> {
        if user.email_verified {
            return Ok(None);
        }
        let conn = lock(&self.conn);
        let token = self.issue_token(&conn, user, account_id, TokenKind::Verify)?;
        if token.is_some() {
            self.audit(&conn, account_id, &user.email, "verification_sent", &user.email)?;
        }
        Ok(token)
    }

    /// Marks the address proven. The link stops working.
    pub fn verify_email(&self, token: &str) -> Result<(Account, User)> {
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let (user_id, account_id, _) = Self::take_token(&tx, TokenKind::Verify, token)?;
        tx.execute(
            "UPDATE users SET email_verified_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1 AND email_verified_at IS NULL",
            params![user_id],
        )?;
        let user = tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let account = Self::first_account(&tx, user_id)?;
        self.audit(&tx, account_id, &user.email, "email_verified", &user.email)?;
        tx.commit()?;
        Ok((account, user))
    }

    /// A reset link for an address, or `None` when no user has it, when
    /// they belong to no account, or when one was issued moments ago.
    ///
    /// The three cases are deliberately one answer: the endpoint says the
    /// same thing either way, so nobody can ask this which addresses have
    /// accounts.
    pub fn start_reset(&self, email: &str) -> Result<Option<(Account, User, String)>> {
        let Ok(email) = email_of(email) else { return Ok(None) };
        let conn = lock(&self.conn);
        let Some(user) = conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE email = ?1"), params![email], user_row).optional()?
        else {
            return Ok(None);
        };
        let Ok(account) = Self::first_account(&conn, user.id) else { return Ok(None) };
        let Some(token) = self.issue_token(&conn, &user, account.id, TokenKind::Reset)? else {
            return Ok(None);
        };
        self.audit(&conn, account.id, &user.email, "reset_requested", &user.email)?;
        Ok(Some((account, user, token)))
    }

    /// Sets a password from a reset link.
    ///
    /// Three things happen besides the password. The link stops working,
    /// so it cannot be replayed from a mailbox later. Every session of
    /// that user is dropped, since whoever knew the old password may be
    /// signed in. And the address counts as verified, because following
    /// a link sent to it is the same proof verification asks for.
    ///
    /// Personal keys are left alone: a forgotten password is not a
    /// reason to break the user's machines, and the keys are revoked
    /// from the account pages when it is.
    pub fn reset_password(&self, token: &str, password: &str) -> Result<(Account, User)> {
        let password_hash = hash_password(password)?;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        let (user_id, account_id, _) = Self::take_token(&tx, TokenKind::Reset, token)?;
        tx.execute(
            "UPDATE users SET password_hash = ?1, email_verified_at = coalesce(email_verified_at, strftime('%Y-%m-%dT%H:%M:%SZ','now')) WHERE id = ?2",
            params![password_hash, user_id],
        )?;
        tx.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
        let user = tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let account = Self::first_account(&tx, user_id)?;
        self.audit(&tx, account_id, &user.email, "password_reset", &user.email)?;
        tx.commit()?;
        Ok((account, user))
    }

    // --------------------------------------------------------------- audit

    pub fn record(&self, account_id: i64, actor: &str, action: &str, subject: &str) -> Result<()> {
        let conn = lock(&self.conn);
        Ok(self.audit(&conn, account_id, actor, action, subject)?)
    }

    pub fn audit_log(&self, account_id: i64, limit: usize) -> Result<Vec<serde_json::Value>> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare("SELECT actor, action, subject, at FROM audit WHERE account_id = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = st.query_map(params![account_id, limit as i64], |r| {
            Ok(serde_json::json!({ "actor": r.get::<_, String>(0)?, "action": r.get::<_, String>(1)?, "subject": r.get::<_, String>(2)?, "at": r.get::<_, String>(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(store: &Store, secret: &str) -> Principal {
        store.authenticate(secret).unwrap().expect("a live key")
    }

    /// Authentication is cached, so every change that could make a
    /// cached answer wrong has to be felt at once. Each step here would
    /// pass trivially with the cache off; with it on, each one is a
    /// check that the change invalidated.
    #[test]
    fn a_change_is_felt_at_once_through_the_cache() {
        let s = Store::in_memory().unwrap();
        assert!(!s.ttl.is_zero(), "this test is only meaningful with the cache on");
        let su = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let ann = su.user.clone();
        let bob = s.add_member(su.account.id, &ann.email, "bob@acme.test", "Bob", Role::Developer).unwrap();
        let (bob_key, bob_secret) = s.create_user_key(su.account.id, &bob.user, "laptop").unwrap();

        // Warm the cache, and check it is really holding the answer.
        assert!(s.authenticate(&bob_secret).unwrap().is_some());
        assert_eq!(read(&s.cache).principals.len(), 1, "the decision is cached");
        assert!(s.authenticate(&bob_secret).unwrap().is_some(), "a second ask is served from memory");

        // Revoking the key stops it on the next request, not in a minute.
        s.revoke_user_key(su.account.id, &bob.user, bob_key.id).unwrap();
        assert!(s.authenticate(&bob_secret).unwrap().is_none(), "a revoked key stops at once");

        // A query key: rotation keeps the grants and kills the old secret.
        let project = s.create_project(su.account.id, &ann, "sales", "Sales", "main").unwrap();
        let (qk, old_secret) = s.create_query_key(su.account.id, &ann, "web app").unwrap();
        s.grant(su.account.id, &ann, qk.id, &project, None, None).unwrap();
        let before = principal(&s, &old_secret);
        s.authorize(&before, &project, "main", Verb::Read).unwrap();
        let (_, new_secret) = s.rotate_query_key(su.account.id, &ann, qk.id).unwrap();
        assert!(s.authenticate(&old_secret).unwrap().is_none(), "the rotated-away secret stops at once");
        let after = principal(&s, &new_secret);
        s.authorize(&after, &project, "main", Verb::Read).unwrap();

        // Revoking the grant is felt by the next principal read.
        s.revoke_grant(su.account.id, &ann, qk.id, &project).unwrap();
        let after = principal(&s, &new_secret);
        assert!(matches!(s.authorize(&after, &project, "main", Verb::Read), Err(Error::Denied(_))));

        // Levels and visibility are cached beside the principal.
        let (_, bob_secret) = s.create_user_key(su.account.id, &bob.user, "desk").unwrap();
        let bob_p = principal(&s, &bob_secret);
        assert_eq!(s.level_for(&project, bob.user.id).unwrap(), Some(Level::Read), "visible to the account");
        s.set_access(&ann, &project, "bob@acme.test", Level::Write).unwrap();
        assert_eq!(s.level_for(&project, bob.user.id).unwrap(), Some(Level::Write), "a new level is felt at once");
        let project = s.update_project(&ann, &project, None, None, Some("restricted"), None).unwrap();
        s.remove_access(&ann, &project, "bob@acme.test").unwrap();
        assert_eq!(s.level_for(&project, bob.user.id).unwrap(), None, "restricted and ungranted");
        assert!(matches!(s.authorize(&bob_p, &project, "main", Verb::Read), Err(Error::Denied(_))));

        // The project lookup is cached too, and a rename is felt.
        assert_eq!(s.project(su.account.id, "sales").unwrap().unwrap().name, "Sales");
        s.update_project(&ann, &project, Some("Revenue"), None, None, None).unwrap();
        assert_eq!(s.project(su.account.id, "sales").unwrap().unwrap().name, "Revenue");

        // Sessions: logging out ends the session now.
        let (cookie, _) = s.create_session(&ann, 30).unwrap();
        assert!(s.session(&cookie).unwrap().is_some());
        assert_eq!(read(&s.cache).sessions.len(), 1);
        s.delete_session(&cookie).unwrap();
        assert!(s.session(&cookie).unwrap().is_none(), "logout ends it at once");

        // Removing a member stops the keys they still hold.
        let (_, bob_secret) = s.create_user_key(su.account.id, &bob.user, "third").unwrap();
        assert!(s.authenticate(&bob_secret).unwrap().is_some());
        s.remove_member(su.account.id, &ann.email, "bob@acme.test").unwrap();
        assert!(s.authenticate(&bob_secret).unwrap().is_none(), "a removed member's key stops at once");
    }

    /// A secret that matches nothing must not be kept: the caller picks
    /// it, so caching misses would be a way to fill memory.
    #[test]
    fn unknown_secrets_are_not_cached() {
        let s = Store::in_memory().unwrap();
        for i in 0..50 {
            assert!(s.authenticate(&format!("zsk_nonsense{i}")).unwrap().is_none());
        }
        assert!(read(&s.cache).principals.is_empty());
    }

    #[test]
    fn signup_admin_developer_and_project_access() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme Corp", "Ann@acme.com", "Ann", Some("hunter2hunter2")).unwrap();
        assert_eq!(su.account.uid, "acme-corp");
        assert!(su.secret.starts_with("zsk_"));
        let ann = principal(&s, &su.secret);
        assert!(matches!(ann, Principal::User { role: Role::Admin, .. }));

        // A second account with the same name gets a suffix.
        assert_eq!(s.signup("Acme Corp", "other@x.com", "", None).unwrap().account.uid, "acme-corp-2");

        let bob = s.add_member(su.account.id, "ann@acme.com", "bob@acme.com", "", Role::Developer).unwrap();
        // Bob gets his first key by redeeming an invite; the token is one-time.
        let token = s.create_invite(su.account.id, "ann@acme.com", "bob@acme.com").unwrap();
        let (_, bob_user, first, bob_secret) = s.accept_invite(&token, "Bob", "", Some("bobpassword")).unwrap();
        assert_eq!((bob_user.name.as_str(), first.name.as_str()), ("Bob", "default"));
        assert!(matches!(s.accept_invite(&token, "", "", None), Err(Error::NotFound(_))));
        let (bob_key, _) = s.create_user_key(su.account.id, &bob.user, "laptop").unwrap();
        assert_eq!(bob_key.name, "laptop");
        let bob_p = principal(&s, &bob_secret);

        // Ann creates a project; Bob reads it by account visibility, cannot write.
        let sales = s.create_project(su.account.id, &su.user, "sales", "Sales", "main").unwrap();
        s.authorize(&bob_p, &sales, "main", Verb::Read).unwrap();
        assert!(matches!(s.authorize(&bob_p, &sales, "main", Verb::Write), Err(Error::Denied(_))));
        // Granted write, Bob deploys; the protected production branch still needs an owner.
        s.set_access(&su.user, &sales, "bob@acme.com", Level::Write).unwrap();
        s.authorize(&bob_p, &sales, "feature", Verb::Write).unwrap();
        let sales = s.update_project(&su.user, &sales, None, None, None, Some(true)).unwrap();
        assert!(matches!(s.authorize(&bob_p, &sales, "main", Verb::Write), Err(Error::Denied(_))));
        s.authorize(&bob_p, &sales, "feature", Verb::Write).unwrap();
        // Restricted projects hide from developers without a grant.
        let secret_p = s.create_project(su.account.id, &su.user, "secret", "", "").unwrap();
        let secret_p = s.update_project(&su.user, &secret_p, None, None, Some("restricted"), None).unwrap();
        assert!(matches!(s.authorize(&bob_p, &secret_p, "main", Verb::Read), Err(Error::Denied(_))));
        // Bob's own project: he owns it and can grant.
        let bobs = s.create_project(su.account.id, &bob.user, "bobs", "", "").unwrap();
        s.authorize(&bob_p, &bobs, "main", Verb::Owner).unwrap();
        // The last owner cannot be demoted.
        assert!(matches!(s.set_access(&bob.user, &bobs, "bob@acme.com", Level::Read), Err(Error::Invalid(_))));
        // Admins own everything.
        s.authorize(&ann, &bobs, "main", Verb::Owner).unwrap();
        // The last admin cannot leave.
        assert!(matches!(s.remove_member(su.account.id, "ann@acme.com", "ann@acme.com"), Err(Error::Invalid(_))));
        // Passwords and sessions.
        assert!(s.verify_password("ann@acme.com", "wrong").unwrap().is_none());
        let ann_user = s.verify_password("ann@acme.com", "hunter2hunter2").unwrap().expect("ann's password");
        let (cookie, acct) = s.create_session(&ann_user, 30).unwrap();
        assert_eq!(acct.uid, "acme-corp");
        assert!(matches!(s.session(&cookie).unwrap(), Some(Principal::User { role: Role::Admin, .. })));
        s.delete_session(&cookie).unwrap();
        assert!(s.session(&cookie).unwrap().is_none());
        assert!(s.verify_password("bob@acme.com", "bobpassword").unwrap().is_some());
        // Removing Bob revokes his key.
        s.remove_member(su.account.id, "ann@acme.com", "bob@acme.com").unwrap();
        assert!(s.authenticate(&bob_secret).unwrap().is_none());
    }

    /// Sign-up is open to anyone, so an address that is already
    /// someone's must be proven with that person's password before it
    /// opens another account, and nothing is minted or changed before.
    #[test]
    fn signup_with_an_existing_email_needs_that_users_password() {
        let s = Store::in_memory().unwrap();
        let ann = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let sales = s.create_project(ann.account.id, &ann.user, "sales", "", "").unwrap();

        // Someone else, with Ann's address and their own password.
        assert!(matches!(s.signup("Evil", "ann@acme.test", "Mallory", Some("attackerpass1")), Err(Error::Denied(_))));
        assert!(matches!(s.signup("Evil", "ann@acme.test", "Mallory", None), Err(Error::Denied(_))));
        // Nothing was opened, Ann's password is hers, her key still acts in Acme.
        assert_eq!(s.members(ann.account.id).unwrap().len(), 1);
        assert!(s.verify_password("ann@acme.test", "hunter2hunter2").unwrap().is_some());
        assert!(s.verify_password("ann@acme.test", "attackerpass1").unwrap().is_none());
        assert_eq!(principal(&s, &ann.secret).account().uid, "acme");

        // Ann herself opens a second account with her password: the new
        // key acts there and only there, and her login lands there.
        let second = s.signup("Ann Side Project", "ann@acme.test", "", Some("hunter2hunter2")).unwrap();
        assert_eq!(second.user.id, ann.user.id);
        let p2 = principal(&s, &second.secret);
        assert_eq!(p2.account().uid, "ann-side-project");
        assert!(matches!(s.authorize(&p2, &sales, "main", Verb::Read), Err(Error::NotFound(_))));
        assert_eq!(principal(&s, &ann.secret).account().uid, "acme");
        let (_, landed) = s.create_session(&ann.user, 1).unwrap();
        assert_eq!(landed.uid, "ann-side-project");

        // A user with no password yet cannot be claimed by a sign-up either.
        let bob = s.add_member(ann.account.id, "ann@acme.test", "bob@acme.test", "Bob", Role::Developer).unwrap();
        assert!(matches!(s.signup("Bobs", "bob@acme.test", "", Some("bobpassword1")), Err(Error::Denied(_))));
        assert!(s.verify_password("bob@acme.test", "bobpassword1").unwrap().is_none());
        let _ = bob;
    }

    /// An invite token passes through the admin who asked for it, so it
    /// may only ever set up someone who has no way to sign in yet.
    #[test]
    fn an_invite_cannot_take_over_someone_who_can_already_sign_in() {
        let s = Store::in_memory().unwrap();
        let ann = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let mal = s.signup("Evil", "mal@evil.test", "Mallory", Some("attackerpass1")).unwrap();

        // Mallory adds Ann to Evil and redeems the invite herself.
        s.add_member(mal.account.id, "mal@evil.test", "ann@acme.test", "", Role::Developer).unwrap();
        let token = s.create_invite(mal.account.id, "mal@evil.test", "ann@acme.test").unwrap();
        assert!(matches!(s.accept_invite(&token, "Owned", "", Some("hijacked1234")), Err(Error::Denied(_))));
        assert!(s.verify_password("ann@acme.test", "hunter2hunter2").unwrap().is_some());
        assert!(s.verify_password("ann@acme.test", "hijacked1234").unwrap().is_none());
        assert_eq!(s.user_keys(mal.account.id, ann.user.id).unwrap().len(), 0);
        // Ann's login still lands in her own account.
        assert_eq!(s.create_session(&ann.user, 1).unwrap().1.uid, "acme");

        // A newcomer is set up by the invite, and a token has a shelf life.
        s.add_member(ann.account.id, "ann@acme.test", "bob@acme.test", "Bob", Role::Developer).unwrap();
        let token = s.create_invite(ann.account.id, "ann@acme.test", "bob@acme.test").unwrap();
        lock(&s.conn).execute("UPDATE invites SET created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now','-8 days')", []).unwrap();
        assert!(matches!(s.accept_invite(&token, "Bob", "", Some("bobpassword1")), Err(Error::NotFound(_))));
        let token = s.create_invite(ann.account.id, "ann@acme.test", "bob@acme.test").unwrap();
        let (acct, bob, key, secret) = s.accept_invite(&token, "Bob", "", Some("bobpassword1")).unwrap();
        assert_eq!((acct.uid.as_str(), key.account_id), ("acme", Some(ann.account.id)));
        assert_eq!(principal(&s, &secret).account().uid, "acme");
        // Having been set up once, Bob cannot be set up again by another token.
        let again = s.create_invite(ann.account.id, "ann@acme.test", "bob@acme.test").unwrap();
        assert!(matches!(s.accept_invite(&again, "", "", Some("replaced1234")), Err(Error::Denied(_))));
        assert!(s.verify_password("bob@acme.test", "bobpassword1").unwrap().is_some());
        let _ = bob;
    }

    /// A key is issued by one account and acts there alone; leaving that
    /// account ends that key and no other.
    #[test]
    fn a_personal_key_acts_only_in_the_account_that_issued_it() {
        let s = Store::in_memory().unwrap();
        let ann = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let ned = s.signup("Newco", "ned@newco.test", "Ned", Some("nedpassword1")).unwrap();
        s.add_member(ned.account.id, "ned@newco.test", "ann@acme.test", "", Role::Admin).unwrap();
        let (k, newco_secret) = s.create_user_key(ned.account.id, &ann.user, "for newco").unwrap();
        assert_eq!(k.account_id, Some(ned.account.id));
        assert_eq!(principal(&s, &newco_secret).account().uid, "newco");
        assert_eq!(principal(&s, &ann.secret).account().uid, "acme");
        // Listing is per account.
        assert_eq!(s.user_keys(ned.account.id, ann.user.id).unwrap().len(), 1);
        assert_eq!(s.user_keys(ann.account.id, ann.user.id).unwrap().len(), 1);
        // Removed from Newco: that key dies, the Acme one lives.
        s.remove_member(ned.account.id, "ned@newco.test", "ann@acme.test").unwrap();
        assert!(s.authenticate(&newco_secret).unwrap().is_none());
        assert_eq!(principal(&s, &ann.secret).account().uid, "acme");
    }

    #[test]
    fn changing_the_password_ends_other_sessions() {
        let s = Store::in_memory().unwrap();
        let ann = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let (mine, _) = s.create_session(&ann.user, 1).unwrap();
        let (theirs, _) = s.create_session(&ann.user, 1).unwrap();
        s.set_password(ann.account.id, &ann.user, "freshpassword1", Some("hunter2hunter2"), Some(&mine)).unwrap();
        assert!(s.session(&mine).unwrap().is_some(), "the session the change was made from stays");
        assert!(s.session(&theirs).unwrap().is_none(), "every other session is gone");
        assert!(s.verify_password("ann@acme.test", "freshpassword1").unwrap().is_some());
        // Without a session to keep, all of them go.
        let (c, _) = s.create_session(&ann.user, 1).unwrap();
        s.set_password(ann.account.id, &ann.user, "freshpassword2", Some("freshpassword1"), None).unwrap();
        assert!(s.session(&c).unwrap().is_none());
    }

    /// A database from before keys had accounts comes up with every key
    /// and every login resolving exactly as it did: to the first account.
    #[test]
    fn upgrading_a_v1_database_keeps_every_key_working() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 1; COMMIT;", MIGRATIONS[0])).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, uid, name) VALUES (1, 'acme', 'Acme'), (2, 'newco', 'Newco');
             INSERT INTO users (id, email) VALUES (1, 'ann@acme.test'), (2, 'nobody@x.test');
             INSERT INTO memberships (account_id, user_id, role) VALUES (2, 1, 'developer'), (1, 1, 'admin');
             INSERT INTO user_keys (id, user_id, name, prefix, hash) VALUES (1, 1, 'default', 'p', 'h1'), (2, 2, 'orphan', 'q', 'h2');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        // Ann's key is now Acme's (her first account by id), as it answered before.
        let acct: Option<i64> = conn.query_row("SELECT account_id FROM user_keys WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(acct, Some(1));
        let home: Option<i64> = conn.query_row("SELECT home_account_id FROM users WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(home, Some(1));
        // A key whose user belongs nowhere has no account, and answers for nothing.
        let orphan: Option<i64> = conn.query_row("SELECT account_id FROM user_keys WHERE id = 2", [], |r| r.get(0)).unwrap();
        assert_eq!(orphan, None);
        let s = Store::over(conn, DEFAULT_AUTH_CACHE_TTL);
        assert!(s.authenticate("zsk_whatever").unwrap().is_none());
    }

    #[test]
    fn query_keys_read_granted_branches_only() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme", "ann@acme.com", "Ann", None).unwrap();
        let sales = s.create_project(su.account.id, &su.user, "sales", "Sales", "main").unwrap();
        let hr = s.create_project(su.account.id, &su.user, "hr", "HR", "main").unwrap();
        let (qk, secret) = s.create_query_key(su.account.id, &su.user, "web app").unwrap();
        assert!(secret.starts_with("zqk_"));
        let p = principal(&s, &secret);
        assert!(matches!(s.authorize(&p, &sales, "main", Verb::Read), Err(Error::Denied(_))));
        s.grant(su.account.id, &su.user, qk.id, &sales, None, None).unwrap();
        let p = principal(&s, &secret);
        s.authorize(&p, &sales, "main", Verb::Read).unwrap();
        assert!(matches!(s.authorize(&p, &sales, "dev", Verb::Read), Err(Error::Denied(_))));
        assert!(matches!(s.authorize(&p, &hr, "main", Verb::Read), Err(Error::Denied(_))));
        assert!(matches!(s.authorize(&p, &sales, "main", Verb::Write), Err(Error::Denied(_))));
        s.grant(su.account.id, &su.user, qk.id, &sales, Some("*"), None).unwrap();
        let p = principal(&s, &secret);
        s.authorize(&p, &sales, "dev", Verb::Read).unwrap();
        // Rotation keeps the grants and kills the old secret.
        let (_, secret2) = s.rotate_query_key(su.account.id, &su.user, qk.id).unwrap();
        assert!(s.authenticate(&secret).unwrap().is_none());
        let p = principal(&s, &secret2);
        s.authorize(&p, &sales, "dev", Verb::Read).unwrap();
        assert_eq!(s.keys_granted_on(&sales).unwrap().len(), 1);
        s.revoke_query_key(su.account.id, &su.user, qk.id).unwrap();
        assert!(s.authenticate(&secret2).unwrap().is_none());
        assert!(s.audit_log(su.account.id, 50).unwrap().len() >= 6);
    }

    /// A bound context rides on the grant: it comes back on the cached
    /// principal, a re-grant without one clears it, and a revoke drops it.
    #[test]
    fn a_grant_carries_the_context_its_owner_bound() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme", "ann@acme.com", "Ann", None).unwrap();
        let sales = s.create_project(su.account.id, &su.user, "sales", "Sales", "main").unwrap();
        let (qk, secret) = s.create_query_key(su.account.id, &su.user, "web app").unwrap();
        let ctx = serde_json::json!({ "groups": [{ "name": "emea" }] });
        let g = s.grant(su.account.id, &su.user, qk.id, &sales, None, Some(&ctx)).unwrap();
        assert_eq!(g.context, Some(ctx.clone()));
        let p = principal(&s, &secret);
        let Principal::QueryKey { grants, .. } = &p else { panic!("a query key") };
        assert_eq!(grants[0].context, Some(ctx.clone()));
        assert_eq!(s.keys_granted_on(&sales).unwrap()[0].1.context, Some(ctx.clone()));
        // The change is felt at once: the cached principal is gone.
        s.grant(su.account.id, &su.user, qk.id, &sales, Some("*"), None).unwrap();
        let Principal::QueryKey { grants, .. } = principal(&s, &secret) else { panic!("a query key") };
        assert_eq!(grants[0].context, None, "a grant without a context is unbound again");
        assert_eq!(grants[0].branch.as_deref(), Some("*"));
        let log = s.audit_log(su.account.id, 10).unwrap();
        assert!(log.iter().any(|r| r["subject"].as_str().unwrap_or("").ends_with("with a bound context")), "{log:?}");
    }

    /// A reset link is a password in waiting, so each of its edges is
    /// worth a line: it works once, it drops the sessions someone else
    /// may be holding, and following it proves the address.
    #[test]
    fn a_reset_link_sets_a_password_once_and_proves_the_address() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        assert!(!su.user.email_verified, "a new address is unproven");
        assert!(su.verify_token.is_some(), "signing up issues a verification link");

        // A session from before the reset, which the reset must drop.
        let (session, _) = s.create_session(&su.user, 30).unwrap();
        assert!(s.session(&session).unwrap().is_some());

        let (_, user, token) = s.start_reset("ann@acme.test").unwrap().expect("a link for a known address");
        assert!(token.starts_with("zrs_"), "{token}");
        assert_eq!(user.email, "ann@acme.test");

        let (_, after) = s.reset_password(&token, "correcthorse").unwrap();
        assert!(after.email_verified, "following a link to the address proves it");
        assert!(s.verify_password("ann@acme.test", "correcthorse").unwrap().is_some(), "the new password works");
        assert!(s.verify_password("ann@acme.test", "hunter2hunter2").unwrap().is_none(), "the old one does not");
        assert!(s.session(&session).unwrap().is_none(), "every session of that user is dropped");

        // Replay, from a mailbox read later, does nothing.
        assert!(matches!(s.reset_password(&token, "another1234"), Err(Error::NotFound(_))), "a link works once");
        assert!(s.verify_password("ann@acme.test", "correcthorse").unwrap().is_some(), "and the password it set stands");

        // The keys the user's machines hold are untouched: a forgotten
        // password is not a reason to break their CLI.
        assert!(s.authenticate(&su.secret).unwrap().is_some(), "personal keys survive a reset");
    }

    /// Asking for a reset must not say whether an address is registered,
    /// and must not be a way to mailbomb someone who is.
    #[test]
    fn asking_for_a_reset_reveals_nothing_and_is_not_repeatable() {
        let s = Store::in_memory().unwrap();
        s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();

        // Unknown, and malformed, are the same answer as a success:
        // nothing the caller can tell apart.
        assert!(s.start_reset("nobody@acme.test").unwrap().is_none());
        assert!(s.start_reset("not-an-address").unwrap().is_none());

        // A first ask works; a second inside the cooldown does not, so
        // nobody can pump one mailbox from many addresses.
        let first = s.start_reset("ann@acme.test").unwrap().expect("the first link");
        assert!(s.start_reset("ann@acme.test").unwrap().is_none(), "a second ask inside the cooldown is refused");
        // The first link is still the live one, and still works.
        assert!(s.reset_password(&first.2, "correcthorse").is_ok());
    }

    /// Verification and reset are different doors, and a token for one
    /// must not open the other: a verification link is long-lived and
    /// sent on sign-up, so redeeming it as a reset would be a way in.
    #[test]
    fn a_link_only_opens_the_door_it_was_made_for() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let verify = su.verify_token.clone().expect("a verification link");
        assert!(verify.starts_with("zvf_"), "{verify}");

        assert!(matches!(s.reset_password(&verify, "correcthorse"), Err(Error::NotFound(_))), "a verification link is not a reset");
        assert!(s.verify_password("ann@acme.test", "hunter2hunter2").unwrap().is_some(), "the password is untouched");

        let (_, user) = s.verify_email(&verify).unwrap();
        assert!(user.email_verified);
        assert!(matches!(s.verify_email(&verify), Err(Error::NotFound(_))), "and it works once");

        // Nothing left to prove, so no second verification link.
        assert!(s.start_verification(su.account.id, &user).unwrap().is_none(), "an address already proven needs no link");
    }

    /// Issuing a link invalidates the one before it, so a mailbox never
    /// holds two live links and the newest is always the one that works.
    #[test]
    fn a_new_link_replaces_the_one_before_it() {
        let s = Store::in_memory().unwrap();
        let su = s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let first = su.verify_token.clone().expect("a verification link");

        // Past the cooldown, so the second ask is allowed. Reaching into
        // the row is the only way to age it without sleeping a minute.
        s.conn
            .lock()
            .unwrap()
            .execute("UPDATE email_tokens SET created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now','-5 minutes')", [])
            .unwrap();
        let second = s.start_verification(su.account.id, &su.user).unwrap().expect("a fresh link");
        assert_ne!(first, second);

        assert!(matches!(s.verify_email(&first), Err(Error::NotFound(_))), "the earlier link is dead");
        assert!(s.verify_email(&second).is_ok(), "the newest one works");
    }

    /// An expired link is refused rather than quietly honored.
    #[test]
    fn an_expired_link_is_refused() {
        let s = Store::in_memory().unwrap();
        s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        let (_, _, token) = s.start_reset("ann@acme.test").unwrap().expect("a link");
        s.conn
            .lock()
            .unwrap()
            .execute("UPDATE email_tokens SET expires_at = strftime('%Y-%m-%dT%H:%M:%SZ','now','-1 minute')", [])
            .unwrap();
        assert!(matches!(s.reset_password(&token, "correcthorse"), Err(Error::NotFound(_))));
    }

    /// An address that would break out of a mail header is not an
    /// address. Refused where addresses enter, not where they are sent.
    #[test]
    fn an_address_cannot_carry_a_mail_header() {
        let s = Store::in_memory().unwrap();
        for bad in ["ann@acme.test\nBcc: everyone@acme.test", "ann@acme.test\r\nBcc: v@x.test", "ann @acme.test"] {
            assert!(matches!(s.signup("Acme", bad, "Ann", None), Err(Error::Invalid(_))), "{bad:?} was accepted");
        }
    }

    /// A temporary directory of our own, removed by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zsql-accounts-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn version_of(path: &std::path::Path) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    /// The whole point of the version: a new file ends up on the last
    /// step, and opening it again applies nothing and changes nothing.
    #[test]
    fn a_new_database_lands_on_the_last_step_and_stays_there() {
        let dir = scratch("fresh");
        let path = dir.join("accounts.db");

        let s = Store::open(&path).unwrap();
        assert_eq!(version_of(&path), MIGRATIONS.len() as i64, "a new file is at the current version");
        s.signup("Acme", "ann@acme.test", "Ann", Some("hunter2hunter2")).unwrap();
        drop(s);

        let before = schema_of(&path);
        let s = Store::open(&path).unwrap();
        assert_eq!(version_of(&path), MIGRATIONS.len() as i64, "opening again does not move the version");
        assert_eq!(schema_of(&path), before, "opening again does not change the schema");
        assert!(s.start_reset("ann@acme.test").unwrap().is_some(), "and the account that was there still is");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every table and index in the file, as SQLite stores the
    /// statement that made it. A change to the schema has to change this
    /// too, which is the point: it makes the shape of the database
    /// something a reviewer sees rather than something they infer.
    fn schema_of(path: &std::path::Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt =
            conn.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// A file written before versioning existed cannot be told apart
    /// from one halfway through step 1, so it is refused with something
    /// to do about it rather than guessed at. Replacing the old
    /// `ALTER`-on-every-open means this is the one case that regressed,
    /// so it is the one worth a test.
    #[test]
    fn a_database_from_before_versioning_is_refused_not_guessed() {
        let dir = scratch("unversioned");
        let path = dir.join("accounts.db");
        {
            // Every table of version 1, as the live file had, and no
            // version stamp, as it also had.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
        }

        let Err(err) = Store::open(&path) else { panic!("tables with no version must not be taken for an empty file") };
        let said = err.to_string();
        assert!(said.contains("predates versioning"), "the error should say what is wrong: {said}");
        assert!(said.contains("user_version = 1"), "and what to do about it: {said}");

        // Stamping it, which is what step 4 of the plan does on the live
        // service, is enough to make it open; the later steps then run.
        Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 1").unwrap();
        assert!(Store::open(&path).is_ok(), "a stamped file opens");
        let v: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len(), "and is brought up to date");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file from a newer build is not downgraded by running an older
    /// one against it, because this build has no idea what the newer one
    /// did.
    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = scratch("newer");
        let path = dir.join("accounts.db");
        Store::open(&path).unwrap();
        Connection::open(&path).unwrap().execute_batch(&format!("PRAGMA user_version = {}", MIGRATIONS.len() + 1)).unwrap();

        let Err(err) = Store::open(&path) else { panic!("a newer file must not be opened blindly") };
        assert!(err.to_string().contains("newer than"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
