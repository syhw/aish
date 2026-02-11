#!/usr/bin/env bash
set -euo pipefail

# Live MCP web-search smoke test against Z.ai web_search_prime MCP server.
# Requires: ZAI_API_KEY

if [[ -z "${ZAI_API_KEY:-}" ]]; then
  echo "ZAI_API_KEY is required" >&2
  exit 2
fi

HOST="${AISHD_MCP_HOST:-127.0.0.1}"
PORT="${AISHD_MCP_PORT:-5183}"
BASE_URL="http://${HOST}:${PORT}"
TMP_ROOT="${TMPDIR:-/tmp}/aish_mcp_test_$$"
LOG_DIR="${TMP_ROOT}/logs"
CFG_PATH="${TMP_ROOT}/aish_mcp_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
AISHD_BIN="${AISHD_MCP_BIN:-target/debug/aishd}"
KEEP="${AISHD_MCP_KEEP:-0}"
PID=""

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "${PID}" >/dev/null 2>&1; then
    kill -TERM "${PID}" >/dev/null 2>&1 || true
    wait "${PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${KEEP}" != "1" ]]; then
    rm -rf "${TMP_ROOT}"
  else
    echo "kept artifacts at ${TMP_ROOT}" >&2
  fi
}
trap cleanup EXIT

wait_http() {
  local url="$1"
  for _ in $(seq 1 80); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

mkdir -p "${TMP_ROOT}" "${LOG_DIR}"

cat >"${CFG_PATH}" <<JSON
{
  "server": { "hostname": "${HOST}", "port": ${PORT} },
  "logging": { "dir": "${LOG_DIR}", "retention_days": 7 },
  "tools": {
    "default_policy": "ask",
    "policies": {
      "mcp": "allow"
    }
  },
  "mcpServers": {
    "web-search-prime": {
      "type": "streamable-http",
      "url": "https://api.z.ai/api/mcp/web_search_prime/mcp",
      "headers": {
        "Authorization": "Bearer your_api_key"
      }
    }
  }
}
JSON

if [[ ! -x "${AISHD_BIN}" ]]; then
  cargo build -p aishd >/dev/null
fi

"${AISHD_BIN}" --config "${CFG_PATH}" --bind "${HOST}:${PORT}" >"${DAEMON_LOG}" 2>&1 &
PID="$!"

if ! wait_http "${BASE_URL}/health"; then
  echo "aishd did not become healthy at ${BASE_URL}" >&2
  tail -n 120 "${DAEMON_LOG}" >&2 || true
  exit 1
fi

TOOLS_BODY="${TMP_ROOT}/tools_resp.json"
TOOLS_STATUS="$(curl -sS -o "${TOOLS_BODY}" -w "%{http_code}" -X POST "${BASE_URL}/v1/tools/mcp.list_tools/call" \
  -H 'content-type: application/json' \
  -d '{"args":{"server":"web-search-prime"},"approved":true}')"
if [[ "${TOOLS_STATUS}" != "200" ]]; then
  echo "mcp.list_tools failed with HTTP ${TOOLS_STATUS}" >&2
  cat "${TOOLS_BODY}" >&2
  exit 1
fi
TOOLS_RESP="$(cat "${TOOLS_BODY}")"

python3 - <<'PY' "${TOOLS_RESP}"
import json, sys
data = json.loads(sys.argv[1])
tools = data.get("result", {}).get("tools", [])
if not isinstance(tools, list) or not tools:
    raise SystemExit("mcp.list_tools returned no tools")
print(f"mcp.list_tools ok: {len(tools)} tools")
PY

QUERY="${1:-latest MCP protocol streamable-http summary}"
SEARCH_BODY="${TMP_ROOT}/search_resp.json"
SEARCH_PAYLOAD="$(python3 - <<'PY' "$QUERY"
import json, sys
print(json.dumps({
  "args": {"server": "web-search-prime", "query": sys.argv[1], "max_results": 5},
  "approved": True
}))
PY
)"
SEARCH_STATUS="$(curl -sS -o "${SEARCH_BODY}" -w "%{http_code}" -X POST "${BASE_URL}/v1/tools/mcp.web_search/call" \
  -H 'content-type: application/json' \
  -d "${SEARCH_PAYLOAD}")"
if [[ "${SEARCH_STATUS}" != "200" ]]; then
  echo "mcp.web_search failed with HTTP ${SEARCH_STATUS}" >&2
  cat "${SEARCH_BODY}" >&2
  exit 1
fi
SEARCH_RESP="$(cat "${SEARCH_BODY}")"

python3 - <<'PY' "${SEARCH_RESP}"
import json, sys
data = json.loads(sys.argv[1])
result = data.get("result", {})
tool = result.get("tool")
text = result.get("content_text", "")
if not tool:
    raise SystemExit("mcp.web_search missing selected tool")
if not isinstance(text, str):
    raise SystemExit("mcp.web_search content_text invalid")
print(f"mcp.web_search ok: tool={tool}")
print("snippet:")
print((text[:400] + "...") if len(text) > 400 else text)
PY

echo "PASS: MCP web search test succeeded via ${BASE_URL}"
