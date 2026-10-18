#!/usr/bin/env bash
# 仅用于集成测试；协议请求原文写入独立日志，由 Rust 断言检查。
printf '%s' "$$" > "$MCP_LOG.pid"
trap 'printf closed > "$MCP_LOG.closed"' EXIT
count=0
while IFS= read -r message; do
  printf '%s\n' "$message" >> "$MCP_LOG"
  [[ "$message" =~ \"id\":([0-9]+) ]] || continue
  id=${BASH_REMATCH[1]}
  case "$message" in
    *'"method":"initialize"'*)
      case "$MCP_MODE" in
        timeout_init) sleep 30; exit ;;
        invalid_json) printf 'secret-invalid-json\n'; continue ;;
        oversized) head -c 1048580 /dev/zero | tr '\0' x; continue ;;
        eof) exit ;;
        wrong_id) printf '{"jsonrpc":"2.0","id":999,"result":{}}\n'; continue ;;
        init_error) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-1,"message":"secret-rpc-body","data":"secret-config"}}\n' "$id"; continue ;;
      esac
      version=2025-11-25
      [[ "$MCP_MODE" = old_version ]] && version=2024-11-05
      [[ "$MCP_MODE" = bad_version ]] && version=unknown
      capabilities='{"tools":{}}'
      [[ "$MCP_MODE" = no_tools ]] && capabilities='{}'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":%s,"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" "$version" "$capabilities"
      ;;
    *'"method":"tools/list"'*)
      printf 'secret-stderr\n' >&2
      if [[ "$message" = *'"cursor"'* ]]; then
        cursor=''
        [[ "$MCP_MODE" = cursor_loop ]] && cursor=',"nextCursor":"page-2"'
        tool=fail
        [[ "$MCP_MODE" = duplicate ]] && tool=echo
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"%s","inputSchema":{"type":"object"}}]%s}}\n' "$id" "$tool" "$cursor"
      else
        tool=echo
        [[ "$MCP_MODE" = special_name ]] && tool='文件.search'
        [[ "$MCP_MODE" = long_name ]] && printf -v tool '%080d' 0
        schema='{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}'
        [[ "$MCP_MODE" = invalid_schema ]] && schema='{"type":"array"}'
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"%s","description":"测试工具","inputSchema":%s}],"nextCursor":"page-2"}}\n' "$id" "$tool" "$schema"
      fi
      if [[ "$MCP_MODE" = blocked_write && "$message" = *'"cursor"'* ]]; then sleep 30; exit; fi
      ;;
    *'"method":"tools/call"'*)
      count=$((count + 1))
      case "$MCP_MODE" in
        call_timeout) sleep 30; exit ;;
        call_eof) exit ;;
        call_bad_json) printf 'secret-invalid-json\n'; continue ;;
        rpc_error) if [[ $count = 1 ]]; then
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-1,"message":"secret-rpc-body"}}\n' "$id"
          continue
        fi ;;
      esac
      printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"data":"secret-notification"}}\n'
      printf '{"jsonrpc":"2.0","id":"server-ping","method":"ping"}\n'
      IFS= read -r ping
      printf '%s\n' "$ping" >> "$MCP_LOG"
      printf '{"jsonrpc":"2.0","id":"unsupported","method":"sampling/createMessage","params":{}}\n'
      IFS= read -r unsupported
      printf '%s\n' "$unsupported" >> "$MCP_LOG"
      failed=false
      [[ "$message" = *'"name":"fail"'* ]] && failed=true
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"structuredContent":{"count":%s},"isError":%s}}\n' "$id" "$MCP_VALUE" "$count" "$failed"
      ;;
  esac
done
