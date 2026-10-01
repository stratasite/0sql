# Accounts, users, keys and access

Decided 2026-10-01; implemented in `crates/accounts` and the server's
accounts endpoints. The earlier draft (project-scoped keys, branch owners)
is superseded.

## The model

- An **account** is the tenant. It has users through memberships with a
  role, `admin` or `developer`, and it owns projects. The first user of an
  account is its admin, and an account always keeps at least one admin.
- A **user** acts through **personal keys** (`zsk_...`), which do exactly
  what the user may do; revoking the user revokes the keys. A user may
  hold several, named, one per machine.
- Every developer **reads every project** in the account, unless the
  project is `restricted`, which shows it to granted users only. The
  creator of a project **owns** it. Owners grant `read`, `write` or
  `owner` to other members; a project always keeps at least one owner.
  Admins own every project.
- A **query key** (`zqk_...`) is a principal of its own, for applications.
  It is read only, always. It carries **grants**, each naming a project and
  a branch: the project's production branch by default, `*` for every
  branch, or one named branch. Any developer can create a query key;
  grants on a project are given by that project's owners (or an admin).
  Rotation issues a new secret and keeps the grants; the old secret stops
  at once.
- A project's **protected production** flag makes deploys to its
  production branch need an owner.

| level  | can                                                                                  |
|--------|--------------------------------------------------------------------------------------|
| read   | sql, explain, explore, fields, tables, values, status                                 |
| write  | read plus deploy, validate, test, remove a branch, upload samples                     |
| owner  | write plus grant and revoke access, manage query key grants, settings, delete project |
| admin  | owner on every project, plus members, roles, every key, the audit log                 |

Secrets are shown once and stored as SHA-256; a twelve-character prefix
names a key in logs and listings. Every change writes an audit row with
its actor: sign-up, members, keys, grants, access, deploys, removals.

## Modes

`ZSQL_ACCOUNTS=on` turns tenancy on: `accounts.db` (SQLite) in the data
directory, deployments under `data/<account>/<project>/<branch>`, every
request authenticated. `ZSQL_SIGNUP=closed` stops new accounts. Off (the
default), the server is single-tenant with no auth: the self-hosted mode,
and the mode the CLI's tests run against.

The CLI is unchanged: it sends whatever key `zsql auth` stored, and the
server answers 401 without one, 403 when the key may not do the action.

## Endpoints

```
POST   /signup                                 {account, email, name?} → account, user, key, secret (once)
GET    /me
GET    /account/members                        POST {email, name?, role}  (admin; a member without a key also gets an invite token, once)
DELETE /account/members/{email}                                           (admin)
POST   /account/members/{email}/invite         a fresh invite token        (admin)
POST   /invites/accept                         {token, name?, key_name?} → account, user, key, secret (no auth; the token is the credential)
GET    /account/keys                           POST {name} → key, secret   (the caller's own)
DELETE /account/keys/{id}
GET    /account/query-keys                     POST {name} → key, secret
DELETE /account/query-keys/{id}                                           (admin or creator)
POST   /account/query-keys/{id}/rotate         → key, secret               (admin or creator)
POST   /account/query-keys/{id}/grants         {project, branch?}          (project owner)
DELETE /account/query-keys/{id}/grants/{project}                          (project owner)
GET    /account/audit?limit=                                              (admin)
GET    /projects                               deployments the caller may read, and the account's projects with the caller's level
PATCH  /projects/{uid}                         {name?, production_branch?, visibility?, protected_production?} (owner)
DELETE /projects/{uid}                         the record and every deployment (owner)
GET    /projects/{uid}/access                  users and query keys on the project
PUT    /projects/{uid}/access                  {email, level}              (owner)
DELETE /projects/{uid}/access/{email}                                     (owner)
```

A user deploying to a project uid the account does not have yet creates
the project and owns it; its name and production branch come from the
archive's project.yml.

## Deferred

- Sign-up, login and the key pages in a web app (`app.0sql.io`);
  passwords or magic links. The `users.password_hash` column waits for it.
- A user in several accounts: the schema allows it, the key answers for
  the first membership for now.
- Key expiry, a viewer role, teams, binding a query key to the security
  context claims it may assert.
- Multiple server instances: a storage trait with a Postgres and S3
  backend behind the same endpoints (see the hosted-service notes in the
  conversation of 2026-10-01: lazy load, LRU, version stamps).
