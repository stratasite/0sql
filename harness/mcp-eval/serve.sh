#!/bin/sh
# Restart the local eval server with a variant's instruction files:
#   ./serve.sh hillclimb/v1
# Expects the data directory and pid file of the smoke setup in ZSQL_EVAL_STATE
# (default: $TMPDIR/zsql-eval).
set -e
V=$(cd "$1" && pwd)
S=${ZSQL_EVAL_STATE:-${TMPDIR:-/tmp}/zsql-eval}
BIN=${ZSQL:-$(cd "$(dirname "$0")/../.." && pwd)/target/debug/zsql}
[ -f "$S/pid" ] && kill "$(cat "$S/pid")" 2>/dev/null || true
sleep 0.5
ZSQL_DATA_DIR=$S/data ZSQL_BIND=127.0.0.1:3699 ZSQL_ACCOUNTS=on ZSQL_PUBLIC_URL=http://127.0.0.1:3699 ZSQL_LIMITS=off \
ZSQL_MCP_PROMPTS=$V/mcp_prompts.json ZSQL_MCP_SPEC_SCHEMA=$V/mcp_spec_schema.json \
nohup "$BIN" serve > "$S/log" 2>&1 &
echo $! > "$S/pid"
sleep 1
curl -sf 127.0.0.1:3699/healthz >/dev/null && echo "serving $1 (pid $(cat "$S/pid"))"
