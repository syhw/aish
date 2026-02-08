# Integration Tests

This document lists end-to-end tests for the current feature set. These tests assume `aishd` is running locally and your environment has the required API keys if you want LLM-backed tests.

## Prerequisites
- `tmux` installed and available on PATH.
- `curl`, `python`.
- `aishd` running (default `http://127.0.0.1:5033`).
- LLM tests require:
  - `ZAI_API_KEY` (default provider), or
  - `TOGETHER_API_KEY` (Together profile).
- Git worktree tests require a git repo path (`repo_path`).

## How to run
Automated script:
```
AISHD_URL=http://127.0.0.1:5033 scripts/run_integration_tests.sh
```

Relevance eval harness:
```
AISHD_URL=http://127.0.0.1:5033 python3 scripts/relevance_eval.py
```

Restart/resume workflow test (isolated daemon/config):
```
scripts/test_resume_workflow.sh
```

## Tests (manual description)

### 1) Health + Version
- `GET /health` returns `{"status":"ok"}`.
- `GET /version` returns name + version.

### 2) Tools list + tool calls (approvals)
- `GET /v1/tools` returns `shell`, `fs.read`, `fs.write`, `fs.list`.
- `POST /v1/tools/shell/call` with `"approved": true` runs `ls -la`.
- `fs.write` then `fs.read` and `fs.list` verify filesystem operations.

### 3) Sessions + Agents + tmux
- `POST /v1/sessions` creates a session.
- `POST /v1/sessions/:id/agents` creates an agent.
- `tmux list-sessions` includes `aish-<session-id>-<agent-id>`.

### 4) Worktree management
- Create agent with:
  - `worktree_mode: "new"`
  - `repo_path: "<git repo>"`
- Verify worktree path exists and `git worktree list` contains it.
- Create subagent with `worktree_mode: "inherit"` and verify the same path.

### 5) LLM completions
- `POST /v1/completions` returns a response when API key is set.

### 6) Agent run (non-stream)
- `POST /v1/agents/:id/run` returns `status: completed` and `run_id`.
- `GET /v1/runs/:id` returns the persisted run.

### 7) Agent run (SSE stream)
- `POST /v1/agents/:id/run/stream` returns SSE events:
  - `run.start`, `message.start/end`, `tool.start/end` (if tools), `run.end`.

### 8) Subagents + aggregation
- `POST /v1/agents/:id/subagents` with 2 tasks returns:
  - `results` array
  - `summary` (when `aggregate: true`)
- Ensure subagents cannot spawn subagents.

### 9) Flow execution (non-stream)
- `POST /v1/agents/:id/flow` with tool-only nodes:
  - tool node + tool node (template substitution)
- Verify `outputs` contains keys for each node and `run_id`.

Optional (LLM):
- Use an `aggregate` node to synthesize outputs (requires API key).

### 10) Flow execution (SSE stream)
- `POST /v1/agents/:id/flow/stream` emits:
  - `flow.node.start/end`
  - `tool.start/end`
  - `run.end`

### 11) Cancellation
- Start a long-running flow via `/flow/stream`.
- Call `POST /v1/runs/:id/cancel`.
- Expect `run.canceled` and run status updated to `canceled`.
- Note: cancellation is cooperative (checked between steps/nodes).

### 12) tmux diagnostics
- `GET /v1/diagnostics/tmux` returns `ok: true` and shows create/list/cleanup steps.

### 13) Restart + Resume Workflow (reboot simulation)
- Starts isolated `aishd` with temporary config/log dir and unique tmux prefix.
- Creates multiple sessions (simulating multiple terminals), each with:
  - one main agent
  - one subagent
- Verifies tmux layout:
  - per-session human tmux shows only main agent window(s)
  - subagents are not shown in human tmux window list
- Simulates reboot:
  - force-kills `aishd`
  - removes all test tmux sessions
- Restarts `aishd` with same config and verifies:
  - sessions are restored from persisted store
  - agent/subagent tmux sessions are recreated
  - per-session human tmux view is restored (main only)
  - resumed agents can execute new flow tasks successfully
