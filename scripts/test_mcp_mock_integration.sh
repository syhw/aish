#!/usr/bin/env bash
set -euo pipefail

# Local MCP integration test using a mock streamable-http MCP server.

HOST="${AISHD_MCP_IT_HOST:-127.0.0.1}"
PORT="${AISHD_MCP_IT_PORT:-5185}"
MCP_PORT="${AISHD_MCP_IT_MOCK_PORT:-5184}"
BASE_URL="http://${HOST}:${PORT}"
MCP_URL="http://${HOST}:${MCP_PORT}/mcp"
TMP_ROOT="${TMPDIR:-/tmp}/aish_mcp_it_$$"
CFG_PATH="${TMP_ROOT}/aishd_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
MCP_LOG="${TMP_ROOT}/mcp.log"
MCP_SCRIPT="${TMP_ROOT}/mock_mcp.py"
AISHD_BIN="${AISHD_MCP_IT_BIN:-target/debug/aishd}"
KEEP="${AISHD_MCP_IT_KEEP:-0}"
AISHD_PID=""
MCP_PID=""

cleanup() {
  if [[ -n "${AISHD_PID}" ]] && kill -0 "${AISHD_PID}" >/dev/null 2>&1; then
    kill -TERM "${AISHD_PID}" >/dev/null 2>&1 || true
    wait "${AISHD_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${MCP_PID}" ]] && kill -0 "${MCP_PID}" >/dev/null 2>&1; then
    kill -TERM "${MCP_PID}" >/dev/null 2>&1 || true
    wait "${MCP_PID}" >/dev/null 2>&1 || true
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
    sleep 0.2
  done
  return 1
}

mkdir -p "${TMP_ROOT}"

cat >"${MCP_SCRIPT}" <<'PY'
#!/usr/bin/env python3
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        return

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(size).decode("utf-8", errors="replace")
        try:
            req = json.loads(raw)
        except Exception:
            req = {"raw": raw}

        method = req.get("method")
        req_id = req.get("id")
        if method == "initialize":
            body = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.1.0"},
                },
            }
            encoded = json.dumps(body).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Mcp-Session-Id", "mock-session-1")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
            return
        if method == "notifications/initialized":
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if method == "tools/list":
            body = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": [
                        {
                            "name": "web_search_prime",
                            "description": "Mock web search tool",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": {"type": "string"},
                                    "max_results": {"type": "integer"},
                                },
                                "required": ["query"],
                            },
                        }
                    ]
                },
            }
            encoded = json.dumps(body).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
            return
        if method == "tools/call":
            params = req.get("params", {})
            args = params.get("arguments", {})
            query = args.get("query", "")
            max_results = args.get("max_results", 0)
            body = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [
                        {"type": "text", "text": f"mock search for: {query} (max={max_results})"}
                    ]
                },
            }
            encoded = json.dumps(body).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
            return

        body = {"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "method not found"}}
        encoded = json.dumps(body).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", 5184), Handler)
    server.serve_forever()
PY
chmod +x "${MCP_SCRIPT}"

cat >"${CFG_PATH}" <<JSON
{
  "server": { "hostname": "${HOST}", "port": ${PORT} },
  "logging": { "dir": "${TMP_ROOT}/logs", "retention_days": 7 },
  "tools": {
    "default_policy": "ask",
    "policies": {
      "mcp": "allow"
    }
  },
  "mcpServers": {
    "web-search-prime": {
      "type": "streamable-http",
      "url": "${MCP_URL}"
    }
  }
}
JSON

python3 "${MCP_SCRIPT}" >"${MCP_LOG}" 2>&1 &
MCP_PID="$!"
sleep 0.2

if [[ ! -x "${AISHD_BIN}" ]]; then
  cargo build -p aishd >/dev/null
fi

"${AISHD_BIN}" --config "${CFG_PATH}" --bind "${HOST}:${PORT}" >"${DAEMON_LOG}" 2>&1 &
AISHD_PID="$!"
if ! wait_http "${BASE_URL}/health"; then
  echo "aishd did not become healthy at ${BASE_URL}" >&2
  tail -n 120 "${DAEMON_LOG}" >&2 || true
  exit 1
fi

TOOLS_RESP="$(curl -fsS -X POST "${BASE_URL}/v1/tools/mcp.list_tools/call" \
  -H 'content-type: application/json' \
  -d '{"args":{"server":"web-search-prime"},"approved":true}')"

python3 - <<'PY' "${TOOLS_RESP}"
import json, sys
data = json.loads(sys.argv[1])
tools = data.get("result", {}).get("tools", [])
assert isinstance(tools, list) and len(tools) == 1
assert tools[0]["name"] == "web_search_prime"
print("mcp.list_tools ok")
PY

SEARCH_RESP="$(curl -fsS -X POST "${BASE_URL}/v1/tools/mcp.web_search/call" \
  -H 'content-type: application/json' \
  -d '{"args":{"server":"web-search-prime","query":"latest rust release","max_results":3},"approved":true}')"

python3 - <<'PY' "${SEARCH_RESP}"
import json, sys
data = json.loads(sys.argv[1])
res = data.get("result", {})
assert res.get("tool") == "web_search_prime"
text = res.get("content_text", "")
assert "latest rust release" in text
assert "max=3" in text
print("mcp.web_search ok")
PY

echo "PASS: MCP mock integration validated at ${BASE_URL}"
