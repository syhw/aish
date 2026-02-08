# aish Examples

Practical, copy-paste examples for `aish` and `aishd`.

## 1) Start `aishd` and verify it is reachable

```bash
cargo run -p aishd -- --bind 127.0.0.1:5033
```

In another terminal:

```bash
export AISHD_URL="http://127.0.0.1:5033"
curl -fsS "$AISHD_URL/health"
curl -fsS "$AISHD_URL/version"
```

## 2) Use `aish` CLI directly

Start daemon only:

```bash
./target/debug/aish serve
```

Launch tmux-backed shell:

```bash
./target/debug/aish launch
```

Ask the model:

```bash
./target/debug/aish llm "Summarize this repository in 5 bullets."
```

Pipe prompt from stdin:

```bash
echo "What did I do wrong in this session?" | ./target/debug/aish llm
```

Show status:

```bash
./target/debug/aish status
```

## 3) Create a session and a lead agent (HTTP API)

```bash
export AISHD_URL="http://127.0.0.1:5033"

SESSION_JSON=$(curl -fsS "$AISHD_URL/v1/sessions" \
  -H 'content-type: application/json' \
  -d '{"title":"demo"}')
SID=$(python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' <<<"$SESSION_JSON")

AGENT_JSON=$(curl -fsS "$AISHD_URL/v1/sessions/$SID/agents" \
  -H 'content-type: application/json' \
  -d '{}')
LEAD=$(python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' <<<"$AGENT_JSON")

echo "SID=$SID"
echo "LEAD=$LEAD"
```

## 4) Run a simple agent task

```bash
curl -fsS "$AISHD_URL/v1/agents/$LEAD/run" \
  -H 'content-type: application/json' \
  -d '{"prompt":"Say hello in one sentence.","max_steps":1}' \
  | python3 -m json.tool
```

## 5) Stream an agent run (SSE)

```bash
curl -N "$AISHD_URL/v1/agents/$LEAD/run/stream" \
  -H 'content-type: application/json' \
  -d '{"prompt":"List current directory with shell tool","tools":["shell"],"approved_tools":["shell"],"approved":true}'
```

## 6) Three subagents: design, implement, review

Important: set `"worktree_mode":"none"` unless your lead agent has a worktree.

```bash
WORKDIR=/tmp/aish_three_agent_demo
mkdir -p "$WORKDIR"

cat >/tmp/aish_3agent_payload.json <<JSON
{
  "mode": "sequential",
  "aggregate": true,
  "approved_tools": ["shell", "fs.read", "fs.write", "fs.list"],
  "tasks": [
    {
      "worktree_mode": "none",
      "prompt": "Design a tiny Python utility in ${WORKDIR}. Create DESIGN.md with goals, API, and test plan. Keep it concise.",
      "tools": ["fs.write", "fs.read", "fs.list"]
    },
    {
      "worktree_mode": "none",
      "prompt": "Implement the design in ${WORKDIR}: create calc.py and test_calc.py for add_positive(nums) that sums only positive ints. Use unittest.",
      "tools": ["fs.write", "fs.read", "fs.list", "shell"]
    },
    {
      "worktree_mode": "none",
      "prompt": "Review the implementation in ${WORKDIR}. Run python3 -m unittest -q. Create REVIEW.md with findings and fixes if needed.",
      "tools": ["fs.write", "fs.read", "shell", "fs.list"]
    }
  ]
}
JSON

RESP=$(curl -fsS "$AISHD_URL/v1/agents/$LEAD/subagents" \
  -H 'content-type: application/json' \
  -d @/tmp/aish_3agent_payload.json)

echo "$RESP" | python3 -m json.tool
ls -la "$WORKDIR"
```

## 7) Run a flow (DAG) on one agent

Note: `flow` does not spawn new agents. It runs flow nodes under one agent.

```bash
curl -fsS "$AISHD_URL/v1/agents/$LEAD/flow" \
  -H 'content-type: application/json' \
  -d '{
    "nodes": [
      {"id":"n1","kind":"tool","tool":"shell","args":{"cmd":"echo hello"}},
      {"id":"n2","kind":"tool","tool":"shell","args":{"cmd":"printf \"%s\" \"{{n1}}\" | wc -c"}}
    ],
    "edges": [{"from":"n1","to":"n2"}],
    "tools": ["shell"],
    "approved_tools": ["shell"]
  }' \
  | python3 -m json.tool
```

## 8) List runs and cancel one

```bash
curl -fsS "$AISHD_URL/v1/runs" | python3 -m json.tool
RUN_ID="<run_id_here>"
curl -fsS -X POST "$AISHD_URL/v1/runs/$RUN_ID/cancel" | python3 -m json.tool
```

## 9) Query indexed context for a session

```bash
curl -fsS "$AISHD_URL/v1/logs/context/$SID?q=what%20did%20i%20do%20wrong&max_lines=120&max_chars=4500" \
  | python3 -m json.tool
```

## 10) Relevant context retrieval (end-to-end)

This example intentionally creates failures, then retrieves relevant context and
uses it in an LLM call.

```bash
export AISHD_URL="http://127.0.0.1:5033"

# Create isolated session + agent
SESSION_JSON=$(curl -fsS "$AISHD_URL/v1/sessions" \
  -H 'content-type: application/json' \
  -d '{"title":"context-debug-demo"}')
SID=$(python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' <<<"$SESSION_JSON")
AGENT_JSON=$(curl -fsS "$AISHD_URL/v1/sessions/$SID/agents" \
  -H 'content-type: application/json' \
  -d '{}')
AID=$(python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])' <<<"$AGENT_JSON")

# Produce a failing run (shell command exits non-zero)
curl -fsS "$AISHD_URL/v1/agents/$AID/run" \
  -H 'content-type: application/json' \
  -d '{"prompt":"Run tool shell with cmd `ls /definitely-not-real` and report the result.","tools":["shell"],"approved_tools":["shell"],"approved":true}' \
  | python3 -m json.tool

# Ensure log index is updated for this session
curl -fsS -X POST "$AISHD_URL/v1/logs/ingest/$SID" | python3 -m json.tool

# Inspect selected evidence/context bundle directly
curl -fsS "$AISHD_URL/v1/logs/context/$SID?q=why%20did%20my%20command%20fail&max_lines=120&max_chars=4500&output_window=1" \
  | python3 -m json.tool

# Ask for analysis with diagnostic context mode
curl -fsS "$AISHD_URL/v1/completions" \
  -H 'content-type: application/json' \
  -d "{\"prompt\":\"What did I do wrong in this session?\",\"session_id\":\"$SID\",\"context_mode\":\"diagnostic\"}" \
  | python3 -m json.tool
```

## 11) Useful diagnostics

tmux diagnostics:

```bash
curl -fsS "$AISHD_URL/v1/diagnostics/tmux" | python3 -m json.tool
```

Integration tests:

```bash
AISHD_URL="$AISHD_URL" scripts/run_integration_tests.sh
```

Restart/resume workflow test:

```bash
scripts/test_resume_workflow.sh
```

## Troubleshooting

### `JSONDecodeError` after `curl | python3 -c ...`

Cause: `curl` returned empty output or non-JSON (often wrong URL/port).

Fix:

```bash
export AISHD_URL="http://127.0.0.1:5033"
curl -i "$AISHD_URL/health"
```

Use `curl -fsS` in scripts so failures are visible.

### `heredoc>` prompt does not end

Cause: heredoc terminator (for example `PY`) is not at column 1.

Fix: make the terminator line exactly `PY` with no leading spaces.

### Subagents show `failed to create agent`

Common cause: inherited worktree requested but parent has none.

Fix: set `"worktree_mode":"none"` for each task, or create lead agent with a worktree.
