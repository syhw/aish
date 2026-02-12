# Context Engineering in `aishd`

This document explains how context selection currently works, what it should do in different situations, and how to test it yourself.

## Mental model

`aishd` is doing two related jobs:

1. Keep a durable index of what happened in shell/agent sessions.
2. Build a query-specific context bundle for LLM calls.

ASCII flow:

```text
shell + agents + tools + tmux
          |
          | raw logs per session (events.jsonl, stdin.log, output.log, stdout.log, stderr.log)
          v
   /v1/logs/ingest/:session_id   + startup backfill ingest_all
          |
          v
      logs.sqlite
      - events
      - command_inputs
      - pane_output
      - file_edits
      - context_artifacts
          |
          | build_relevant_context_bundle_with_options(session_id, query, options)
          v
  intent detect/override
          +
  incident synthesis
          +
  artifact refresh/load
          +
  scored evidence selection (caps + budget)
          v
  ContextBundle
  - intent
  - incidents
  - artifacts
  - selected_evidence (+ provenance)
  - context_text
          |
          +--> GET /v1/logs/context/:session_id (inspect bundle)
          |
          +--> POST /v1/completions with context_mode (inject context into prompt/system)
```

## What it should do by situation

The retriever chooses an intent and then applies intent-specific weighting/caps.

Intent modes:

- `debug`: prioritize failures, stderr/error lines, incident chains.
- `what-changed`: prioritize edits, changed files, related commands.
- `resume`: prioritize incidents + artifacts + recent edits for handoff continuity.
- `status`: balanced high-level snapshot.
- `general`: balanced fallback.
- `llm-select` (via `context_mode` on completions): run an LLM selector over full indexed history chunks, always carrying latest stdout/stderr/stdin, then inject selector-produced context into the final answer call.

Intent detection keywords are heuristic (`detect_query_intent`), with this order:

1. debug keywords (`error`, `fail`, `panic`, `debug`, `fix`, ...)
2. resume keywords (`resume`, `continue`, `where were we`, ...)
3. what-changed keywords (`what changed`, `which files`, `diff`, ...)
4. status keywords (`status`, `summary`, `overview`, ...)
5. otherwise general

Important behavior notes:

- Mixed queries can match the first category in that order.
- You can override with `intent=<...>` on `/v1/logs/context` or `context_intent` on `/v1/completions`.
- `context_mode=diagnostic` only injects context if the query looks diagnostic (`what went wrong`, `error`, `failed`, `traceback`, ...).
- `context_mode=always` always injects context when `session_id` is present.
- `context_mode=off` does not inject context.

## What gets logged/indexed

At ingest time, `aishd` indexes:

- `events.jsonl` -> `events`
- `stdin.log` -> `command_inputs`
- `output.log` -> `pane_output` stream `merged`
- `stdout.log` -> `pane_output` stream `stdout`
- `stderr.log` -> `pane_output` stream `stderr`
- `fs.write` tool starts in events -> `file_edits`

It also derives and stores `context_artifacts` (durable working memory) such as:

- `blocker` from command/tool failures
- `edit` from file edits
- `todo` from command text containing `todo`, `next`, `fixme`, `later`
- `decision` from command text containing `decide`, `chosen`, `use`, `switch to`

## What is returned in a context bundle

`GET /v1/logs/context/:session_id` returns a structured bundle with:

- `intent`
- `incidents[]` (title + summary lines + reasons + score)
- `artifacts[]`
- `selected_evidence[]`
- `context_text` (budgeted text block used for prompt injection)

When `explain=true`, it also returns:

- `explain.intent`
- `explain.selected_count`
- `explain.candidate_count`
- `explain.profile.category_caps`

Each selected evidence item includes provenance fields:

- `source_table`
- `source_id`
- `ts_ms`

## Follow-along examples

Assumptions:

- `aishd` is running.
- `AISHD_URL` points to it.
- log dir is `${AISH_LOG_DIR:-$HOME/.local/share/aish/logs}`.

### 1) Create a test session and seed logs

```bash
export AISHD_URL=${AISHD_URL:-http://127.0.0.1:5033}
export AISH_LOG_DIR=${AISH_LOG_DIR:-$HOME/.local/share/aish/logs}

SID=$(curl -s "$AISHD_URL/v1/sessions" \
  -H 'content-type: application/json' \
  -d '{"title":"context-engineering-walkthrough"}' \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')

echo "session: $SID"
mkdir -p "$AISH_LOG_DIR/$SID"

cat > "$AISH_LOG_DIR/$SID/events.jsonl" <<JSON
{"ts_ms":1000,"session_id":"$SID","kind":"command.start","data":{"cmd":"cargo test --workspace"}}
{"ts_ms":1001,"session_id":"$SID","kind":"command.end","data":{"exit":101}}
{"ts_ms":1002,"session_id":"$SID","kind":"tool.end","data":{"tool":"shell","ok":false,"error":"tests still failing"}}
{"ts_ms":1003,"session_id":"$SID","kind":"tool.start","data":{"tool":"fs.write","args":{"path":"src/lib.rs","append":false,"content":"fn add(){}"}}}
JSON

cat > "$AISH_LOG_DIR/$SID/stdin.log" <<'TXT'
cargo test --workspace
ai resume from where we left off
TXT

cat > "$AISH_LOG_DIR/$SID/stderr.log" <<'TXT'
error[E0425]: cannot find value `foo` in this scope
TXT
```

Ingest:

```bash
curl -s "$AISHD_URL/v1/logs/ingest/$SID" | python3 -m json.tool
```

### 2) Debug query (auto intent = `debug`)

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=what%20did%20i%20do%20wrong%20with%20cargo%20test" \
  | python3 -m json.tool
```

Expected mental picture:

- `intent` is `debug`.
- `incidents` include command failure + tool failure chain.
- `selected_evidence` contains `failure.command`, `output`, and often `incident`.
- `context_text` starts with session/intent/query and includes incident summaries.

### 3) Resume query (auto intent = `resume`)

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=resume%20from%20where%20we%20left%20off" \
  | python3 -m json.tool
```

Expected:

- `intent` is `resume`.
- Artifacts and incidents are both prominent.
- You should see edit memory like `Edited src/lib.rs (overwrite)`.

### 4) Force intent override

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=what%20failed&intent=status" \
  | python3 -m json.tool
```

Expected:

- `intent` is exactly `status` even though query text sounds diagnostic.

### 5) Turn on explain mode

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=why%20did%20it%20fail&explain=true" \
  | python3 -m json.tool
```

Expected:

- `explain` object present with candidate/selected counts.
- `profile.category_caps` visible.
- `selected_evidence` has provenance (`source_table`, `source_id`, `ts_ms`).

### 6) Enforce tiny context budget

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=why%20did%20it%20fail&max_chars=180" \
  | python3 -c 'import sys,json; d=json.load(sys.stdin); print(len(d.get("context_text",""))); print(d.get("context_text",""))'
```

Expected:

- `context_text` length is `<= max_chars`.
- It may include truncation marker text.

### 7) Disable artifacts when you want only raw/log-derived evidence

```bash
curl -s "$AISHD_URL/v1/logs/context/$SID?q=what%20went%20wrong&include_artifacts=false" \
  | python3 -m json.tool
```

Expected:

- `artifacts` is empty.
- Failures/incidents/output still drive retrieval.

## How this maps to `ai` calls

`POST /v1/completions` supports:

- `context_mode`: `off | diagnostic | always`
- `session_id`
- `context_intent`
- `context_max_lines`
- `context_max_chars`
- `context_output_window`
- `context_max_incidents`
- `context_include_artifacts`
- `context_explain`
- `context_selector_chunk_tokens` (for `context_mode=llm-select`, default `128000`)
- `context_selector_max_chunks` (default `16`)
- `context_selector_include_events` (default `true`)
- `context_selector_model` (optional separate model for selection passes)

When injection is active, `aishd` prepends context into a system message (or prompt wrapper).

Minimal pattern:

```json
{
  "session_id": "sess_xxx",
  "context_mode": "always",
  "context_intent": "resume",
  "messages": [
    {"role":"user","content":"continue from where we stopped"}
  ]
}
```

`llm-select` pattern:

```json
{
  "session_id": "sess_xxx",
  "context_mode": "llm-select",
  "context_selector_chunk_tokens": 128000,
  "context_selector_max_chunks": 8,
  "context_selector_include_events": true,
  "messages": [
    {"role":"user","content":"what did i do wrong and what should I do next?"}
  ]
}
```

## Quick alignment checklist

Use this checklist to compare behavior to your mental model:

- For failure questions, does it surface the right failing command and stderr line?
- For resume questions, does it include both incidents and durable artifacts?
- For “what changed”, do edited files dominate over noisy output?
- When intent override is set, does bundle intent follow override exactly?
- When budget is low, does context stay within limits and keep top evidence?
- In explain mode, are reasons/provenance sufficient to audit retrieval quality?

## Current limitations

- Intent detection is keyword heuristics (not semantic).
- Artifacts are heuristic extraction from existing logs/events.
- Incident stitching is local and lexical, not embedding/vector-based.
- Query-time context is per-session; cross-session retrieval is not implemented yet.

That is expected for the current stage before embedding + vector search + BM25.
