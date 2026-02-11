# aish (AI SHell)

aish is a CLI + daemon that augments zsh-in-tmux workflows with logging and agentic LLM execution.

## Architecture
```text
                         +----------------------+
                         |   LLM Providers      |
                         | (OpenAI-compatible)  |
                         +----------^-----------+
                                    |
                                    | completions
                                    |
+-------------------+     HTTP      |      +-------------------------------+
| Human Terminal(s) +--------------->------+ aishd (daemon/server)         |
| (zsh + aish hook) | <---SSE/JSON---------+-------------------------------+
+---------+---------+                      | sessions/agents/runs registry |
          |                                | subagents + flows             |
          | aish launch/llm/status         | tool runtime (shell/fs.*)     |
          v                                +-----+--------------------+----+
+-------------------+                            |                    |
| aish CLI client   |                            |                    |
+-------------------+                            |                    |
                                                 |                    |
                               +-----------------v----+      +--------v------------------+
                               | tmux session views   |      | Per-agent tmux sessions   |
                               | (main agents only)   |      | (main + subagents)        |
                               +----------------------+      +---------------------------+
                                                 |
                                                 |
                          +----------------------v-----------------------+
                          | Logging + Index                              |
                          | - events.jsonl / stdin.log / output.log      |
                          | - sessions.jsonl (store persistence)         |
                          | - logs.sqlite (searchable context index)     |
                          +----------------------------------------------+
```

## Features (current)
- **Daemon + CLI**: `aishd` (server) and `aish` (launcher/CLI).
- **LLM providers**: OpenAI-compatible APIs with profiles + model aliases (Z.ai, Together).
- **Tool runtime**: `shell`, `fs.read`, `fs.write`, `fs.list` with per-tool approval policy.
- **MCP tools**: `mcp.list_tools` and `mcp.web_search` via streamable-HTTP MCP servers.
- **Logging**: JSONL event logs, stdin command capture, PTY output capture.
- **Log index**: SQLite index (`logs.sqlite`) for queryable context across events/input/output/file edits.
- **Sessions + agents**: in-memory registry with JSONL persistence.
- **tmux**: per-agent tmux sessions, per-session main-agent view, startup reconcile, graceful cleanup on daemon shutdown, diagnostics endpoint.
- **Worktrees**: optional git worktrees per agent (inherit/new/none).
- **Subagents / swarms**: parallel or sequential subagents with aggregation.
- **Flows**: simple DAG execution with LLM/tool/aggregate nodes.
- **SSE streaming**: live events for runs and flows.
- **Runs registry**: list/get/cancel runs.
- **Context editing**: clear tool uses (keep last 3) + optional compaction.

## Quickstart
Install (one command):
```bash
./scripts/install.sh
```
This builds release binaries, installs `aish`/`aishd`/`ai` symlinks in a user bin
directory, updates `~/.zshrc` and `~/.bashrc` PATH blocks, and adds daemon
autostart to your active shell rc (`zsh` or `bash`).

Build:
```bash
cargo build
```

Run server (default config):
```bash
./target/debug/aishd
```

Or use the CLI:
```bash
./target/debug/aish serve
```

Health check:
```bash
curl -s http://127.0.0.1:5033/health
```
Adjust the port to match your config (defaults to `4096` if no config file exists).

CLI status and LLM:
```bash
./target/debug/aish status
./target/debug/aish llm "Say hello in one sentence."
```

When `AISH_SESSION_ID` is set (inside `aish launch` shell), `aish llm` sends
`session_id` with `context_mode: "diagnostic"` so prompts like "what did I do
wrong?" can use indexed session context from `logs.sqlite`.
Inspect selected context directly (no LLM call) with:
`GET /v1/logs/context/:session_id?q=what+did+i+do+wrong&max_lines=120`.
The response includes `selected_evidence` items with categories, scores, and reasons.
You can tune context packing with `max_chars` and local output expansion with
`output_window` (0-5).

Create a session and agent:
```bash
SESSION_ID=$(curl -s http://127.0.0.1:5033/v1/sessions \
  -H 'Content-Type: application/json' \
  -d '{"title":"demo"}' | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')

AGENT_ID=$(curl -s http://127.0.0.1:5033/v1/sessions/$SESSION_ID/agents \
  -H 'Content-Type: application/json' \
  -d '{}' | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')
```

Run an agent:
```bash
curl -s http://127.0.0.1:5033/v1/agents/$AGENT_ID/run \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"Say hello","approved":true,"tools":["shell"]}'
```

Stream an agent run (SSE):
```bash
curl -N http://127.0.0.1:5033/v1/agents/$AGENT_ID/run/stream \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"Say hello","approved":true,"tools":["shell"]}'
```

## CLI
Primary commands:
- `aish launch`: starts aishd if needed and launches a tmux-backed shell.
- `aish serve`: starts aishd only.
- `aish llm ...`: calls `/v1/completions` via aishd (reads stdin if no prompt).
- `aish status`: shows recent sessions and log locations.
- `aish ensure-daemon [--quiet]`: checks health and starts `aishd` when needed.

Shell hooks:
- `aish launch` writes hooks to `~/.config/aish/aish.zsh` and uses `ZDOTDIR=~/.config/aish/zdot`.
- Hooks log command start/end events and add an `llm` shell function that maps to `aish llm`.

## Configuration
Default config file:
```
~/.config/aish/aish.json
```

Example (Z.ai + Together):
```json
{
  "server": { "hostname": "127.0.0.1", "port": 5033 },
  "providers": {
    "openai_compat": {
      "base_url": "https://api.z.ai/api/coding/paas/v4",
      "model": "glm-4.7",
      "completions_path": "/chat/completions"
    },
    "openai_compat_profiles": {
      "together": {
        "base_url": "https://api.together.ai/v1",
        "model": "moonshotai/Kimi-K2.5",
        "completions_path": "/chat/completions",
        "api_key_env": "TOGETHER_API_KEY"
      }
    },
    "model_aliases": {
      "kimi-k2.5": { "provider": "together", "model": "moonshotai/Kimi-K2.5" },
      "qwen3-coder-next-fp8": { "provider": "together", "model": "Qwen/Qwen3-Coder-Next-FP8" }
    }
  },
  "tools": {
    "default_policy": "ask"
  }
}
```

Environment variables:
- `ZAI_API_KEY` (default OpenAI-compat provider)
- `TOGETHER_API_KEY` (Together profile)
- `AISH_OPENAI_COMPAT_BASE_URL`, `AISH_OPENAI_COMPAT_MODEL`, `AISH_OPENAI_COMPAT_COMPLETIONS_PATH`
- `AISH_OPENAI_COMPAT_API_KEY` (overrides `ZAI_API_KEY` when set; empty string falls back to `ZAI_API_KEY`)

MCP servers:
```json
{
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
```
If a header contains `your_api_key`, `${ZAI_API_KEY}`, or `$ZAI_API_KEY`,
`aishd` substitutes it from the `ZAI_API_KEY` environment variable.

Tool policies:
```json
{
  "tools": {
    "default_policy": "ask",
    "policies": {
      "shell": "ask",
      "fs": "allow"
    }
  }
}
```
Policies are `allow`, `ask`, or `deny`. More specific keys win (e.g., `fs.read` overrides `fs`).

## Endpoints (high-level)
- `GET /health`, `GET /version`
- `POST /v1/completions`
- `GET/POST /v1/sessions`
- `GET/PATCH /v1/sessions/:id`
- `GET/POST /v1/sessions/:id/agents`
- `POST /v1/agents/:id/run`
- `POST /v1/agents/:id/run/stream` (SSE)
- `POST /v1/agents/:id/subagents`
- `POST /v1/agents/:id/flow`
- `POST /v1/agents/:id/flow/stream` (SSE)
- `GET /v1/tools`
- `POST /v1/tools/:name/call`
- `GET /v1/runs`, `GET /v1/runs/:id`, `POST /v1/runs/:id/cancel`
- `GET /v1/diagnostics/tmux`
- `POST /v1/logs/ingest/:session_id`
- `GET /v1/logs/context/:session_id` (debug context bundle)

## Logs
Default log dir:
```
~/.local/share/aish/logs
```
Contains:
- `events.jsonl` (structured events)
- `output.log` (PTY output)
- `stdin.log` (command-level input)
- `sessions.jsonl` (session/agent/run registry)
- `logs.sqlite` (query index for events, command input, pane output, and file edits)

## Logging and Context Management
`aish` and `aishd` separate raw log capture from context selection:

- Raw capture:
`events.jsonl` stores structured events (tool runs, command lifecycle, etc.), `stdin.log` stores command input lines, and `output.log` stores terminal output.
- Persistent store:
`sessions.jsonl` stores session/agent/run state so daemon restarts can recover runtime metadata.
- Query index:
`logs.sqlite` is built from logs and used for context retrieval (`events`, `command_inputs`, `pane_output`, `file_edits`, plus session/agent/run tables).

Indexing behavior:
- On daemon startup, `aishd` initializes and backfills the SQLite index from existing logs.
- During runtime, events are appended incrementally and can also be re-ingested via:
`POST /v1/logs/ingest/:session_id`

Context bundle retrieval:
- Inspect selected context directly with:
`GET /v1/logs/context/:session_id?q=<query>&max_lines=120&max_chars=4500&output_window=1`
- The response includes:
`context_text`, `selected_evidence`, `failing_commands`, `failing_tools`, `related_commands`, `related_output`, and `recent_edits`.

How context is included in LLM calls:
- `aish llm` automatically sends `session_id` and `context_mode: "diagnostic"` when `AISH_SESSION_ID` is set (for example inside `aish launch`).
- For direct API usage, pass `session_id` and `context_mode` to `/v1/completions`.
- Diagnostic mode is optimized for questions like:
`"what did I do wrong?"`, `"why did this command fail?"`, `"what changed in this session?"`.

## Integration Tests
See `INTEGRATION_TESTS.md` and `scripts/run_integration_tests.sh`.
Run:
```bash
AISHD_URL=http://127.0.0.1:5033 scripts/run_integration_tests.sh
```
Reboot/restart resume workflow test:
```bash
scripts/test_resume_workflow.sh
```

## Usage Examples
See `EXAMPLES.md` for copy-paste workflows covering:
- `aish launch` / `aish serve` / `aish llm`
- session/agent/subagent/flow API usage
- restart/resume and troubleshooting patterns

## Relevance Eval Harness
Evaluate retrieval relevance for `/v1/logs/context` against predefined cases:
```bash
AISHD_URL=http://127.0.0.1:5033 python3 scripts/relevance_eval.py
```
Use a custom case file:
```bash
python3 scripts/relevance_eval.py --cases scripts/relevance_eval_cases.json
```
