#!/usr/bin/env bash
set -euo pipefail

# Integration test for OpenAI-compatible provider routing through Rig.
# Covers:
# 1) chat/completions profile
# 2) raw completions profile (including absolute endpoint path support)
# 3) responses profile
# 4) anthropic messages profile
# 5) raw SSE streaming path

HOST="${AISHD_RIG_IT_HOST:-127.0.0.1}"
PORT="${AISHD_RIG_IT_PORT:-5193}"
MOCK_PORT="${AISHD_RIG_IT_MOCK_PORT:-5194}"
BASE_URL="http://${HOST}:${PORT}"
MOCK_URL="http://${HOST}:${MOCK_PORT}"
TMP_ROOT="${TMPDIR:-/tmp}/aish_rig_it_$$"
LOG_DIR="${TMP_ROOT}/logs"
CFG_PATH="${TMP_ROOT}/aish_rig_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
MOCK_LOG="${TMP_ROOT}/mock.log"
MOCK_REQ_LOG="${TMP_ROOT}/mock_requests.jsonl"
MOCK_SCRIPT="${TMP_ROOT}/mock_provider.py"
AISH_BIN="${AISHD_RIG_IT_BIN:-target/debug/aishd}"
KEEP_ARTIFACTS="${AISHD_RIG_IT_KEEP:-0}"
PREFIX="aish-rig-${RANDOM}-$$"
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

write_config() {
  mkdir -p "${TMP_ROOT}" "${LOG_DIR}"
  cat >"${CFG_PATH}" <<JSON
{
  "server": { "hostname": "${HOST}", "port": ${PORT} },
  "tmux": { "session_prefix": "${PREFIX}", "attach_on_start": false },
  "logging": { "dir": "${LOG_DIR}", "retention_days": 7 },
  "share": "manual",
  "providers": {
    "openai_compat": {
      "base_url": "${MOCK_URL}",
      "api_key": "mock-key",
      "model": "chat-model",
      "completions_path": "/v1/chat/completions"
    },
    "openai_compat_profiles": {
      "chat-local": {
        "base_url": "${MOCK_URL}",
        "api_key": "mock-key",
        "model": "chat-model",
        "completions_path": "/v1/chat/completions"
      },
      "raw-local": {
        "base_url": "${MOCK_URL}/v1",
        "api_key": "mock-key",
        "model": "raw-model",
        "completions_path": "/completions"
      },
      "raw-absolute-local": {
        "base_url": "${MOCK_URL}/ignored",
        "api_key": "mock-key",
        "model": "raw-model",
        "completions_path": "${MOCK_URL}/v1/completions"
      },
      "responses-local": {
        "base_url": "${MOCK_URL}/v1",
        "api_key": "mock-key",
        "model": "gpt-5.2-codex",
        "completions_path": "/responses"
      },
      "anthropic-local": {
        "base_url": "${MOCK_URL}",
        "api_key": "mock-key",
        "model": "claude-mock",
        "completions_path": "/v1/messages"
      }
    },
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

    def _send_json(self, status, obj):
        encoded = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def _send_sse(self, frames):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for frame in frames:
            if frame == "[DONE]":
                line = "data: [DONE]\n\n"
            else:
                line = "data: " + json.dumps(frame) + "\n\n"
            self.wfile.write(line.encode("utf-8"))
            self.wfile.flush()

    def do_POST(self):
        raw, body = self._read_json()
        write_req(
            {
                "path": self.path,
                "accept": self.headers.get("Accept", ""),
                "content_type": self.headers.get("Content-Type", ""),
                "body": body,
                "raw": raw,
            }
        )

        if self.path == "/v1/chat/completions":
            if body.get("stream") is True:
                self._send_sse(
                    [
                        {"choices": [{"index": 0, "delta": {"content": "CHAT_STREAM_OK"}}]},
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                        "[DONE]",
                    ]
                )
                return
            self._send_json(
                200,
                {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "chat-model",
                    "choices": [
                        {
                            "index": 0,
                            "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "CHAT_OK"},
                        }
                    ],
                    "usage": {"prompt_tokens": 5, "total_tokens": 7},
                },
            )
            return

        if self.path == "/v1/completions":
            if body.get("stream") is True:
                self._send_sse([{"choices": [{"text": "RAW_STREAM_OK"}]}, "[DONE]"])
                return
            self._send_json(
                200,
                {
                    "id": "cmpl-mock",
                    "object": "text_completion",
                    "choices": [{"index": 0, "text": "RAW_OK", "finish_reason": "stop"}],
                },
            )
            return

        if self.path == "/v1/responses":
            self._send_json(
                200,
                {
                    "id": "resp-mock",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "error": None,
                    "incomplete_details": None,
                    "instructions": None,
                    "max_output_tokens": None,
                    "model": "gpt-5.2-codex",
                    "usage": {
                        "input_tokens": 4,
                        "input_tokens_details": {"cached_tokens": 0},
                        "output_tokens": 2,
                        "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": 6,
                    },
                    "output": [
                        {
                            "type": "message",
                            "id": "msg-mock",
                            "role": "assistant",
                            "status": "completed",
                            "content": [{"type": "output_text", "text": "RESPONSES_OK"}],
                        }
                    ],
                    "tools": [],
                },
            )
            return

        if self.path == "/v1/messages":
            if "max_tokens" not in body:
                self._send_json(400, {"error": "max_tokens required"})
                return
            self._send_json(
                200,
                {
                    "id": "msg-mock",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-mock",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "ANTHROPIC_OK"}],
                    "usage": {"input_tokens": 8, "output_tokens": 2},
                },
            )
            return

        self._send_json(404, {"error": f"unknown path: {self.path}"})


if __name__ == "__main__":
    host = os.environ.get("MOCK_HOST", "127.0.0.1")
    port = int(os.environ.get("MOCK_PORT", "5194"))
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

call_completion_with_status() {
  local payload="$1"
  local out_file="$2"
  curl -sS -o "${out_file}" -w "%{http_code}" \
    "${BASE_URL}/v1/completions" \
    -H 'content-type: application/json' \
    -d "${payload}"
}

test_raw_streaming() {
  local stream_file="${TMP_ROOT}/raw_stream.sse"
  curl -sN --max-time 60 "${BASE_URL}/v1/completions/stream" \
    -H 'Content-Type: application/json' \
    -d '{"provider":"raw-local","model":"raw-model","prompt":"stream now"}' > "${stream_file}"
  python3 - <<'PY' "${stream_file}"
import json, sys
path = sys.argv[1]
event = "message"
deltas = []
final_chunks = []
ended = False
with open(path, "r", encoding="utf-8") as f:
  for line in f:
    line = line.strip()
    if not line:
        continue
    if line.startswith("event:"):
        event = line[6:].strip() or "message"
        continue
    if not line.startswith("data:"):
        continue
    payload = json.loads(line[5:].strip())
    delta = payload.get("delta")
    if isinstance(delta, str) and delta:
        deltas.append(delta)
    if isinstance(payload, dict):
        choices = payload.get("choices")
        if isinstance(choices, list) and choices:
            first = choices[0] if isinstance(choices[0], dict) else {}
            msg = first.get("message", {})
            content = msg.get("content") if isinstance(msg, dict) else None
            if isinstance(content, str) and content:
                final_chunks.append(content)
    if event in ("end", "done"):
        ended = True
        break
observed = "".join(deltas) + "".join(final_chunks)
if "RAW_STREAM_OK" not in observed:
    raise SystemExit(f"missing RAW_STREAM_OK in streamed output: deltas={deltas} finals={final_chunks}")
if not ended:
    raise SystemExit("stream did not end cleanly")
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
  health="$(curl -fsS "${BASE_URL}/health")"
  json_assert "${health}" 'data.get("status") == "ok"' "health must be ok"

  local chat_status chat_body
  chat_body="${TMP_ROOT}/chat.json"
  chat_status="$(call_completion_with_status '{"provider":"chat-local","model":"chat-model","messages":[{"role":"user","content":"hello chat"}]}' "${chat_body}")"
  [[ "${chat_status}" == "200" ]] || { echo "chat call failed with ${chat_status}" >&2; cat "${chat_body}" >&2; exit 1; }
  json_assert "$(cat "${chat_body}")" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "CHAT_OK"' "expected chat completion text"

  local raw_status raw_body
  raw_body="${TMP_ROOT}/raw.json"
  raw_status="$(call_completion_with_status '{"provider":"raw-local","model":"raw-model","messages":[{"role":"system","content":"Be terse."},{"role":"user","content":"hello raw"}],"max_tokens":12,"temperature":0.3,"top_p":0.8,"stop":["done"]}' "${raw_body}")"
  [[ "${raw_status}" == "200" ]] || { echo "raw call failed with ${raw_status}" >&2; cat "${raw_body}" >&2; exit 1; }
  json_assert "$(cat "${raw_body}")" 'data.get("choices", [{}])[0].get("text") == "RAW_OK"' "expected raw completion text"

  local raw_abs_status raw_abs_body
  raw_abs_body="${TMP_ROOT}/raw_abs.json"
  raw_abs_status="$(call_completion_with_status '{"provider":"raw-absolute-local","model":"raw-model","prompt":"absolute path"}' "${raw_abs_body}")"
  [[ "${raw_abs_status}" == "200" ]] || { echo "raw absolute call failed with ${raw_abs_status}" >&2; cat "${raw_abs_body}" >&2; exit 1; }
  json_assert "$(cat "${raw_abs_body}")" 'data.get("choices", [{}])[0].get("text") == "RAW_OK"' "expected raw absolute completion text"

  local responses_status responses_body
  responses_body="${TMP_ROOT}/responses.json"
  responses_status="$(call_completion_with_status '{"provider":"responses-local","model":"gpt-5.2-codex","messages":[{"role":"user","content":"hello responses"}]}' "${responses_body}")"
  [[ "${responses_status}" == "200" ]] || { echo "responses call failed with ${responses_status}" >&2; cat "${responses_body}" >&2; exit 1; }
  json_assert "$(cat "${responses_body}")" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "RESPONSES_OK"' "expected responses completion text"

  local anthropic_status anthropic_body
  anthropic_body="${TMP_ROOT}/anthropic.json"
  anthropic_status="$(call_completion_with_status '{"provider":"anthropic-local","model":"claude-mock","messages":[{"role":"user","content":"hello anthropic"}]}' "${anthropic_body}")"
  [[ "${anthropic_status}" == "200" ]] || { echo "anthropic call failed with ${anthropic_status}" >&2; cat "${anthropic_body}" >&2; exit 1; }
  json_assert "$(cat "${anthropic_body}")" 'data.get("choices", [{}])[0].get("message", {}).get("content") == "ANTHROPIC_OK"' "expected anthropic completion text"

  test_raw_streaming

  assert_mock_contains 'any(r.get("path") == "/v1/chat/completions" and isinstance(r.get("body", {}).get("messages"), list) for r in rows)' "expected chat/completions request with messages"
  assert_mock_contains 'any(r.get("path") == "/v1/completions" and "prompt" in r.get("body", {}) for r in rows)' "expected raw completions request with prompt"
  assert_mock_contains 'any(r.get("path") == "/v1/completions" and r.get("body", {}).get("stream") is True for r in rows)' "expected raw streaming request with stream=true"
  assert_mock_contains 'any(r.get("path") == "/v1/responses" for r in rows)' "expected responses endpoint request"
  assert_mock_contains 'any(r.get("path") == "/v1/messages" and int(r.get("body", {}).get("max_tokens", 0)) > 0 for r in rows)' "expected anthropic request with max_tokens"
  assert_mock_contains 'any(r.get("path") == "/v1/completions" and "Be terse." in str(r.get("body", {}).get("prompt", "")) and "hello raw" in str(r.get("body", {}).get("prompt", "")) for r in rows)' "expected raw prompt composition from messages"

  echo "PASS: OpenAI-compatible Rig integration validated at ${BASE_URL}"
}

main "$@"
