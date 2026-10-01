# Accounts, users, keys and access

Design notes, 2026-09-30. Not implemented yet.

## Shape

One cluster serves many accounts. An account has users (the developers who
build and maintain semantic layers), projects, and API keys. Rights flow
down: account, then project, then branch.

- **Account owners** (one or more) administer every project in the account
  and manage membership and keys. No other account-level role for now;
  billing stays with owners until it needs splitting.
- **Project grants** per user: `admin`, `write`, `read`.
  - `read`: see the model, plan queries, run discovery, create a branch of
    their own. No deploying to branches they do not own.
  - `write`: deploy or update the model on unprotected branches, create
    branches.
  - `admin`: everything, including deploying to protected branches, managing
    grants and keys, and deleting the project.
- **Branches** have an owner (the creator) and a `protected` flag, on by
  default for the production branch. A branch is deleted by its owner or a
  project admin; a protected branch by a project admin only. Deploying to a
  protected branch needs project admin or a deploy key granted that branch.

## Machines are the main callers

The plan endpoint is called by the customer's application at runtime, and
deploys mostly come from CI. API keys are therefore first-class principals,
not a convenience attached to a user:

- a key belongs to a project (or, for account-wide tooling, the account);
- it carries a scope: `plan` (plan and discovery, on one branch or all) or
  `deploy` (write); a key never carries delete rights;
- created and revoked by a project admin; optional expiry; last-used
  timestamp; a visible prefix so logs can name a key without revealing it.

The customer's end users never appear. They exist only as the security
context on plan requests, asserted by whoever holds the plan key: the key
authorizes the application, the application vouches for the context. A
leaked plan key can assert any context, including a bypass flag, so keys
should be scoped to one project and one branch where possible. Signed
contexts from the customer's identity provider are a later option.

## From day one

- Soft delete with a retention window for projects and branches.
- An audit log of deploys, key creation and revocation, grants and deletes,
  with the acting principal, user or key.

## Tables

`accounts`; `users` (identified by email, may belong to several accounts);
`memberships` (account, user, `owner | member`); `projects` (account);
`project_grants` (project, user, `admin | write | read`); `branches`
(project, owner, `protected`); `deployments` (branch, version, compiled
snapshot, created by, status); `api_keys` (project or account, scope, hashed
secret, prefix, expiry, last used); `audit_events`.

Not yet: teams or groups within an account. Per-user grants are enough
until an account has dozens of developers; groups can be layered on later
to bulk-assign the same grants.

## Service plan

In order, each shippable on its own:

1. A model handle that is safe to share: owns the snapshot, universe,
   formed paths and planner indexes, `Send + Sync`, built once per deployed
   branch and swapped atomically on redeploy.
2. Deploy: `POST /projects/{uid}/branches/{branch}/deploy` with the project
   archive; load, form, validate, store the compiled snapshot as a blob with
   a head pointer per branch; return the loader's warnings and errors.
3. Plan: `POST /projects/{uid}/branches/{branch}/plan` with the spec and the
   security context; SQL, adapter and timings back; structured errors with
   the existing class names; a context is required when the branch has
   policies.
4. Discovery: fields, tables, joins and policies per branch, hidden fields
   excluded for callers asking as a user.
5. Tenancy and tiers: API keys per project, and the entitlement that gates
   row-level security (policies rejected at deploy, or planned without, on a
   plan that lacks it).
6. Operations: hot reload on deploy, structured logs with plan timings, a
   health endpoint, bench numbers as a regression check.

Open choices: web framework (axum is the obvious one) and synchronous
deploys, which the current load time of a few milliseconds per project
allows.
