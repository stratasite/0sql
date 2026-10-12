# The MCP endpoint

Decided and implemented 2026-10-09. `POST /mcp` on zsqld gives an agent
the server's reads as tools, under the same keys, the same gate and the
same limits as everything else. `crates/server/src/mcp.rs` is the whole
of it.

## What it is

- **One route on the existing server**, reached at
  `https://app.0sql.io/mcp` through the same tunnel as the API. Always
  on; nothing to configure.
- **Streamable HTTP, stateless, JSON only.** Each HTTP request carries
  one JSON-RPC message and the key. No `Mcp-Session-Id`, no server-sent
  events, no GET stream: `GET` and `DELETE` answer 405. The
  specification allows all three, and together they mean there is no
  session that could outlive a key or be bound to the wrong one. Every
  request is authenticated from scratch, through the cache every
  request uses.
- **Read-only tools, one per read the API has:** `list_projects`,
  `fields`, `tables`, `values`, `explore`, `sql`, `explain`. Each runs
  the function the matching REST handler runs, with the principal the
  guard worked out. No deploy, no management, and the server never
  executes SQL, so a leaked key exposes model metadata and SQL text,
  not rows.
- **Specs only, never the shorthand.** `sql`, `explain` and `explore`
  take a query spec, and the spec's JSON schema travels in the tool
  definition, so a client knows the shape before it writes one. A spec
  is what a model writes reliably; the shorthand is for people at a
  prompt.
- **The instruction set is two files beside the code.**
  `crates/server/src/mcp_prompts.json` holds what `initialize` tells
  the client and each tool's description;
  `crates/server/src/mcp_spec_schema.json` is the spec schema. Both are
  compiled in. They are tuned against `harness/mcp-eval`, which runs
  Claude Code as a real MCP client over the demo model and grades the
  spec it settles on by the rows it returns; see its README.
- **Written by hand**, not taken from a library. The subset needed is
  `initialize`, `notifications/initialized`, `ping`, `tools/list`,
  `tools/call` and version negotiation, which is a few hundred lines,
  and the point of writing them here is that every byte between the
  Authorization header and `gate()` is in this crate and covered by the
  isolation suite. The tool layer is plain functions over the handlers,
  so a library transport could replace the shell later without touching
  what the tools may reach.

Protocol revisions `2025-11-25`, `2025-06-18` and `2025-03-26` are
answered; a client asking for another gets the newest. An
`MCP-Protocol-Version` header naming anything else is a 400.

## Authentication: the same rules, and two the transport adds

- **The guard authenticates `/mcp` as it does every route.** The path
  is not on the exempt list, so the bearer key is checked once, bad
  keys count against the per-address gate, and the principal travels
  on the request. The handler reads only that.
- **A key and nothing else.** A session cookie is never read on `/mcp`,
  so a signed-in browser cannot be made to speak MCP as its user, on
  any page. A key in the query string is not read anywhere.
- **Origin.** A request that names an `Origin` must name
  `ZSQL_PUBLIC_URL`'s, or it is refused with 403 before any key is
  looked at. Programs send no Origin and pass; a page from another site
  does not. This is the transport's own rule against DNS rebinding.
- **401 carries `WWW-Authenticate: Bearer realm="0sql"`** and no
  `resource_metadata`. There is no OAuth server to discover, and
  naming one would send clients looking for it. Claude's clients fall
  through to the fixed credential the user configured, which is the
  key.
- **Every tool goes through `gate()` with `Verb::Read`.** A query key's
  grants, a restricted project's list, admin ownership: the same code
  path decides, so there is nothing to keep in step.

## The security context: the one place the same rules were not enough

A row-level policy reads the context a query is planned for. In it,
`system_admin` or `project_admin` set to true skips any policy that
allows the bypass; otherwise the policy resolves what the caller may
see from the email, the user's tags, or group names and tags, and a
policy whose `unresolved` is `allow` is skipped when nothing resolves.
So the context is a full bypass in one field, and an empty one is not a
safe default either.

Over the API the holder of the key sends the context. That is tolerable
for an application acting for its users. It is unacceptable when a
model writes the arguments, so over MCP the caller never says who it
is:

- **A personal key plans as its user's email**, with no flag, whatever
  the user's role in the account. An account admin is a tenancy role,
  not a policy role.
- **A query key plans as the context bound to its grant** on that
  project, by the project's owner. A grant with no bound context on a
  branch that has policies gets the `ContextRequired` refusal a missing
  context always got.
- **A `context` among a tool's arguments is refused** with its own
  message, never dropped silently. Every tool's schema refuses unknown
  properties.

### Binding a context to a grant

The context lives on the grant, not the key. A context is a claim in
one model's vocabulary, so the person entitled to make it is that
project's owner, who already is the one who grants the key access. A
per-key context would let the owner of one project assert
`system_admin` on another owner's project.

- `POST /account/query-keys/{id}/grants {project, branch?, context?}`:
  the same call, the same owner check, with the context as the
  optional third field. It is parsed as a security context with unknown
  fields refused, stored in its canonical shape, and shown back on the
  grant in `/account/query-keys` and `/projects/{uid}/access`. Granting
  again without a context clears it, so a grant is always exactly what
  its last call said. Rotation keeps it; revoking the grant drops it.
  Every change is an audit row, which also empties the auth cache, so
  the next request plans with the new claims.
- **Over the API a bound context wins too.** A request on a bound
  grant that sends no context plans with the bound one; one that sends
  any context, the same one included, is refused with
  `400 ContextBound`. Otherwise an owner could pin a key to one group
  and the application could still assert `system_admin` on the old
  path. Explore is held to the same rule, so a key behaves the same on
  every read. Grants with no bound context keep the old behaviour, so
  existing applications that act for many users are untouched.

For a query key on a branch with policies:

| grant   | request sends a context | API                          | MCP                            |
|---------|-------------------------|------------------------------|--------------------------------|
| unbound | no                      | refused, `ContextRequired`   | refused, `ContextRequired`     |
| unbound | yes                     | planned with the sent one    | refused, context not accepted  |
| bound   | no                      | planned with the bound one   | planned with the bound one     |
| bound   | yes                     | refused, `ContextBound`      | refused, context not accepted  |

On a branch without policies the context does not change the plan, but
the two refusals in the bottom row still apply so the rule stays one
rule.

Personal keys over the API are unchanged: a developer in the playground
needs to try contexts.

## Limits

One HTTP request is one tool call. The guard classifies `POST /mcp` as
`plan` and charges that allowance and seat; `fields` and `explore`,
which cost more, then take an `explore` token and seat as well inside
the dispatcher, and answer 429 or 503 the way the API does so a client's
existing back-off applies. The 30 second deadline is the plan deadline.
The one-megabyte JSON body limit applies.

An `initialize` writes one journal line naming the key and the client
it declared. It does not write an audit row: an audit row empties the
auth cache, and a client that initializes often, or a hostile key
holder that does so on purpose, would keep the cache cold for everyone.
`last_used_at` on the key is touched as on any request.

## What the client is told, and what reaches the model

`initialize` answers with the instructions from `mcp_prompts.json`
followed by a catalogue of every branch the key may read: measures by
table with uid, type, description and synonyms, dimensions by table with
uid and the start of their description, under 48 KB in all. A client
that puts the server's instructions in its system prompt then has what
an application has when it pastes the field catalogue into its own
prompt, and a question is one `sql` call. `fields` with no `q` returns
the same catalogue as one text, for the client that does not.

Measured on Claude Code 2.1, which is that client: it keeps about 2,000
characters of a server's instructions and marks the rest `[truncated]`,
so the catalogue in `initialize` never reaches the model there and the
instructions themselves are kept under that cap. It also loads MCP tool
definitions lazily behind a `ToolSearch` call, one extra model round
trip per conversation. `ENABLE_TOOL_SEARCH=false` in the environment
turns that off; in the eval it was the single largest gain: 11.6 s and
3.4 round trips per question against 14.8 s and 4.8, and a higher pass
rate, because the model sees the full tool schemas from the start and
makes its field searches in parallel. The demo application answers in
two model turns because its catalogue is in its own prompt and it has
no harness in between; through Claude Code the floor is the catalogue
or a search, the `sql` call, and the answer.

## Connecting

Claude Code, with a query key:

```
ENABLE_TOOL_SEARCH=false claude mcp add --transport http 0sql https://app.0sql.io/mcp \
  --header "Authorization: Bearer zqk_..."
```

`ENABLE_TOOL_SEARCH=false` belongs in the environment Claude Code runs
in, not only on the `add`; it applies to every MCP server the session
has.

claude.ai and Claude Desktop: a custom connector at the same URL with
"fixed credentials", the key as the bearer token. The API's MCP
connector takes the key as `authorization_token`.

The answer to `tools/call` carries both a `text` block with the JSON
pretty-printed and `structuredContent` with the same value. A refusal
from the gate or the planner comes back as the tool's own result with
`isError`, with the API's class and message, so the model reads the
reason rather than a transport failure.

## Tests

`crates/server/src/isolation_tests.rs` drives the real router for all
of it: a cookie is refused on `/mcp`, a wrong Origin gets 403 before
authentication, the challenge header is on every 401, GET and DELETE
are 405, a query key lists and plans only its granted branches and the
other account's columns are unreachable by any name, a revoked key is
refused on the very next call, no tool takes a context and one sent is
named, an unbound grant on a policy branch gets `ContextRequired` on
both paths, a bound grant plans as its context on both paths and
refuses any sent one, a developer who does not own the project cannot
bind, rotation keeps the binding and a plain re-grant clears it, and a
person over MCP plans as their email and nothing more.

## Later

- OAuth with zsqld as its own authorization server, so a client can sign
  a user in rather than paste a key. Not needed by any Claude client
  today; when it comes, the protected resource metadata goes on the 401
  and the rest of this document stands.
- Resources or prompts, if a use appears. Tools were enough to start.
