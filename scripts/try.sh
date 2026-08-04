#!/usr/bin/env bash
#
# Drive Bosun over stdio exactly as an MCP client does, without needing a client.
#
#   ./scripts/try.sh                                        # handshake + list tools
#   ./scripts/try.sh bosun_info
#   ./scripts/try.sh list_containers '{"all":true}'
#   ./scripts/try.sh diagnose_container '{"id":"my-container"}'
#   ./scripts/try.sh container_logs '{"id":"my-container","level":"error"}'
#   ./scripts/try.sh container_rm '{"id":"my-container","dry_run":true}'
#
# Server logs go to stderr (shown); JSON-RPC replies are parsed and printed.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${BOSUN_BIN:-$HERE/../target/release/bosun}"
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }

TOOL="${1:-}"
# Default to an empty JSON object. Written this way because "${2:-\{\}}" keeps
# the backslashes literally and produces invalid JSON on the wire.
ARGS="${2:-}"
[ -n "$ARGS" ] || ARGS='{}'

{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"try.sh","version":"0"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  if [ -n "$TOOL" ]; then
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' "$TOOL" "$ARGS"
  else
    echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  fi
  # Hold stdin open long enough for the daemon to answer.
  sleep 3
} | "$BIN" --log-level warn | python3 "$HERE/_render.py"
