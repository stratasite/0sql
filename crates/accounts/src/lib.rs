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
use std::path::Path;
use std::sync::Mutex;

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
#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    pub project_id: i64,
    pub project_uid: String,
    pub branch: Option<String>,
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

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
  id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT NOT NULL DEFAULT '',
  password_hash TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
CREATE TABLE IF NOT EXISTS memberships (
  account_id INTEGER NOT NULL REFERENCES accounts(id), user_id INTEGER NOT NULL REFERENCES users(id),
  role TEXT NOT NULL CHECK (role IN ('admin','developer')),
  PRIMARY KEY (account_id, user_id));
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),
  uid TEXT NOT NULL, name TEXT NOT NULL DEFAULT '', production_branch TEXT NOT NULL DEFAULT 'main',
  visibility TEXT NOT NULL DEFAULT 'account' CHECK (visibility IN ('account','restricted')),
  protected_production INTEGER NOT NULL DEFAULT 0,
  created_by INTEGER REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  UNIQUE (account_id, uid));
CREATE TABLE IF NOT EXISTS project_access (
  project_id INTEGER NOT NULL REFERENCES projects(id), user_id INTEGER NOT NULL REFERENCES users(id),
  level TEXT NOT NULL CHECK (level IN ('read','write','owner')),
  PRIMARY KEY (project_id, user_id));
CREATE TABLE IF NOT EXISTS user_keys (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), name TEXT NOT NULL,
  prefix TEXT NOT NULL, hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  last_used_at TEXT, revoked_at TEXT);
CREATE TABLE IF NOT EXISTS query_keys (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id), name TEXT NOT NULL,
  prefix TEXT NOT NULL, hash TEXT NOT NULL UNIQUE, created_by INTEGER REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  last_used_at TEXT, revoked_at TEXT);
CREATE TABLE IF NOT EXISTS query_key_grants (
  query_key_id INTEGER NOT NULL REFERENCES query_keys(id), project_id INTEGER NOT NULL REFERENCES projects(id),
  branch TEXT,
  PRIMARY KEY (query_key_id, project_id));
CREATE TABLE IF NOT EXISTS invites (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id), user_id INTEGER NOT NULL REFERENCES users(id),
  hash TEXT NOT NULL UNIQUE, created_by TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')), accepted_at TEXT);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), account_id INTEGER NOT NULL REFERENCES accounts(id),
  hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  expires_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  subject TEXT NOT NULL DEFAULT '',
  at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
";

fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::PasswordHasher;
    if password.chars().count() < 8 {
        return Err(Error::Invalid("a password needs at least 8 characters".into()));
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2::Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| Error::Invalid(e.to_string()))?.to_string())
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

fn email_of(s: &str) -> Result<String> {
    let e = s.trim().to_lowercase();
    if !e.contains('@') || e.len() < 3 {
        return Err(Error::Invalid(format!("'{s}' is not an email address")));
    }
    Ok(e)
}

const USER_COLS: &str = "id, email, name";
const PROJECT_COLS: &str = "id, account_id, uid, name, production_branch, visibility, protected_production, created_by";

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User { id: r.get(0)?, email: r.get(1)?, name: r.get(2)? })
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
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn audit(conn: &Connection, account_id: i64, actor: &str, action: &str, subject: &str) -> rusqlite::Result<()> {
        conn.execute("INSERT INTO audit (account_id, actor, action, subject) VALUES (?1, ?2, ?3, ?4)", params![account_id, actor, action, subject])?;
        Ok(())
    }

    // ------------------------------------------------------------ sign-up

    /// A new account with its first admin and key. The account uid is the
    /// slug of its name, made unique with a suffix when taken.
    pub fn signup(&self, account_name: &str, email: &str, user_name: &str, password: Option<&str>) -> Result<Signup> {
        let email = email_of(email)?;
        let password_hash = password.map(hash_password).transpose()?;
        let name = account_name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("an account name is required".into()));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let base = slug(name);
        let base = if base.is_empty() { "account".to_string() } else { base };
        let mut uid = base.clone();
        let mut n = 1;
        while tx.query_row("SELECT 1 FROM accounts WHERE uid = ?1", params![uid], |_| Ok(())).optional()?.is_some() {
            n += 1;
            uid = format!("{base}-{n}");
        }
        tx.execute("INSERT INTO accounts (uid, name) VALUES (?1, ?2)", params![uid, name])?;
        let account = Account { id: tx.last_insert_rowid(), uid, name: name.to_string() };
        let user = match tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE email = ?1"), params![email], user_row).optional()? {
            Some(u) => {
                if let Some(h) = &password_hash {
                    tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2 AND password_hash IS NULL", params![h, u.id])?;
                }
                u
            }
            None => {
                tx.execute("INSERT INTO users (email, name, password_hash) VALUES (?1, ?2, ?3)", params![email, user_name.trim(), password_hash])?;
                User { id: tx.last_insert_rowid(), email: email.clone(), name: user_name.trim().to_string() }
            }
        };
        tx.execute("INSERT INTO memberships (account_id, user_id, role) VALUES (?1, ?2, 'admin')", params![account.id, user.id])?;
        let secret = new_secret("zsk");
        tx.execute(
            "INSERT INTO user_keys (user_id, name, prefix, hash) VALUES (?1, 'default', ?2, ?3)",
            params![user.id, prefix_of(&secret), hash(&secret)],
        )?;
        let key_id = tx.last_insert_rowid();
        Self::audit(&tx, account.id, &user.email, "signup", &account.uid)?;
        let key = Self::user_key_in(&tx, key_id)?;
        tx.commit()?;
        Ok(Signup { account, user, key, secret })
    }

    // ------------------------------------------------------ authentication

    /// The principal a secret stands for, or none. Touches last-used.
    pub fn authenticate(&self, secret: &str) -> Result<Option<Principal>> {
        let secret = secret.trim();
        let h = hash(secret);
        let conn = self.conn.lock().unwrap();
        if secret.starts_with("zsk_") {
            let row = conn
                .query_row(
                    "SELECT k.id, k.user_id, k.name, k.prefix, k.created_at, k.last_used_at FROM user_keys k WHERE k.hash = ?1 AND k.revoked_at IS NULL",
                    params![h],
                    |r| Ok(UserKey { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_at: r.get(4)?, last_used_at: r.get(5)? }),
                )
                .optional()?;
            let Some(key) = row else { return Ok(None) };
            conn.execute("UPDATE user_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![key.id])?;
            let user = conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![key.user_id], user_row)?;
            // A user key answers for the user's account; several accounts
            // per user is a later feature, so the first membership wins.
            let membership = conn
                .query_row(
                    "SELECT a.id, a.uid, a.name, m.role FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 ORDER BY a.id LIMIT 1",
                    params![user.id],
                    |r| Ok((Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }, role_of(&r.get::<_, String>(3)?))),
                )
                .optional()?;
            let Some((account, role)) = membership else { return Ok(None) };
            return Ok(Some(Principal::User { user, account, role, key_id: key.id }));
        }
        if secret.starts_with("zqk_") {
            let row = conn
                .query_row(
                    "SELECT id, account_id, name, prefix, created_by, created_at, last_used_at FROM query_keys WHERE hash = ?1 AND revoked_at IS NULL",
                    params![h],
                    |r| Ok(QueryKey { id: r.get(0)?, account_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_by: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? }),
                )
                .optional()?;
            let Some(key) = row else { return Ok(None) };
            conn.execute("UPDATE query_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![key.id])?;
            let account = conn.query_row("SELECT id, uid, name FROM accounts WHERE id = ?1", params![key.account_id], |r| Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }))?;
            let grants = Self::grants_in(&conn, key.id)?;
            return Ok(Some(Principal::QueryKey { key, account, grants }));
        }
        Ok(None)
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
                    Some(l) => Err(Error::Denied(format!("{} has {} access to {}; this needs {}", user.email, level_name(l), project.uid, level_name(needed)))),
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
        let conn = self.conn.lock().unwrap();
        let granted: Option<String> = conn
            .query_row("SELECT level FROM project_access WHERE project_id = ?1 AND user_id = ?2", params![project.id, user_id], |r| r.get(0))
            .optional()?;
        Ok(match granted {
            Some(l) => Some(level_of(&l)),
            None if project.visibility == "account" => Some(Level::Read),
            None => None,
        })
    }

    // ------------------------------------------------------------ members

    pub fn members(&self, account_id: i64) -> Result<Vec<Membership>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(&format!("SELECT u.id, u.email, u.name, m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = ?1 ORDER BY u.email"))?;
        let rows = st.query_map(params![account_id], |r| Ok(Membership { user: User { id: r.get(0)?, email: r.get(1)?, name: r.get(2)? }, role: role_of(&r.get::<_, String>(3)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a user to the account (creating the user by email when new)
    /// with a role, or changes the role of a member.
    pub fn add_member(&self, account_id: i64, actor: &str, email: &str, name: &str, role: Role) -> Result<Membership> {
        let email = email_of(email)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let user = match tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE email = ?1"), params![email], user_row).optional()? {
            Some(u) => u,
            None => {
                tx.execute("INSERT INTO users (email, name) VALUES (?1, ?2)", params![email, name.trim()])?;
                User { id: tx.last_insert_rowid(), email: email.clone(), name: name.trim().to_string() }
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
        Self::audit(&tx, account_id, actor, "member", &format!("{} {role_s}", user.email))?;
        tx.commit()?;
        Ok(Membership { user, role })
    }

    pub fn remove_member(&self, account_id: i64, actor: &str, email: &str) -> Result<()> {
        let email = email_of(email)?;
        let mut conn = self.conn.lock().unwrap();
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
        tx.execute("UPDATE user_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE user_id = ?1 AND revoked_at IS NULL", params![user.id])?;
        tx.execute("DELETE FROM project_access WHERE user_id = ?1 AND project_id IN (SELECT id FROM projects WHERE account_id = ?2)", params![user.id, account_id])?;
        Self::audit(&tx, account_id, actor, "member_removed", &user.email)?;
        tx.commit()?;
        Ok(())
    }

    /// A one-time token (`zin_...`) the admin hands to a new member, who
    /// redeems it with [`Store::accept_invite`] for a first personal key.
    pub fn create_invite(&self, account_id: i64, actor: &str, email: &str) -> Result<String> {
        let email = email_of(email)?;
        let conn = self.conn.lock().unwrap();
        let user = conn
            .query_row(
                &format!("SELECT {USER_COLS} FROM users u WHERE u.email = ?1 AND EXISTS (SELECT 1 FROM memberships m WHERE m.user_id = u.id AND m.account_id = ?2)"),
                params![email, account_id],
                user_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("{email} is not a member")))?;
        let token = new_secret("zin");
        conn.execute("INSERT INTO invites (account_id, user_id, hash, created_by) VALUES (?1, ?2, ?3, ?4)", params![account_id, user.id, hash(&token), actor])?;
        Self::audit(&conn, account_id, actor, "invite", &user.email)?;
        Ok(token)
    }

    /// Redeems an invite: the member's name is set when given, and a
    /// first personal key is issued. The token stops working.
    pub fn accept_invite(&self, token: &str, name: &str, key_name: &str, password: Option<&str>) -> Result<(Account, User, UserKey, String)> {
        let password_hash = password.map(hash_password).transpose()?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (invite_id, account_id, user_id): (i64, i64, i64) = tx
            .query_row("SELECT id, account_id, user_id FROM invites WHERE hash = ?1 AND accepted_at IS NULL", params![hash(token.trim())], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .ok_or_else(|| Error::NotFound("this invite is not valid or was already used".into()))?;
        tx.execute("UPDATE invites SET accepted_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![invite_id])?;
        if !name.trim().is_empty() {
            tx.execute("UPDATE users SET name = ?1 WHERE id = ?2", params![name.trim(), user_id])?;
        }
        if let Some(h) = &password_hash {
            tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![h, user_id])?;
        }
        let user = tx.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let account = tx.query_row("SELECT id, uid, name FROM accounts WHERE id = ?1", params![account_id], |r| Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }))?;
        let key_name = if key_name.trim().is_empty() { "default" } else { key_name.trim() };
        let secret = new_secret("zsk");
        tx.execute("INSERT INTO user_keys (user_id, name, prefix, hash) VALUES (?1, ?2, ?3, ?4)", params![user.id, key_name, prefix_of(&secret), hash(&secret)])?;
        let key = Self::user_key_in(&tx, tx.last_insert_rowid())?;
        Self::audit(&tx, account.id, &user.email, "invite_accepted", &account.uid)?;
        tx.commit()?;
        Ok((account, user, key, secret))
    }

    fn admin_count(conn: &Connection, account_id: i64) -> rusqlite::Result<i64> {
        conn.query_row("SELECT count(*) FROM memberships WHERE account_id = ?1 AND role = 'admin'", params![account_id], |r| r.get(0))
    }

    // ----------------------------------------------------------- user keys

    fn user_key_in(conn: &Connection, id: i64) -> Result<UserKey> {
        Ok(conn.query_row(
            "SELECT id, user_id, name, prefix, created_at, last_used_at FROM user_keys WHERE id = ?1",
            params![id],
            |r| Ok(UserKey { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_at: r.get(4)?, last_used_at: r.get(5)? }),
        )?)
    }

    pub fn user_key(&self, id: i64) -> Result<UserKey> {
        let conn = self.conn.lock().unwrap();
        Self::user_key_in(&conn, id)
    }

    pub fn user_keys(&self, user_id: i64) -> Result<Vec<UserKey>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT id, user_id, name, prefix, created_at, last_used_at FROM user_keys WHERE user_id = ?1 AND revoked_at IS NULL ORDER BY id")?;
        let rows = st.query_map(params![user_id], |r| Ok(UserKey { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_at: r.get(4)?, last_used_at: r.get(5)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A new personal key; the secret is returned once.
    pub fn create_user_key(&self, account_id: i64, user: &User, name: &str) -> Result<(UserKey, String)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("a key name is required".into()));
        }
        let secret = new_secret("zsk");
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO user_keys (user_id, name, prefix, hash) VALUES (?1, ?2, ?3, ?4)", params![user.id, name, prefix_of(&secret), hash(&secret)])?;
        let id = conn.last_insert_rowid();
        Self::audit(&conn, account_id, &user.email, "user_key_created", &format!("{name} {}", prefix_of(&secret)))?;
        Ok((Self::user_key_in(&conn, id)?, secret))
    }

    pub fn revoke_user_key(&self, account_id: i64, user: &User, key_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE user_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL",
            params![key_id, user.id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("no active key {key_id} for {}", user.email)));
        }
        Self::audit(&conn, account_id, &user.email, "user_key_revoked", &key_id.to_string())?;
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
        let conn = self.conn.lock().unwrap();
        Self::query_key_in(&conn, account_id, id)
    }

    pub fn query_keys(&self, account_id: i64) -> Result<Vec<QueryKey>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT id, account_id, name, prefix, created_by, created_at, last_used_at FROM query_keys WHERE account_id = ?1 AND revoked_at IS NULL ORDER BY id")?;
        let rows = st.query_map(params![account_id], |r| Ok(QueryKey { id: r.get(0)?, account_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_by: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A new query key with no grants; the secret is returned once.
    pub fn create_query_key(&self, account_id: i64, actor: &User, name: &str) -> Result<(QueryKey, String)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("a key name is required".into()));
        }
        let secret = new_secret("zqk");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO query_keys (account_id, name, prefix, hash, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, name, prefix_of(&secret), hash(&secret), actor.id],
        )?;
        let id = conn.last_insert_rowid();
        Self::audit(&conn, account_id, &actor.email, "query_key_created", &format!("{name} {}", prefix_of(&secret)))?;
        Ok((Self::query_key_in(&conn, account_id, id)?, secret))
    }

    /// A new secret for the same key and grants; the old secret stops working.
    pub fn rotate_query_key(&self, account_id: i64, actor: &User, id: i64) -> Result<(QueryKey, String)> {
        let secret = new_secret("zqk");
        let conn = self.conn.lock().unwrap();
        let key = Self::query_key_in(&conn, account_id, id)?;
        conn.execute("UPDATE query_keys SET prefix = ?1, hash = ?2 WHERE id = ?3", params![prefix_of(&secret), hash(&secret), key.id])?;
        Self::audit(&conn, account_id, &actor.email, "query_key_rotated", &format!("{} {}", key.name, prefix_of(&secret)))?;
        Ok((Self::query_key_in(&conn, account_id, id)?, secret))
    }

    pub fn revoke_query_key(&self, account_id: i64, actor: &User, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let key = Self::query_key_in(&conn, account_id, id)?;
        conn.execute("UPDATE query_keys SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?1", params![key.id])?;
        Self::audit(&conn, account_id, &actor.email, "query_key_revoked", &key.name)?;
        Ok(())
    }

    fn grants_in(conn: &Connection, key_id: i64) -> rusqlite::Result<Vec<Grant>> {
        let mut st = conn.prepare("SELECT g.project_id, p.uid, g.branch FROM query_key_grants g JOIN projects p ON p.id = g.project_id WHERE g.query_key_id = ?1 ORDER BY p.uid")?;
        let rows = st.query_map(params![key_id], |r| Ok(Grant { project_id: r.get(0)?, project_uid: r.get(1)?, branch: r.get(2)? }))?;
        rows.collect()
    }

    pub fn grants(&self, account_id: i64, key_id: i64) -> Result<Vec<Grant>> {
        let conn = self.conn.lock().unwrap();
        Self::query_key_in(&conn, account_id, key_id)?;
        Ok(Self::grants_in(&conn, key_id)?)
    }

    /// Grants the key read on a project: its production branch when
    /// `branch` is `None`, every branch for `*`, else the named branch.
    pub fn grant(&self, account_id: i64, actor: &User, key_id: i64, project: &Project, branch: Option<&str>) -> Result<Grant> {
        let conn = self.conn.lock().unwrap();
        let key = Self::query_key_in(&conn, account_id, key_id)?;
        let branch = branch.map(str::trim).filter(|b| !b.is_empty()).map(str::to_string);
        conn.execute(
            "INSERT INTO query_key_grants (query_key_id, project_id, branch) VALUES (?1, ?2, ?3) ON CONFLICT (query_key_id, project_id) DO UPDATE SET branch = excluded.branch",
            params![key.id, project.id, branch],
        )?;
        Self::audit(&conn, account_id, &actor.email, "grant", &format!("{} on {}/{}", key.name, project.uid, branch.as_deref().unwrap_or(&project.production_branch)))?;
        Ok(Grant { project_id: project.id, project_uid: project.uid.clone(), branch })
    }

    pub fn revoke_grant(&self, account_id: i64, actor: &User, key_id: i64, project: &Project) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let key = Self::query_key_in(&conn, account_id, key_id)?;
        let n = conn.execute("DELETE FROM query_key_grants WHERE query_key_id = ?1 AND project_id = ?2", params![key.id, project.id])?;
        if n == 0 {
            return Err(Error::NotFound(format!("{} is not granted {}", key.name, project.uid)));
        }
        Self::audit(&conn, account_id, &actor.email, "grant_revoked", &format!("{} on {}", key.name, project.uid))?;
        Ok(())
    }

    /// Every key granted on a project, for its owners to see.
    pub fn keys_granted_on(&self, project: &Project) -> Result<Vec<(QueryKey, Grant)>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT k.id, k.account_id, k.name, k.prefix, k.created_by, k.created_at, k.last_used_at, g.branch FROM query_key_grants g JOIN query_keys k ON k.id = g.query_key_id WHERE g.project_id = ?1 AND k.revoked_at IS NULL ORDER BY k.name",
        )?;
        let rows = st.query_map(params![project.id], |r| {
            Ok((
                QueryKey { id: r.get(0)?, account_id: r.get(1)?, name: r.get(2)?, prefix: r.get(3)?, created_by: r.get(4)?, created_at: r.get(5)?, last_used_at: r.get(6)? },
                Grant { project_id: project.id, project_uid: project.uid.clone(), branch: r.get(7)? },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------ projects

    pub fn project(&self, account_id: i64, uid: &str) -> Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE account_id = ?1 AND uid = ?2"), params![account_id, uid.trim().to_lowercase()], project_row)
            .optional()?)
    }

    pub fn projects(&self, account_id: i64) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
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
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO projects (account_id, uid, name, production_branch, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, uid, name.trim(), production_branch, creator.id],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO project_access (project_id, user_id, level) VALUES (?1, ?2, 'owner')", params![id, creator.id])?;
        Self::audit(&tx, account_id, &creator.email, "project_created", &uid)?;
        let p = tx.query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"), params![id], project_row)?;
        tx.commit()?;
        Ok(p)
    }

    pub fn update_project(&self, actor: &User, project: &Project, name: Option<&str>, production_branch: Option<&str>, visibility: Option<&str>, protected_production: Option<bool>) -> Result<Project> {
        if let Some(v) = visibility {
            if !matches!(v, "account" | "restricted") {
                return Err(Error::Invalid(format!("'{v}' is not a visibility (account, restricted)")));
            }
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE projects SET name = coalesce(?1, name), production_branch = coalesce(?2, production_branch), visibility = coalesce(?3, visibility), protected_production = coalesce(?4, protected_production) WHERE id = ?5",
            params![name.map(str::trim), production_branch.map(str::trim).filter(|b| !b.is_empty()), visibility, protected_production.map(|b| b as i64), project.id],
        )?;
        Self::audit(&conn, project.account_id, &actor.email, "project_updated", &project.uid)?;
        Ok(conn.query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"), params![project.id], project_row)?)
    }

    pub fn delete_project(&self, actor: &User, project: &Project) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM query_key_grants WHERE project_id = ?1", params![project.id])?;
        tx.execute("DELETE FROM project_access WHERE project_id = ?1", params![project.id])?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![project.id])?;
        Self::audit(&tx, project.account_id, &actor.email, "project_deleted", &project.uid)?;
        tx.commit()?;
        Ok(())
    }

    pub fn project_access(&self, project: &Project) -> Result<Vec<(User, Level)>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT u.id, u.email, u.name, a.level FROM project_access a JOIN users u ON u.id = a.user_id WHERE a.project_id = ?1 ORDER BY u.email")?;
        let rows = st.query_map(params![project.id], |r| Ok((User { id: r.get(0)?, email: r.get(1)?, name: r.get(2)? }, level_of(&r.get::<_, String>(3)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sets a member's level on a project. The user must be a member of
    /// the account; a project keeps at least one owner.
    pub fn set_access(&self, actor: &User, project: &Project, email: &str, level: Level) -> Result<(User, Level)> {
        let email = email_of(email)?;
        let mut conn = self.conn.lock().unwrap();
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
        Self::audit(&tx, project.account_id, &actor.email, "access", &format!("{} {} on {}", user.email, level_name(level), project.uid))?;
        tx.commit()?;
        Ok((user, level))
    }

    pub fn remove_access(&self, actor: &User, project: &Project, email: &str) -> Result<()> {
        let email = email_of(email)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM project_access WHERE project_id = ?1 AND user_id = (SELECT id FROM users WHERE email = ?2)", params![project.id, email])?;
        if n == 0 {
            return Err(Error::NotFound(format!("{email} has no granted access to {}", project.uid)));
        }
        if Self::owner_count(&tx, project.id)? == 0 {
            return Err(Error::Invalid(format!("{} needs at least one owner", project.uid)));
        }
        Self::audit(&tx, project.account_id, &actor.email, "access_removed", &format!("{email} on {}", project.uid))?;
        tx.commit()?;
        Ok(())
    }

    fn owner_count(conn: &Connection, project_id: i64) -> rusqlite::Result<i64> {
        conn.query_row("SELECT count(*) FROM project_access WHERE project_id = ?1 AND level = 'owner'", params![project_id], |r| r.get(0))
    }

    // ----------------------------------------------------- passwords, sessions

    pub fn set_password(&self, account_id: i64, user: &User, password: &str) -> Result<()> {
        let h = hash_password(password)?;
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![h, user.id])?;
        Self::audit(&conn, account_id, &user.email, "password_set", "")?;
        Ok(())
    }

    /// The user when the email and password match; `None` otherwise,
    /// including for users who never set a password.
    pub fn verify_password(&self, email: &str, password: &str) -> Result<Option<User>> {
        let Ok(email) = email_of(email) else { return Ok(None) };
        let conn = self.conn.lock().unwrap();
        let row: Option<(User, Option<String>)> = conn
            .query_row(&format!("SELECT {USER_COLS}, password_hash FROM users WHERE email = ?1"), params![email], |r| Ok((user_row(r)?, r.get(3)?)))
            .optional()?;
        let Some((user, Some(h))) = row else { return Ok(None) };
        use argon2::{PasswordHash, PasswordVerifier};
        let parsed = PasswordHash::new(&h).map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(argon2::Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok().then_some(user))
    }

    /// A browser session for the user on an account; the token is the
    /// cookie value, stored hashed, good for `days`.
    pub fn create_session(&self, user: &User, days: i64) -> Result<(String, Account)> {
        let conn = self.conn.lock().unwrap();
        let account = conn
            .query_row(
                "SELECT a.id, a.uid, a.name FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 ORDER BY a.id LIMIT 1",
                params![user.id],
                |r| Ok(Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }),
            )
            .optional()?
            .ok_or_else(|| Error::Denied(format!("{} belongs to no account", user.email)))?;
        let token = new_secret("zss");
        conn.execute(
            "INSERT INTO sessions (user_id, account_id, hash, expires_at) VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?4))",
            params![user.id, account.id, hash(&token), format!("+{days} days")],
        )?;
        Self::audit(&conn, account.id, &user.email, "login", "")?;
        Ok((token, account))
    }

    /// The principal a session cookie stands for, if it is live.
    pub fn session(&self, token: &str) -> Result<Option<Principal>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT user_id, account_id FROM sessions WHERE hash = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')",
                params![hash(token.trim())],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((user_id, account_id)) = row else { return Ok(None) };
        let user = conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), params![user_id], user_row)?;
        let membership = conn
            .query_row(
                "SELECT a.id, a.uid, a.name, m.role FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = ?1 AND m.account_id = ?2",
                params![user_id, account_id],
                |r| Ok((Account { id: r.get(0)?, uid: r.get(1)?, name: r.get(2)? }, role_of(&r.get::<_, String>(3)?))),
            )
            .optional()?;
        let Some((account, role)) = membership else { return Ok(None) };
        Ok(Some(Principal::User { user, account, role, key_id: 0 }))
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE hash = ?1", params![hash(token.trim())])?;
        Ok(())
    }

    // --------------------------------------------------------------- audit

    pub fn record(&self, account_id: i64, actor: &str, action: &str, subject: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        Ok(Self::audit(&conn, account_id, actor, action, subject)?)
    }

    pub fn audit_log(&self, account_id: i64, limit: usize) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
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
        s.grant(su.account.id, &su.user, qk.id, &sales, None).unwrap();
        let p = principal(&s, &secret);
        s.authorize(&p, &sales, "main", Verb::Read).unwrap();
        assert!(matches!(s.authorize(&p, &sales, "dev", Verb::Read), Err(Error::Denied(_))));
        assert!(matches!(s.authorize(&p, &hr, "main", Verb::Read), Err(Error::Denied(_))));
        assert!(matches!(s.authorize(&p, &sales, "main", Verb::Write), Err(Error::Denied(_))));
        s.grant(su.account.id, &su.user, qk.id, &sales, Some("*")).unwrap();
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
}
