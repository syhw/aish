#!/usr/bin/env bash
set -euo pipefail

# Integration test runner for aishd.
# Prereqs:
# - aishd running
# - tmux installed
# - curl + python available
# - LLM tests require ZAI_API_KEY or TOGETHER_API_KEY

BASE_URL="${AISHD_URL:-http://127.0.0.1:5033}"
REPO_ROOT="${REPO_ROOT:-$(pwd)}"
TMP_DIR="${TMP_DIR:-/tmp/aish_it}"
RED=$'\033[31m'
GREEN=$'\033[32m'
RESET=$'\033[0m'
TEST_INDEX=0
FAILURES=0
FAILED_TESTS=()

# Ensure required commands are available before starting.
require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

# Non-fatal failure helper (lets the suite continue).
fail() {
  echo "$1" >&2
  return 1
}

# GET helper that fails on empty responses.
curl_json() {
  local url="$1"
  local resp
  resp="$(curl -sS "$url" || true)"
  if [[ -z "${resp}" ]]; then
    fail "empty response from ${url} (is aishd running at ${BASE_URL}?)"
    return 1
  fi
  echo "${resp}"
}

# POST helper that fails on empty responses.
curl_json_post() {
  local url="$1"
  local data="$2"
  local resp
  resp="$(curl -sS "$url" -H 'Content-Type: application/json' -d "${data}" || true)"
  if [[ -z "${resp}" ]]; then
    fail "empty response from ${url} (request data: ${data})"
    return 1
  fi
  echo "${resp}"
}

# Simple JSON path getter (dot-delimited, numeric indices supported).
json_get() {
  python - <<'PY' "$1" "$2"
import json, sys
data = json.loads(sys.argv[1])
key = sys.argv[2]
cur = data
for part in key.split("."):
    if part.isdigit():
        cur = cur[int(part)]
    else:
        cur = cur[part]
print(cur)
PY
}

# JSON assertion helper with a friendly failure message.
json_assert() {
  local json="$1"
  local expr="$2"
  local msg="$3"
  python - <<'PY' "$json" "$expr" "$msg"
import json, sys
data = json.loads(sys.argv[1])
expr = sys.argv[2]
msg = sys.argv[3]
try:
    ok = eval(expr, {"data": data})
except Exception as exc:
    raise SystemExit(f"{msg}: {exc}")
if not ok:
    raise SystemExit(msg)
PY
}

# Test runner that prints a single indented status line with ✓/✗.
run_test() {
  local name="$1"
  local desc="$2"
  local func="$3"
  TEST_INDEX=$((TEST_INDEX + 1))
  echo "[${TEST_INDEX}] ${name}"
  local output status tmpfile
  tmpfile="$(mktemp "${TMP_DIR}/aish_it.XXXXXX")"
  set +e
  $func >"${tmpfile}" 2>&1
  status=$?
  set -e
  output="$(cat "${tmpfile}")"
  rm -f "${tmpfile}"
  local msg="${output%%$'\n'*}"
  if [[ "${status}" -eq 0 ]]; then
    if [[ -z "${msg}" ]]; then
      msg="${desc}"
    fi
    printf "  %b✓%b %s\n" "${GREEN}" "${RESET}" "${msg}"
  else
    if [[ -z "${msg}" ]]; then
      msg="${desc}"
    fi
    printf "  %b✗%b %s\n" "${RED}" "${RESET}" "${msg}" >&2
    FAILURES=$((FAILURES + 1))
    FAILED_TESTS+=("${name}")
  fi
}

require_cmd curl
require_cmd python
require_cmd tmux
require_cmd git
mkdir -p "${TMP_DIR}"

# Test 1: /health + /version
test_health_version() {
  local health_resp version_resp
  health_resp="$(curl_json "${BASE_URL}/health")"
  json_assert "${health_resp}" 'data.get("status") == "ok"' "expected /health status ok"
  version_resp="$(curl_json "${BASE_URL}/version")"
  json_assert "${version_resp}" '"version" in data' "expected /version to include version"
}

# Test 2: tool registry + fs helpers
test_tools_and_fs() {
  local tools_json write_resp read_resp list_resp
  tools_json="$(curl_json "${BASE_URL}/v1/tools")"
  json_assert "${tools_json}" '"shell" in [t["name"] for t in data]' "expected tools list to include shell"
  json_assert "${tools_json}" '"fs.read" in [t["name"] for t in data]' "expected tools list to include fs.read"
  json_assert "${tools_json}" '"fs.write" in [t["name"] for t in data]' "expected tools list to include fs.write"
  json_assert "${tools_json}" '"fs.list" in [t["name"] for t in data]' "expected tools list to include fs.list"

  write_resp="$(curl_json_post "${BASE_URL}/v1/tools/fs.write/call" "{\"args\":{\"path\":\"${TMP_DIR}/hello.txt\",\"content\":\"hi\",\"create_dirs\":true},\"session_id\":\"it\",\"approved\":true}")"
  json_assert "${write_resp}" 'data.get("result", {}).get("ok") is True' "expected fs.write ok"

  read_resp="$(curl_json_post "${BASE_URL}/v1/tools/fs.read/call" "{\"args\":{\"path\":\"${TMP_DIR}/hello.txt\"},\"session_id\":\"it\",\"approved\":true}")"
  json_assert "${read_resp}" '"hi" in data.get("result", {}).get("content", "")' "expected fs.read to include content"

  list_resp="$(curl_json_post "${BASE_URL}/v1/tools/fs.list/call" "{\"args\":{\"path\":\"${TMP_DIR}\"},\"session_id\":\"it\",\"approved\":true}")"
  json_assert "${list_resp}" '"hello.txt" in [e["name"] for e in data.get("result", {}).get("entries", [])]' "expected fs.list to include hello.txt"
}

# Test 3: session + agent + tmux creation
test_sessions_agents_tmux() {
  local session_json agent_json tmux_name
  session_json="$(curl_json_post "${BASE_URL}/v1/sessions" '{"title":"it"}')"
  SESSION_ID="$(json_get "$session_json" "id")"
  agent_json="$(curl_json_post "${BASE_URL}/v1/sessions/${SESSION_ID}/agents" '{}')"
  AGENT_ID="$(json_get "$agent_json" "id")"
  tmux_name="$(json_get "$agent_json" "tmux_session_name")"
  if ! tmux list-sessions 2>/dev/null | grep -q "${tmux_name}"; then
    echo "expected tmux session ${tmux_name} to exist"
    return 1
  fi
}

# Test 4: git worktree management per agent
test_worktree_management() {
  if [[ -z "${SESSION_ID:-}" ]]; then
    echo "skipped: no SESSION_ID (sessions test failed)"
    return 0
  fi
  local agent_wt_json wt_path
  agent_wt_json="$(curl_json_post "${BASE_URL}/v1/sessions/${SESSION_ID}/agents" "{\"worktree_mode\":\"new\",\"repo_path\":\"${REPO_ROOT}\"}")"
  wt_path="$(json_get "$agent_wt_json" "worktree")"
  if [[ ! -d "${wt_path}" ]]; then
    echo "expected worktree directory ${wt_path}"
    return 1
  fi
  if ! git -C "${REPO_ROOT}" worktree list | grep -q "${wt_path}"; then
    echo "expected git worktree list to include ${wt_path}"
    return 1
  fi
}

# Test 5: LLM completion via /v1/completions
test_llm_completion() {
  if [[ -z "${ZAI_API_KEY:-}" && -z "${TOGETHER_API_KEY:-}" ]]; then
    echo "skipped: no API key for LLM completion"
    return 0
  fi
  local llm_resp
  llm_resp="$(curl_json_post "${BASE_URL}/v1/completions" '{"prompt":"Say hello","max_tokens":16}')"
  json_assert "${llm_resp}" '"choices" in data or "raw" in data' "expected LLM completion choices or raw output"
}

# Test 6: non-stream agent run + run registry
test_agent_run() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  local run_resp run_id run_status
  run_resp="$(curl_json_post "${BASE_URL}/v1/agents/${AGENT_ID}/run" '{"prompt":"Hello","max_steps":1}')"
  run_id="$(json_get "$run_resp" "run_id")"
  run_status="$(curl_json "${BASE_URL}/v1/runs/${run_id}")"
  json_assert "${run_status}" '"id" in data' "expected run status to include id"
}

# Test 7: SSE agent run
test_agent_run_sse() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  if [[ -z "${ZAI_API_KEY:-}" && -z "${TOGETHER_API_KEY:-}" ]]; then
    echo "skipped: no API key for SSE run"
    return 0
  fi
  curl -sN --max-time 60 "${BASE_URL}/v1/agents/${AGENT_ID}/run/stream" \
    -H 'Content-Type: application/json' \
    -d '{"prompt":"Say hi","max_steps":1}' | python -c 'import json,sys; seen=0
for line in sys.stdin:
    line=line.strip()
    if line.startswith("data:"):
        seen+=1
        payload=json.loads(line[5:].strip())
        if payload.get("type") in ("run.end","run.error","run.canceled"):
            break
if seen==0:
    raise SystemExit("no SSE data events received")'
}

# Test 8: subagents + aggregation
test_subagents() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  if [[ -z "${ZAI_API_KEY:-}" && -z "${TOGETHER_API_KEY:-}" ]]; then
    echo "skipped: no API key for subagents"
    return 0
  fi
  local sub_resp
  sub_resp="$(curl_json_post "${BASE_URL}/v1/agents/${AGENT_ID}/subagents" '{"mode":"parallel","aggregate":true,"approved_tools":["shell"],"tasks":[{"prompt":"List files","tools":["shell"]},{"prompt":"Count files","tools":["shell"]}]}')"
  json_assert "${sub_resp}" '"results" in data' "expected subagent results in response"
}

# Test 9: tool-only flow execution
test_flow_run() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  local flow_resp
  flow_resp="$(curl_json_post "${BASE_URL}/v1/agents/${AGENT_ID}/flow" '{"nodes":[{"id":"n1","kind":"tool","tool":"shell","args":{"cmd":"echo hello"}},{"id":"n2","kind":"tool","tool":"shell","args":{"cmd":"printf \"%s\" \"{{n1}}\" | wc -c"}}],"edges":[{"from":"n1","to":"n2"}],"tools":["shell"],"approved_tools":["shell"]}')"
  json_assert "${flow_resp}" '"outputs" in data' "expected flow outputs in response"
}

# Test 9b: flow execution with LLM aggregation
test_flow_run_llm() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  if [[ -z "${ZAI_API_KEY:-}" && -z "${TOGETHER_API_KEY:-}" ]]; then
    echo "skipped: no API key for LLM flow"
    return 0
  fi
  local flow_llm_resp
  flow_llm_resp="$(curl_json_post "${BASE_URL}/v1/agents/${AGENT_ID}/flow" '{"nodes":[{"id":"n1","kind":"tool","tool":"shell","args":{"cmd":"echo hello"}},{"id":"n2","kind":"aggregate","prompt":"Summarize {{n1}}","inputs":["n1"]}],"edges":[{"from":"n1","to":"n2"}],"tools":["shell"],"approved_tools":["shell"]}')"
  json_assert "${flow_llm_resp}" '"outputs" in data' "expected LLM flow outputs in response"
}

# Test 10: flow stream + cancellation
test_flow_stream_cancel() {
  if [[ -z "${AGENT_ID:-}" ]]; then
    echo "skipped: no AGENT_ID (agents test failed)"
    return 0
  fi
  local flow_body
  flow_body='{"nodes":[{"id":"n1","kind":"tool","tool":"shell","args":{"cmd":"sleep 2"}},{"id":"n2","kind":"tool","tool":"shell","args":{"cmd":"echo done"}}],"edges":[{"from":"n1","to":"n2"}],"tools":["shell"],"approved_tools":["shell"]}'
  curl -sN --max-time 60 "${BASE_URL}/v1/agents/${AGENT_ID}/flow/stream" \
    -H 'Content-Type: application/json' \
    -d "${flow_body}" | python -c 'import json,sys,subprocess
base=sys.argv[1]
run_id=None
cancel_sent=False
for line in sys.stdin:
    line=line.strip()
    if line.startswith("data:"):
        payload=json.loads(line[5:].strip())
        run_id=payload.get("run_id", run_id)
        if not cancel_sent and run_id:
            subprocess.call(["curl","-s","-X","POST",f"{base}/v1/runs/{run_id}/cancel"], stdout=subprocess.DEVNULL)
            cancel_sent=True
        if payload.get("type") in ("run.end","run.error","run.canceled"):
            break' "${BASE_URL}"
}

# Test 11: tmux diagnostics endpoint
test_tmux_diagnostics() {
  local tmux_diag
  tmux_diag="$(curl_json "${BASE_URL}/v1/diagnostics/tmux")"
  json_assert "${tmux_diag}" 'data.get("ok") in (True, False)' "expected tmux diagnostics ok boolean"
}

run_test "health + version" "verifies /health ok and /version includes version" test_health_version
run_test "tools list + shell/fs calls" "verifies tool list and fs helpers work" test_tools_and_fs
run_test "sessions + agents + tmux" "creates a session + agent and confirms tmux session exists" test_sessions_agents_tmux
run_test "worktree management" "creates a git worktree for a new agent" test_worktree_management
run_test "LLM completion" "requests a simple completion" test_llm_completion
run_test "agent run (non-stream)" "runs a simple agent step and inspects run status" test_agent_run
run_test "SSE agent run" "streams events until run completion" test_agent_run_sse
run_test "subagents + aggregation" "spawns parallel subagents and aggregates results" test_subagents
run_test "flow run (non-stream)" "executes a tool-only flow and checks outputs" test_flow_run
run_test "flow run with LLM aggregate" "executes a flow with LLM aggregation" test_flow_run_llm
run_test "flow stream + cancel" "streams flow events and attempts cancellation" test_flow_stream_cancel
run_test "tmux diagnostics" "checks tmux diagnostics endpoint" test_tmux_diagnostics

if [[ "${FAILURES}" -ne 0 ]]; then
  printf "%b✗%b %d test(s) failed: %s\n" "${RED}" "${RESET}" "${FAILURES}" "${FAILED_TESTS[*]}" >&2
  exit 1
fi
echo "integration tests complete"
