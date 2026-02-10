#!/usr/bin/env bash
set -euo pipefail

# HTTP-level integration test for completion context retrieval ledger rows.
# Verifies ledger persistence for:
# 1) successful deterministic retrieval completion
# 2) successful llm-select retrieval completion
# 3) upstream provider error on final completion call

HOST="${AISHD_LEDGER_HOST:-127.0.0.1}"
PORT="${AISHD_LEDGER_PORT:-5173}"
MOCK_PORT="${AISHD_LEDGER_MOCK_PORT:-5174}"
BASE_URL="http://${HOST}:${PORT}"
MOCK_URL="http://${HOST}:${MOCK_PORT}"
TMP_ROOT="${TMPDIR:-/tmp}/aish_ledger_http_it_$$"
LOG_DIR="${TMP_ROOT}/logs"
CFG_PATH="${TMP_ROOT}/aish_ledger_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
MOCK_LOG="${TMP_ROOT}/mock.log"
MOCK_REQ_LOG="${TMP_ROOT}/mock_requests.jsonl"
MOCK_SCRIPT="${TMP_ROOT}/mock_openai.py"
AISH_BIN="${AISHD_LEDGER_BIN:-target/debug/aishd}"
KEEP_ARTIFACTS="${AISHD_LEDGER_KEEP:-0}"
PREFIX="aish-ledger-${RANDOM}-$$"
DAEMON_PID=""
MOCK_PID=""

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
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

db_assert() {
  local sid="$1"
  local expr="$2"
  local msg="$3"
  python3 - <<'PY' "${LOG_DIR}/logs.sqlite" "$sid" "$expr" "$msg"
import json, sqlite3, sys
db_path, sid, expr, msg = sys.argv[1:5]
conn = sqlite3.connect(db_path)
rows = conn.execute(
    """
    SELECT context_mode, retriever, selector_used, selector_chunks, fallback_used,
           context_chars, answer_chars, status, COALESCE(error, '') AS error
    FROM context_retrieval_ledger
    WHERE session_id = ?
    ORDER BY id ASC
    """,
    (sid,),
).fetchall()
conn.close()
data = [
    {
        "context_mode": r[0],
        "retriever": r[1],
        "selector_used": int(r[2]),
        "selector_chunks": int(r[3]),
        "fallback_used": int(r[4]),
        "context_chars": int(r[5]),
        "answer_chars": int(r[6]),
        "status": r[7],
        "error": r[8],
    }
    for r in rows
]
try:
    ok = eval(expr, {"rows": data})
except Exception as exc:
    raise SystemExit(f"{msg}: {exc}\nrows={json.dumps(data, indent=2)}")
if not ok:
    raise SystemExit(f"{msg}\nrows={json.dumps(data, indent=2)}")
PY
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
        return body

    def do_POST(self):
        body = self._read_json()
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

        write_req({"path": self.path, "kind": kind, "system": system, "user": user})

        if kind == "final" and "FORCE_FINAL_PROVIDER_ERROR" in user:
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": "forced final failure"}).encode("utf-8"))
            return

        if kind == "selector":
            content = "- Relevant evidence: stderr has traceback\n- Why: root cause signal"
        elif kind == "synthesis":
            content = (
                "1) Most relevant evidence\n- command failed with non-zero exit\n"
                "2) Likely failure chain\n- compile/runtime error propagated\n"
                "3) Files/commands to inspect first\n- src/lib.rs\n"
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
    port = int(os.environ.get("MOCK_PORT", "5174"))
    server = HTTPServer((host, port), Handler)
    server.serve_forever()
PY
}

start_mock() {
  : >"${MOCK_REQ_LOG}"
  MOCK_REQ_LOG="${MOCK_REQ_LOG}" MOCK_HOST="${HOST}" MOCK_PORT="${MOCK_PORT}" \
    python3 "${MOCK_SCRIPT}" >"${MOCK_LOG}" 2>&1 &
  MOCK_PID="$!"
  sleep 0.3
}

start_daemon() {
  "${AISH_BIN}" --config "${CFG_PATH}" --bind "${HOST}:${PORT}" >"${DAEMON_LOG}" 2>&1 &
  DAEMON_PID="$!"
  if ! wait_http "${BASE_URL}/health"; then
    echo "aishd failed to become healthy at ${BASE_URL}" >&2
    tail -n 200 "${DAEMON_LOG}" >&2 || true
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

seed_logs() {
  local sid="$1"
  mkdir -p "${LOG_DIR}/${sid}"
  cat >"${LOG_DIR}/${sid}/events.jsonl" <<JSON
{"ts_ms":1000,"session_id":"${sid}","kind":"command.start","data":{"cmd":"cargo test"}}
{"ts_ms":1001,"session_id":"${sid}","kind":"command.end","data":{"exit":101}}
{"ts_ms":1002,"session_id":"${sid}","kind":"tool.end","data":{"tool":"shell","ok":false,"error":"test failed"}}
JSON
  cat >"${LOG_DIR}/${sid}/stdin.log" <<'TXT'
cargo test
llm explain failure
TXT
  cat >"${LOG_DIR}/${sid}/stderr.log" <<'TXT'
error[E0425]: cannot find value `foo` in this scope
TXT
  cat >"${LOG_DIR}/${sid}/stdout.log" <<'TXT'
running 5 tests
TXT
}

call_completion_with_status() {
  local payload="$1"
  local out_file="$2"
  curl -sS -o "${out_file}" -w "%{http_code}" \
    "${BASE_URL}/v1/completions" \
    -H 'content-type: application/json' \
    -d "${payload}"
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
  health="$(curl -fsS "${BASE_URL}/health")"
  json_assert "${health}" 'data.get("status") == "ok"' "health must be ok"

  local session_json sid ingest
  session_json="$(curl -fsS "${BASE_URL}/v1/sessions" -H 'content-type: application/json' -d '{"title":"ledger-http-it"}')"
  sid="$(python3 - <<'PY' "${session_json}"
import json, sys
print(json.loads(sys.argv[1])["id"])
PY
)"
  seed_logs "${sid}"
  ingest="$(curl -fsS "${BASE_URL}/v1/logs/ingest/${sid}" -H 'content-type: application/json' -d '{}')"
  json_assert "${ingest}" 'data.get("events_inserted", 0) >= 3' "expected events ingested"

  local status_ok resp_ok
  resp_ok="${TMP_ROOT}/resp_ok.json"
  status_ok="$(call_completion_with_status "{\"session_id\":\"${sid}\",\"context_mode\":\"always\",\"messages\":[{\"role\":\"user\",\"content\":\"why did cargo test fail?\"}]}" "${resp_ok}")"
  [[ "${status_ok}" == "200" ]] || { echo "expected 200, got ${status_ok}" >&2; cat "${resp_ok}" >&2; exit 1; }
  json_assert "$(cat "${resp_ok}")" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "FINAL_ANSWER_OK"' "expected successful completion content"

  local status_sel resp_sel
  resp_sel="${TMP_ROOT}/resp_sel.json"
  status_sel="$(call_completion_with_status "{\"session_id\":\"${sid}\",\"context_mode\":\"llm-select\",\"context_selector_chunk_tokens\":4096,\"context_selector_max_chunks\":8,\"messages\":[{\"role\":\"user\",\"content\":\"what context matters most?\"}]}" "${resp_sel}")"
  [[ "${status_sel}" == "200" ]] || { echo "expected 200 llm-select, got ${status_sel}" >&2; cat "${resp_sel}" >&2; exit 1; }

  local status_err resp_err
  resp_err="${TMP_ROOT}/resp_err.json"
  status_err="$(call_completion_with_status "{\"session_id\":\"${sid}\",\"context_mode\":\"always\",\"messages\":[{\"role\":\"user\",\"content\":\"FORCE_FINAL_PROVIDER_ERROR why did this fail?\"}]}" "${resp_err}")"
  [[ "${status_err}" == "500" ]] || { echo "expected 500 provider failure, got ${status_err}" >&2; cat "${resp_err}" >&2; exit 1; }

  db_assert "${sid}" 'len(rows) >= 3' "expected at least three ledger rows"
  db_assert "${sid}" 'any(r["status"] == "ok" and r["context_mode"] == "always" and r["retriever"] == "lexical" and r["context_chars"] > 0 and r["answer_chars"] > 0 for r in rows)' "missing successful deterministic ledger row"
  db_assert "${sid}" 'any(r["status"] == "ok" and r["context_mode"] == "llm-select" and r["selector_used"] == 1 and r["selector_chunks"] >= 1 and r["retriever"] == "llm-select" for r in rows)' "missing successful llm-select ledger row"
  db_assert "${sid}" 'any(r["status"] == "provider_error" and r["context_mode"] == "always" and r["error"] != "" for r in rows)' "missing provider_error ledger row"

  echo "PASS: retrieval ledger HTTP integration validated at ${BASE_URL}"
}

main "$@"
