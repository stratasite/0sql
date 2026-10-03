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
- An account may keep **5 deployed branches per project and 20 in all**
  (`ZSQL_MAX_BRANCHES_PER_PROJECT`, `ZSQL_MAX_BRANCHES_PER_ACCOUNT`;
  `off` for no limit). Going over answers `409 OverLimit` and names what
  to remove. Redeploying a branch that is already deployed is not a new
  branch, so an account at its cap can still ship to what it has, and
  `validate` is not held to the caps since it keeps nothing. Counted
  from the models in memory, not the project records, because memory is
  what they protect: see `running-it.md`.

| level  | can                                                                                  |
|--------|--------------------------------------------------------------------------------------|
| read   | sql, explain, explore, fields, tables, values, status                                 |
| write  | read plus deploy, validate, test, remove a branch, upload samples                     |
| owner  | write plus grant and revoke access, manage query key grants, settings, delete project |
| admin  | owner on every project, plus members, roles, every key, the audit log                 |

Secrets are shown once and stored as SHA-256; a twelve-character prefix
names a key in logs and listings. Every change writes an audit row with
its actor: sign-up, members, keys, grants, access, deploys, removals,
and the verification and reset links as they are asked for and used.

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
POST   /password-reset                         {email} → {ok} always              (no auth)
POST   /password-reset/accept                  {token, password} → user, account  (no auth; opens a session)
POST   /verify-email                           {token} → {verified, user, account} (no auth)
POST   /account/verify-email                   a fresh verification link to the caller's own address
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

## The web app

Served by zsqld itself at `/app/` (the root redirects there when accounts
are on): sign-up, login, invite acceptance, projects with their deployed
branches, settings and people, query keys with grants and rotation,
personal keys and password, members with invites, the audit log for
admins, and a playground per deployed branch: a shorthand line or a spec,
an optional security context, and the SQL, the explain tree or the
explore lists back, with a field search beside it and recent lines kept
in the browser. One HTML shell, one stylesheet and one script, embedded in the
binary; views render from the JSON API. The styling follows the 0sql brand
package (the marketing repo's `brand/`): monochrome with the one rust
accent on the logo badge, links, the primary action, and active and focus
states; Inter for the UI, mono for titles, keys and code; the outlined
wordmark and site icons from the package live under `crates/server/ui/brand`
and are served at `/app/brand/...` and the site root. Interface icons are
Lucide, inlined. Light and dark follow the system, with a toggle.

Authentication is cached in memory for a minute (`ZSQL_AUTH_CACHE_TTL`)
so a key in steady use does not take the store's lock on every request.
Every change empties that cache, through the audit row each one writes,
so a revoked or rotated key, a changed role, level or grant, and a logout
are all felt by the next request. See `docs/rate-limiting.md`.

Sessions are a cookie (`zsql_session`, HttpOnly, SameSite=Lax, Secure
when `ZSQL_PUBLIC_URL` is https) backed by a hashed row with a 30-day
expiry. Any state-changing call under a cookie must also carry the
`x-requested-with: zsql` header the app's scripts send, which a
cross-site form cannot. Passwords are argon2 hashes; sign-up and invite
acceptance set them. `POST /login`, `POST /logout`,
`POST /account/password`.

## Verification and password reset

Two emailed links, both `zvf_`/`zrs_` secrets stored as SHA-256 like
every other secret here, both one-time, and both bound to the address
they were sent to: changing the address kills the links to the old one.
Issuing a link invalidates any unused one of the same kind, so a mailbox
never holds two live links and the newest is always the one that works.
A link of one kind cannot be redeemed at the other's endpoint.

**Verification** is issued at sign-up and expires in 24 hours. Nothing
is denied for want of it: blocking an unverified account would lock out
everyone who signed up before this existed, and the point is to know
whether the address can be trusted for support and billing, not to gate
the product. The web app carries a line at the top of every page until
it is done, with a button that sends a fresh link. Invite acceptance
does not verify: the invite token is relayed by an admin, so following
it proves nothing about the address.

**Reset** expires in an hour. `POST /password-reset` answers `{ok: true}`
whether or not the address has an account, whether or not it belongs to
one, and whether or not a link went out moments ago — otherwise it would
be a way to ask the server which addresses are registered. Redeeming it
sets the password, drops **every session** of that user, marks the
address verified (following a link to it is the proof verification asks
for), and opens a new session so the user lands signed in. Personal keys
are left alone: a forgotten password is not a reason to break the user's
machines.

One link per user per minute, whichever address asked. The per-address
rate limit stops one person asking for many; this stops many addresses
mailbombing one victim. See `docs/rate-limiting.md` for the `reset` gate.

Delivery is a command, not an SMTP client: the message goes to
`ZSQL_MAIL_COMMAND` on stdin, the way `sendmail` has always been fed.
With none configured the links are written to the journal and never put
in a response — a reset is asked for by whoever typed the address, who
may not own it. `docs/running-it.md` has the live configuration.

## Deferred

See `docs/running-it.md` for the state of the running service and the
operational side of this list.

- Magic links, and SSO. Password reset and verification are done; see
  above.
- What startup should do when it cannot load every deployment. The caps
  bound one account, not the machine across many, and a process that
  runs out of memory still restarts into the same reload. `running-it.md`
  keeps this open.
- A user in several accounts: the schema allows it, the key answers for
  the first membership for now.
- Key expiry, a viewer role, teams, binding a query key to the security
  context claims it may assert.
- Multiple server instances: a storage trait with a Postgres and S3
  backend behind the same endpoints (see the hosted-service notes in the
  conversation of 2026-10-01: lazy load, LRU, version stamps).
