#!/usr/bin/env bash
set -euo pipefail

# Comprehensive integration test for context engineering and llm-select retrieval.
# Covers:
# 1) ingest + deterministic context bundle retrieval
# 2) intent detection + override + explain + budget
# 3) /v1/completions context injection in always mode
# 4) llm-select chunk selector + synthesis prompt composition
# 5) llm-select fallback to deterministic retrieval when selector fails

HOST="${AISHD_CTX_HOST:-127.0.0.1}"
PORT="${AISHD_CTX_PORT:-5163}"
MOCK_PORT="${AISHD_CTX_MOCK_PORT:-5164}"
BASE_URL="http://${HOST}:${PORT}"
MOCK_URL="http://${HOST}:${MOCK_PORT}"
TMP_ROOT="${TMPDIR:-/tmp}/aish_ctx_eng_it_$$"
LOG_DIR="${TMP_ROOT}/logs"
CFG_PATH="${TMP_ROOT}/aish_ctx_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
MOCK_LOG="${TMP_ROOT}/mock.log"
MOCK_REQ_LOG="${TMP_ROOT}/mock_requests.jsonl"
MOCK_SCRIPT="${TMP_ROOT}/mock_openai.py"
AISH_BIN="${AISHD_CTX_BIN:-target/debug/aishd}"
KEEP_ARTIFACTS="${AISHD_CTX_KEEP:-0}"
PREFIX="aish-ctx-${RANDOM}-$$"
DAEMON_PID=""
MOCK_PID=""

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

json_get() {
  python3 - <<'PY' "$1" "$2"
import json, sys
obj = json.loads(sys.argv[1])
path = sys.argv[2]
cur = obj
for part in path.split('.'):
    if part.isdigit():
        cur = cur[int(part)]
    else:
        cur = cur[part]
print(cur)
PY
}

json_assert() {
  local json="$1"
  local expr="$2"
  local msg="$3"
  python3 - <<'PY' "$json" "$expr" "$msg"
import json, sys
obj = json.loads(sys.argv[1])
expr = sys.argv[2]
msg = sys.argv[3]
try:
    ok = eval(expr, {"data": obj})
except Exception as exc:
    raise SystemExit(f"{msg}: {exc}")
if not ok:
    raise SystemExit(msg)
PY
}

api_get() {
  curl -fsS "$1"
}

api_post() {
  local url="$1"
  local body="$2"
  curl -fsS "$url" -H 'content-type: application/json' -d "$body"
}

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

write_config() {
  mkdir -p "${TMP_ROOT}" "${LOG_DIR}"
  cat >"${CFG_PATH}" <<JSON
{
  "server": { "hostname": "${HOST}", "port": ${PORT} },
  "tmux": { "session_prefix": "${PREFIX}", "attach_on_start": false },
  "logging": { "dir": "${LOG_DIR}", "retention_days": 30 },
  "share": "manual",
  "providers": {
    "openai_compat": {
      "base_url": "${MOCK_URL}",
      "api_key": "mock-key",
      "model": "mock-model",
      "completions_path": "/v1/chat/completions"
    },
    "openai_compat_profiles": {},
    "model_aliases": {}
  },
  "tools": { "default_policy": "ask", "policies": {} }
}
JSON
}

write_mock_server() {
  cat >"${MOCK_SCRIPT}" <<'PY'
#!/usr/bin/env python3
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

REQ_LOG = os.environ["MOCK_REQ_LOG"]


def write_req(obj):
    with open(REQ_LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps(obj, separators=(",", ":")) + "\n")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        return

    def _read_json(self):
        size = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(size).decode("utf-8", errors="replace")
        try:
            body = json.loads(raw)
        except Exception:
            body = {"raw": raw}
        return raw, body

    def do_POST(self):
        raw, body = self._read_json()
        messages = body.get("messages") or []
        system = ""
        user = ""
        for msg in messages:
            if msg.get("role") == "system" and not system:
                system = str(msg.get("content", ""))
            if msg.get("role") == "user":
                user = str(msg.get("content", ""))

        kind = "final"
        if "context selector for debugging and incident analysis" in system:
            kind = "selector"
        elif "building final context for another LLM call" in system:
            kind = "synthesis"

        write_req({
            "path": self.path,
            "kind": kind,
            "system": system,
            "user": user,
            "body": body,
            "raw": raw,
        })

        if kind == "selector" and "force_selector_failure" in user:
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": "forced selector failure"}).encode("utf-8"))
            return

        if kind == "selector":
            content = (
                "- Relevant evidence: command failed with exit 101\n"
                "- Relevant evidence: stderr has cannot find value\n"
                "- Why: directly explains failing build"
            )
        elif kind == "synthesis":
            content = (
                "1) Most relevant evidence\n"
                "- cargo test failed with exit 101\n"
                "- stderr: cannot find value\n"
                "2) Likely failure chain\n"
                "- compile error caused test command failure\n"
                "3) Files/commands to inspect first\n"
                "- src/lib.rs\n"
                "- cargo test --workspace"
            )
        else:
            content = "FINAL_ANSWER_OK"

        resp = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": content},
                }
            ],
        }
        encoded = json.dumps(resp).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


if __name__ == "__main__":
    host = os.environ.get("MOCK_HOST", "127.0.0.1")
    port = int(os.environ.get("MOCK_PORT", "5164"))
    server = HTTPServer((host, port), Handler)
    server.serve_forever()
PY
}

start_mock() {
  : >"${MOCK_REQ_LOG}"
  MOCK_REQ_LOG="${MOCK_REQ_LOG}" MOCK_HOST="${HOST}" MOCK_PORT="${MOCK_PORT}" \
    python3 "${MOCK_SCRIPT}" >"${MOCK_LOG}" 2>&1 &
  MOCK_PID="$!"
  if ! wait_http "${MOCK_URL}/v1/chat/completions"; then
    # endpoint is POST-only, so health check via process + short sleep fallback
    sleep 0.5
  fi
}

start_daemon() {
  "${AISH_BIN}" --config "${CFG_PATH}" --bind "${HOST}:${PORT}" >"${DAEMON_LOG}" 2>&1 &
  DAEMON_PID="$!"
  if ! wait_http "${BASE_URL}/health"; then
    echo "aishd did not become healthy at ${BASE_URL}" >&2
    tail -n 120 "${DAEMON_LOG}" >&2 || true
    exit 1
  fi
}

stop_proc() {
  local pid="$1"
  if [[ -n "${pid}" ]] && kill -0 "${pid}" >/dev/null 2>&1; then
    kill -TERM "${pid}" >/dev/null 2>&1 || true
    wait "${pid}" >/dev/null 2>&1 || true
  fi
}

cleanup() {
  stop_proc "${DAEMON_PID}"
  stop_proc "${MOCK_PID}"
  if [[ "${KEEP_ARTIFACTS}" == "1" ]]; then
    echo "kept artifacts at ${TMP_ROOT}" >&2
  else
    rm -rf "${TMP_ROOT}"
  fi
}
trap cleanup EXIT

seed_session_logs() {
  local sid="$1"
  mkdir -p "${LOG_DIR}/${sid}"
  cat >"${LOG_DIR}/${sid}/events.jsonl" <<JSON
{"ts_ms":1000,"session_id":"${sid}","kind":"command.start","data":{"cmd":"cargo test --workspace"}}
{"ts_ms":1001,"session_id":"${sid}","kind":"command.end","data":{"exit":101}}
{"ts_ms":1002,"session_id":"${sid}","kind":"tool.end","data":{"tool":"shell","ok":false,"error":"build failed"}}
{"ts_ms":1003,"session_id":"${sid}","kind":"tool.start","data":{"tool":"fs.write","args":{"path":"src/lib.rs","append":false,"content":"fn add(){}"}}}
JSON

  cat >"${LOG_DIR}/${sid}/stdin.log" <<'TXT'
cargo test --workspace
resume from where we left off
which files changed
TXT

  cat >"${LOG_DIR}/${sid}/stdout.log" <<'TXT'
Compiling aishd v0.0.1
Finished dev profile
TXT

  cat >"${LOG_DIR}/${sid}/stderr.log" <<'TXT'
error[E0425]: cannot find value `foo` in this scope
TXT

  cat >"${LOG_DIR}/${sid}/output.log" <<'TXT'
build started
build failed
TXT
}

assert_mock_contains() {
  local expr="$1"
  local msg="$2"
  python3 - <<'PY' "${MOCK_REQ_LOG}" "$expr" "$msg"
import json, sys
path, expr, msg = sys.argv[1:4]
rows = []
with open(path, "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if line:
            rows.append(json.loads(line))
try:
    ok = eval(expr, {"rows": rows})
except Exception as exc:
    raise SystemExit(f"{msg}: {exc}")
if not ok:
    raise SystemExit(msg)
PY
}

main() {
  require_cmd cargo
  require_cmd curl
  require_cmd python3
  require_cmd tmux

  mkdir -p "${TMP_ROOT}"
  write_config
  write_mock_server

  if [[ "${AISH_BIN}" == "target/debug/aishd" ]]; then
    cargo build -p aishd >/dev/null
  fi

  start_mock
  start_daemon

  local health
  health="$(api_get "${BASE_URL}/health")"
  json_assert "${health}" 'data.get("status") == "ok"' "expected daemon health ok"

  local session_json sid ingest
  session_json="$(api_post "${BASE_URL}/v1/sessions" '{"title":"ctx-it"}')"
  sid="$(json_get "${session_json}" "id")"
  seed_session_logs "${sid}"

  ingest="$(api_post "${BASE_URL}/v1/logs/ingest/${sid}" '{}')"
  json_assert "${ingest}" 'data.get("events_inserted", 0) >= 4' "expected events ingested"
  json_assert "${ingest}" 'data.get("stdin_lines_inserted", 0) >= 3' "expected stdin ingested"
  json_assert "${ingest}" 'data.get("output_lines_inserted", 0) >= 4' "expected output ingested"

  local ctx_debug ctx_resume ctx_override ctx_explain ctx_budget
  ctx_debug="$(api_get "${BASE_URL}/v1/logs/context/${sid}?q=what%20did%20i%20do%20wrong%20with%20cargo%20test")"
  json_assert "${ctx_debug}" 'data.get("intent") == "debug"' "expected debug intent"
  json_assert "${ctx_debug}" 'len(data.get("incidents", [])) >= 1' "expected incidents"
  json_assert "${ctx_debug}" 'len(data.get("artifacts", [])) >= 1' "expected artifacts"
  json_assert "${ctx_debug}" '"Recent command failures" in data.get("context_text","")' "expected failure section"

  ctx_resume="$(api_get "${BASE_URL}/v1/logs/context/${sid}?q=resume%20from%20where%20we%20left%20off")"
  json_assert "${ctx_resume}" 'data.get("intent") == "resume"' "expected resume intent"

  ctx_override="$(api_get "${BASE_URL}/v1/logs/context/${sid}?q=what%20failed&intent=status")"
  json_assert "${ctx_override}" 'data.get("intent") == "status"' "expected override intent"

  ctx_explain="$(api_get "${BASE_URL}/v1/logs/context/${sid}?q=why%20did%20it%20fail&explain=true")"
  json_assert "${ctx_explain}" 'isinstance(data.get("explain"), dict)' "expected explain payload"

  ctx_budget="$(api_get "${BASE_URL}/v1/logs/context/${sid}?q=why%20did%20it%20fail&max_chars=180")"
  json_assert "${ctx_budget}" 'len(data.get("context_text", "")) <= 180' "expected max_chars budget"

  local det_resp
  det_resp="$(api_post "${BASE_URL}/v1/completions" "{\"session_id\":\"${sid}\",\"context_mode\":\"always\",\"messages\":[{\"role\":\"user\",\"content\":\"why did cargo test fail?\"}]}")"
  json_assert "${det_resp}" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "FINAL_ANSWER_OK"' "expected completion response"

  # Isolate llm-select request trace.
  : >"${MOCK_REQ_LOG}"

  local sel_resp
  sel_resp="$(api_post "${BASE_URL}/v1/completions" "{\"session_id\":\"${sid}\",\"context_mode\":\"llm-select\",\"context_selector_chunk_tokens\":4096,\"context_selector_max_chunks\":8,\"context_selector_include_events\":true,\"messages\":[{\"role\":\"user\",\"content\":\"what did i do wrong and what should i do next?\"}]}")"
  json_assert "${sel_resp}" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "FINAL_ANSWER_OK"' "expected llm-select completion response"

  assert_mock_contains 'len(rows) >= 3' "expected selector + synthesis + final requests"
  assert_mock_contains 'any(r.get("kind") == "selector" for r in rows)' "expected selector request"
  assert_mock_contains 'any(r.get("kind") == "synthesis" for r in rows)' "expected synthesis request"
  assert_mock_contains 'any(r.get("kind") == "selector" and "Recent context (always include mentally):" in r.get("user", "") for r in rows)' "expected selector prompt to include recent context header"
  assert_mock_contains 'any(r.get("kind") == "selector" and "Last stderr:" in r.get("user", "") for r in rows)' "expected selector prompt to include last stderr"
  assert_mock_contains 'any(r.get("kind") == "final" and "LLM-selected relevant context:" in r.get("system", "") for r in rows)' "expected final call to include synthesized selected context"

  # Fallback mode: force selector failure and verify final still succeeds via deterministic fallback.
  : >"${MOCK_REQ_LOG}"
  local fallback_resp
  fallback_resp="$(api_post "${BASE_URL}/v1/completions" "{\"session_id\":\"${sid}\",\"context_mode\":\"llm-select\",\"messages\":[{\"role\":\"user\",\"content\":\"force_selector_failure what failed?\"}]}")"
  json_assert "${fallback_resp}" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "FINAL_ANSWER_OK"' "expected fallback completion response"
  assert_mock_contains 'any(r.get("kind") == "selector" for r in rows)' "expected selector attempt before fallback"
  assert_mock_contains 'any(r.get("kind") == "final" and "LLM-selected relevant context:" not in r.get("system", "") and "Use this indexed shell/agent execution context" in r.get("system", "") for r in rows)' "expected final call to use deterministic context after selector failure"

  echo "PASS: context engineering integration workflow validated at ${BASE_URL}"
}

main "$@"
