#!/usr/bin/env bash
set -euo pipefail

# End-to-end restart/resume workflow test.
# Simulates:
# 1) aishd launched once
# 2) multiple terminal sessions (session + main agent + subagent)
# 3) machine reboot (aishd dies + tmux sessions disappear)
# 4) aishd restart and session/agent resume validation
#
# This test runs against an isolated config, log dir, tmux prefix, and port.

HOST="${AISHD_RESTART_HOST:-127.0.0.1}"
PORT="${AISHD_RESTART_PORT:-5143}"
BASE_URL="http://${HOST}:${PORT}"
SESSION_COUNT="${AISHD_RESTART_SESSION_COUNT:-3}"
TMP_ROOT="${TMPDIR:-/tmp}/aish_resume_it_$$"
LOG_DIR="${TMP_ROOT}/logs"
CFG_PATH="${TMP_ROOT}/aish_restart_config.json"
DAEMON_LOG="${TMP_ROOT}/aishd.log"
PREFIX="aish-resume-${RANDOM}-$$"
DAEMON_PID=""
AISH_BIN="${AISHD_RESTART_BIN:-target/debug/aishd}"
KEEP_ARTIFACTS="${AISHD_RESTART_KEEP:-0}"

declare -a SESSION_IDS=()
declare -a SESSION_TMUX=()
declare -a MAIN_AGENT_IDS=()
declare -a MAIN_AGENT_TMUX=()
declare -a SUB_AGENT_IDS=()
declare -a SUB_AGENT_TMUX=()

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

json_get() {
  python3 - <<'PY' "$1" "$2"
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

json_assert() {
  local json="$1"
  local expr="$2"
  local msg="$3"
  python3 - <<'PY' "$json" "$expr" "$msg"
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

api_get() {
  curl -fsS "$1"
}

api_post() {
  local url="$1"
  local body="$2"
  curl -fsS "$url" -H 'Content-Type: application/json' -d "$body"
}

wait_for_health() {
  for _ in $(seq 1 80); do
    if curl -fsS "${BASE_URL}/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "aishd did not become healthy at ${BASE_URL}" >&2
  [[ -f "${DAEMON_LOG}" ]] && tail -n 120 "${DAEMON_LOG}" >&2 || true
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
    "openai_compat": null,
    "openai_compat_profiles": {},
    "model_aliases": {}
  },
  "tools": { "default_policy": "ask", "policies": {} }
}
JSON
}

start_daemon() {
  "${AISH_BIN}" --config "${CFG_PATH}" --bind "${HOST}:${PORT}" >"${DAEMON_LOG}" 2>&1 &
  DAEMON_PID="$!"
  wait_for_health
}

stop_daemon_graceful() {
  if [[ -n "${DAEMON_PID}" ]] && kill -0 "${DAEMON_PID}" >/dev/null 2>&1; then
    kill -TERM "${DAEMON_PID}" >/dev/null 2>&1 || true
    wait "${DAEMON_PID}" >/dev/null 2>&1 || true
  fi
  DAEMON_PID=""
}

stop_daemon_hard() {
  if [[ -n "${DAEMON_PID}" ]] && kill -0 "${DAEMON_PID}" >/dev/null 2>&1; then
    kill -9 "${DAEMON_PID}" >/dev/null 2>&1 || true
    wait "${DAEMON_PID}" >/dev/null 2>&1 || true
  fi
  DAEMON_PID=""
}

tmux_has_session() {
  tmux has-session -t "$1" >/dev/null 2>&1
}

tmux_kill_if_exists() {
  local name="$1"
  if tmux_has_session "$name"; then
    tmux kill-session -t "$name" >/dev/null 2>&1 || true
  fi
}

cleanup() {
  stop_daemon_graceful
  for name in "${SESSION_TMUX[@]:-}" "${MAIN_AGENT_TMUX[@]:-}" "${SUB_AGENT_TMUX[@]:-}"; do
    [[ -n "${name}" ]] && tmux_kill_if_exists "${name}"
  done
  if [[ "${KEEP_ARTIFACTS}" == "1" ]]; then
    echo "kept test artifacts at: ${TMP_ROOT}" >&2
  else
    rm -rf "${TMP_ROOT}"
  fi
}

validate_human_tmux_view() {
  local session_tmux="$1"
  local main_id="$2"
  local sub_id="$3"
  local windows
  windows="$(tmux list-windows -t "${session_tmux}" -F '#{window_name}')"
  if ! grep -qx "${main_id}" <<<"${windows}"; then
    echo "expected session tmux ${session_tmux} to include main window ${main_id}" >&2
    return 1
  fi
  if grep -qx "${sub_id}" <<<"${windows}"; then
    echo "unexpected subagent window ${sub_id} in session tmux ${session_tmux}" >&2
    return 1
  fi
}

create_sessions_and_agents() {
  local i session_json agent_main_json agent_sub_json
  for i in $(seq 1 "${SESSION_COUNT}"); do
    session_json="$(api_post "${BASE_URL}/v1/sessions" "{\"title\":\"terminal-${i}\"}")"
    agent_main_json="$(api_post "${BASE_URL}/v1/sessions/$(json_get "${session_json}" "id")/agents" '{}')"
    agent_sub_json="$(api_post "${BASE_URL}/v1/sessions/$(json_get "${session_json}" "id")/agents" "{\"parent_agent_id\":\"$(json_get "${agent_main_json}" "id")\"}")"

    SESSION_IDS+=("$(json_get "${session_json}" "id")")
    SESSION_TMUX+=("$(json_get "${session_json}" "tmux_session_name")")
    MAIN_AGENT_IDS+=("$(json_get "${agent_main_json}" "id")")
    MAIN_AGENT_TMUX+=("$(json_get "${agent_main_json}" "tmux_session_name")")
    SUB_AGENT_IDS+=("$(json_get "${agent_sub_json}" "id")")
    SUB_AGENT_TMUX+=("$(json_get "${agent_sub_json}" "tmux_session_name")")
  done
}

assert_tmux_exists_for_all() {
  local idx
  for idx in "${!SESSION_IDS[@]}"; do
    tmux_has_session "${SESSION_TMUX[$idx]}" || {
      echo "missing session tmux ${SESSION_TMUX[$idx]}" >&2
      return 1
    }
    tmux_has_session "${MAIN_AGENT_TMUX[$idx]}" || {
      echo "missing main agent tmux ${MAIN_AGENT_TMUX[$idx]}" >&2
      return 1
    }
    tmux_has_session "${SUB_AGENT_TMUX[$idx]}" || {
      echo "missing subagent tmux ${SUB_AGENT_TMUX[$idx]}" >&2
      return 1
    }
  done
}

assert_human_views_for_all() {
  local idx
  for idx in "${!SESSION_IDS[@]}"; do
    validate_human_tmux_view \
      "${SESSION_TMUX[$idx]}" \
      "${MAIN_AGENT_IDS[$idx]}" \
      "${SUB_AGENT_IDS[$idx]}"
  done
}

simulate_reboot_environment() {
  # Reboot-like behavior: daemon is abruptly gone and tmux sessions are lost.
  stop_daemon_hard
  local name
  for name in "${SESSION_TMUX[@]}" "${MAIN_AGENT_TMUX[@]}" "${SUB_AGENT_TMUX[@]}"; do
    tmux_kill_if_exists "${name}"
  done
}

assert_sessions_restored() {
  local sessions_json idx sid
  sessions_json="$(api_get "${BASE_URL}/v1/sessions")"
  for idx in "${!SESSION_IDS[@]}"; do
    sid="${SESSION_IDS[$idx]}"
    if ! json_assert "${sessions_json}" "'${sid}' in [s['id'] for s in data]" "expected restored sessions to include ${sid}"; then
      echo "sessions payload after restart: ${sessions_json}" >&2
      return 1
    fi
  done
}

assert_agents_resumable() {
  local idx flow_body flow_resp marker
  for idx in "${!SESSION_IDS[@]}"; do
    marker="resume-ok-${idx}"
    flow_body="$(cat <<JSON
{"nodes":[{"id":"n1","kind":"tool","tool":"shell","args":{"cmd":"echo ${marker}"}}],"edges":[],"tools":["shell"],"approved_tools":["shell"]}
JSON
)"
    flow_resp="$(api_post "${BASE_URL}/v1/agents/${MAIN_AGENT_IDS[$idx]}/flow" "${flow_body}")"
    json_assert "${flow_resp}" 'data.get("status") == "completed"' "expected resumed flow to complete"
    json_assert "${flow_resp}" "'${marker}' in str(data.get('outputs', {}).get('n1', ''))" "expected resumed flow output marker"
  done
}

main() {
  require_cmd cargo
  require_cmd curl
  require_cmd python3
  require_cmd tmux
  if [[ "${AISH_BIN}" == "target/debug/aishd" ]]; then
    cargo build -p aishd >/dev/null
  elif [[ ! -x "${AISH_BIN}" ]]; then
    echo "aishd binary not found/executable: ${AISH_BIN}" >&2
    exit 2
  fi

  trap cleanup EXIT

  write_config
  start_daemon
  create_sessions_and_agents

  assert_tmux_exists_for_all
  assert_human_views_for_all
  if [[ ! -s "${LOG_DIR}/sessions.jsonl" ]]; then
    echo "expected persisted store at ${LOG_DIR}/sessions.jsonl before reboot simulation" >&2
    exit 1
  fi

  simulate_reboot_environment

  start_daemon
  assert_sessions_restored
  assert_tmux_exists_for_all
  assert_human_views_for_all
  assert_agents_resumable

  echo "PASS: restart/resume workflow (${SESSION_COUNT} sessions) validated at ${BASE_URL}"
}

main "$@"
