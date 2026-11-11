# The MCP endpoint

`POST /mcp` on the server gives an agent the server's reads as tools.
`crates/server/src/mcp.rs` is the whole of it. First implemented
2026-10-09; reworked when accounts left the server, which is what this
describes.

## What it is

- **One route on the existing server**, `/mcp` beside the API, reached
  however the API is. Always on; nothing to configure.
- **Streamable HTTP, stateless, JSON only.** Each HTTP request carries
  one JSON-RPC message. No `Mcp-Session-Id`, no server-sent events, no
  GET stream: `GET` and `DELETE` answer 405.
- **Read-only tools, one per read the API has:** `list_projects`,
  `fields`, `tables`, `values`, `explore`, `sql`, `explain`. Each runs
  the function the matching REST handler runs. No deploy, no removal,
  and the server never executes SQL, so what the endpoint exposes is
  model metadata and SQL text, not rows.
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
  `tools/call` and version negotiation, which is a few hundred lines.
  The tool layer is plain functions over the handlers, so a library
  transport could replace the shell later without touching the tools.

Protocol revisions `2025-11-25`, `2025-06-18` and `2025-03-26` are
answered; a client asking for another gets the newest. An
`MCP-Protocol-Version` header naming anything else is a 400.

## No authentication

The endpoint answers whoever can reach it, like every read on the
server. There is no key, no challenge, no OAuth metadata (the
`/.well-known` discovery paths are plain 404s) and no Origin check. An
`Authorization` header is not read.

That makes it a component, not a public endpoint. It belongs in one of
two places:

- **Behind the developer's own service**, which authenticates its
  users, proxies `POST /mcp`, and sets the context header below per
  user. The service decides which projects a user may name as well:
  the server lists and plans every deployed branch for anyone.
- **On a loopback**, for the developer's own agent against the model
  they are editing.

A browser page can reach a loopback server by DNS rebinding. What it
gets is what any caller gets: the catalog and SQL text, and, on a
server with no deploy token, nothing more over this endpoint, since no
tool writes.

## The security context

A row-level policy reads the context a query is planned for. In it,
`system_admin` or `project_admin` set to true skips any policy that
allows the bypass; otherwise the policy resolves what the caller may
see from the email, the user's tags, or group names and tags, and a
policy whose `unresolved` is `allow` is skipped when nothing resolves.
So the context is a full bypass in one field, and an empty one is not a
safe default either.

Over the API the caller sends the context in the request body. Over
MCP a model writes the arguments, and a model must not say who it is
acting as, so the context is not an argument:

- **The `Zsql-Context` header carries it**, as the same JSON object the
  API takes in `context`. It applies to the one tool call the request
  carries. The service in front sets it per user; a client that
  connects directly has it set once in its configuration. The model
  never sees or writes it.
- **A `context` among a tool's arguments is refused** with its own
  message, never dropped silently. Every tool's schema refuses unknown
  properties.
- **No header is no context**, which a branch with policies refuses
  with `ContextRequired`, as the API does.
- **A header that is not a security context is a 400**: not JSON, not
  an object, or with a member the context does not have. Planning with
  no context when one was meant is the wrong answer to a typo.

The header is as trusted as the request body is over the API: whoever
can reach the server can claim any context. That is the design. The
service in front is what stands between a user and that claim, so it
must set the header itself and never pass one through from its caller.

## Admission

One HTTP request is one tool call. The guard admits `POST /mcp` as
`plan`; `fields` and `explore`, which cost more, then take an `explore`
seat as well inside the dispatcher, and answer 503 the way the API does
so a client's existing back-off applies. The 30 second deadline is the
plan deadline. The one-megabyte JSON body limit applies.

An `initialize` writes one journal line naming the client it declared.

## What the client is told, and what reaches the model

`initialize` answers with the instructions from `mcp_prompts.json`
followed by a catalog of every deployed branch: measures by
table with uid, type, description and synonyms, dimensions by table with
uid and the start of their description, under 48 KB in all. A client
that puts the server's instructions in its system prompt then has what
an application has when it pastes the field catalog into its own
prompt, and a question is one `sql` call. `fields` with no `q` returns
the same catalog as one text, for the client that does not.

Measured on Claude Code 2.1, which is that client: it keeps about 2,000
characters of a server's instructions and marks the rest `[truncated]`,
so the catalog in `initialize` never reaches the model there and the
instructions themselves are kept under that cap. It also loads MCP tool
definitions lazily behind a `ToolSearch` call, one extra model round
trip per conversation. `ENABLE_TOOL_SEARCH=false` in the environment
turns that off; in the eval it was the single largest gain: 11.6 s and
3.4 round trips per question against 14.8 s and 4.8, and a higher pass
rate, because the model sees the full tool schemas from the start and
makes its field searches in parallel. The demo application answers in
two model turns because its catalog is in its own prompt and it has
no harness in between; through Claude Code the floor is the catalog
or a search, the `sql` call, and the answer.

## Connecting

Claude Code, to a server on this machine:

```
ENABLE_TOOL_SEARCH=false claude mcp add --transport http 0sql http://127.0.0.1:3699/mcp \
  --header 'Zsql-Context: {"email":"me@example.com"}'
```

The header is only needed for a model with policies.
`ENABLE_TOOL_SEARCH=false` belongs in the environment Claude Code runs
in, not only on the `add`; it applies to every MCP server the session
has.

claude.ai and Claude Desktop reach a server over the internet, so they
connect to the service in front of this one, with whatever sign-in that
service has, and it adds the header.

The answer to `tools/call` carries both a `text` block with the JSON
pretty-printed and `structuredContent` with the same value. A refusal
from the planner comes back as the tool's own result with `isError`,
with the API's class and message, so the model reads the reason rather
than a transport failure.

## Tests

`crates/server/src/tests.rs` drives the real router: the handshake and
version negotiation, GET and DELETE are 405, the discovery paths are
404, there are seven tools and none takes a context or writes, the
header decides the plan on `sql` and `explain`, no header on a policy
branch gets `ContextRequired`, a context in the arguments is refused
with or without the header, a header that is not a context is a 400, a
caller's own filter on the context dimension narrows the policy and
never replaces it, the catalog lists every deployed branch, the default
branch is the one deployed or the project's production branch, and a
refused field comes back with the fields nearest it.

## Later

- Resources or prompts, if a use appears. Tools were enough to start.
