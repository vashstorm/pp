#!/usr/bin/env bash
# Integration tests only: handshake barrier fixture for startup connect.
# With MCP_BARRIER_DIR set the server publishes $$.pid at startup, writes $$.ready as soon as it enters the
# `initialize` handshake and blocks there until $MCP_BARRIER_DIR/release exists, so a test can observe several
# servers handshaking at the same time. MCP_INIT_DELAY_SECONDS delays the initialize answer and MCP_TOOLS lists
# the tools to expose.
if [[ -n "$MCP_BARRIER_DIR" ]]; then
  printf '%s' "$$" > "$MCP_BARRIER_DIR/$$.pid"
fi
schema='{"type":"object","properties":{"text":{"type":"string"}}}'
[[ -n "$MCP_BARRIER_INVALID_SCHEMA" ]] && schema='{"type":"array"}'
while IFS= read -r message; do
  [[ "$message" =~ \"id\":([0-9]+) ]] || continue
  id=${BASH_REMATCH[1]}
  case "$message" in
    *'"method":"initialize"'*)
      if [[ -n "$MCP_BARRIER_DIR" ]]; then
        printf '%s' "$$" > "$MCP_BARRIER_DIR/$$.ready"
        while [[ ! -e "$MCP_BARRIER_DIR/release" ]]; do sleep 0.01; done
      fi
      if [[ -n "$MCP_INIT_DELAY_SECONDS" ]]; then
        sleep "$MCP_INIT_DELAY_SECONDS"
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"barrier","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      tools=''
      for tool in ${MCP_TOOLS:-echo}; do
        [[ -n "$tools" ]] && tools="$tools,"
        tools="$tools{\"name\":\"$tool\",\"description\":\"barrier tool\",\"inputSchema\":$schema}"
      done
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[%s]}}\n' "$id" "$tools"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"barrier result"}],"isError":false}}\n' "$id"
      ;;
  esac
done
