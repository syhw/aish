# aish_codex.md

Design document for **aish (AI SHell)**.

## Summary
aish (AI SHell) is a zsh-compatible shell (or zsh extension) that runs inside tmux, logs all activity, and embeds an always-on agentic LLM. The user interacts like a normal shell, but can invoke the agent with `llm ...` to run an agent loop that can spawn subagents, manage worktrees, and run tools. aish is split into a server and client so sessions can be shared as webpages.

## Goals
- zsh-compatible (plugin + optional wrapper shell).
- tmux required at all times; each agent/subagent gets its own tmux session.
- full logging of stdin, stdout, stderr, and agent activity.
- agentic LLM loop with subagents, worktrees, tool execution.
- server/client split with web UI + shareable session pages.

## Requirements (Tightened)
### Authentication
- Default: bind to `127.0.0.1`, no auth.
- Optional: token auth for all HTTP endpoints (`Authorization: Bearer <token>`).
- Optional: local-only unix socket mode (no TCP) for single-user setups.

### Sharing Backends
- **Local share** (default): HTML page stored locally and served by aishd.
- **Remote share** (pluggable): upload HTML + assets to a remote target
  (e.g., S3-compatible bucket or hosted aish share service).
- Share policy is explicit: manual/auto/disabled.

### Tool Approval Policy
- Per-tool policy: `allow`, `ask`, or `deny`.
- Global default: `ask`.
- Policies apply to file writes, command execution, and network access.
- Audit trail: all approvals are logged with user, timestamp, and reason.

### Logging & Retention
- PTY logs + structured JSONL events stored locally by default.
- Configurable retention by size or days.
- Optional encryption-at-rest for logs (v2).

### Context Capture Requirements (Human + Agents)
- **Command-level input**: log shell commands and prompt-level inputs; raw keystroke capture is out of scope.
- **Output streams**: stdout/stderr captured separately where possible; preserve ordering.
- **Terminal state**: log window size changes, alternate-screen toggles, and session metadata needed for replay.
- **Command provenance**: for each command, record cwd, env snapshot (whitelist), start/end timestamps, exit code, and tty/session IDs.
- **Correlation IDs**: link human commands → agent turns → tool calls → agent shell output for traceability.
- **Redaction policy**: define how secrets/PII are detected, stored, and excluded from shared pages.

### Model Providers
- Providers are pluggable: OpenAI, Anthropic (SDK/API), Mistral, Gemini.
- Per-session model selection; per-request overrides allowed.

### Provider Support (Anthropic Included)
- aishd exposes a provider interface with streaming support.
- Anthropic support via official SDK/API with:
  - streaming deltas
  - tool-call metadata
  - token accounting
- Provider config is centralized in `aish.json` and can be overridden by env vars.

## Non-goals (v1)
- A full terminal emulator replacement.
- Running without tmux.
- Hidden/automatic cloud sharing without explicit user control.

## High-level Architecture

### Processes
1. **aish-launch** (bootstrapper)
   - Ensures tmux is running and launches/attaches to a aish tmux session.
   - Starts aish-server if not already running.
   - Execs zsh with aish plugin loaded.

2. **aish-client**
   - zsh plugin + optional TUI.
   - Captures user commands, wraps `llm` prompt, sends to aish-server.
   - Can attach to an existing server (like `aish attach http://host:port`).

3. **aish-server (aishd)**
   - Long-lived daemon that owns sessions, logs, agents, and tools.
   - Exposes HTTP API + OpenAPI spec for clients and integrations.
   - Serves web UI to browse sessions and share them.

4. **aish-web**
   - Web UI served by aishd.
   - Shows sessions, agent status, logs, and shared conversation pages.

### Architecture Diagram (Draft)
```
┌──────────────────────────────────────────────────────────────────┐
│                           aish-launch                              │
│  enforces tmux, starts aishd, execs zsh w/ aish plugin               │
└───────────────┬───────────────────────────────────────────────────┘
                │
                ▼
┌───────────────────────────┐       HTTP/SSE/Websocket      ┌──────────────┐
│       aish-client          │ <---------------------------> │    aishd      │
│  (zsh plugin + TUI)       │                               │ server/daemon│
└───────────────┬───────────┘                               └─────┬────────┘
                │                                                 │
                │                                                 │
                ▼                                                 ▼
      ┌────────────────┐                               ┌──────────────────┐
      │  tmux sessions │                               │ Tool Runtime     │
      │ per agent/sub  │                               │ (Python helpers) │
      └────────────────┘                               └──────────────────┘
                ▲                                                 │
                │                                                 ▼
                │                                      ┌──────────────────┐
                │                                      │ LLM Providers    │
                │                                      │ OpenAI/Anthropic │
                │                                      │ Mistral/Gemini   │
                │                                      └──────────────────┘
                │
                ▼
      ┌─────────────────┐
      │  aish-web UI     │
      │ session sharing │
      └─────────────────┘
```

### Core Components
- **Session Manager**: Creates and tracks sessions, messages, and metadata.
- **Agent Manager**: Runs LLM loops, subagents, task scheduling, and worktree mapping.
- **Tmux Manager**: Creates per-agent tmux sessions; allows `tmux -a` by name.
- **Tool Runtime**: Executes Python helpers (LLMVM-inspired) in a sandboxed worker.
- **Log Pipeline**: Writes raw PTY logs + structured JSONL events.

## Server/Client Split (OpenCode-like)
aish follows the same separation model as OpenCode:
- The TUI is a client that talks to a server.
- The server exposes an OpenAPI endpoint and can be run headless.
- Multiple clients can connect (TUI, zsh plugin, web UI, IDE extensions).
- A web interface can attach to the same server and display sessions.

## Session Sharing (Web Pages)
aish supports share modes:
- **manual** (default): sessions are shared only when the user calls `/share`.
- **auto**: automatically share new sessions.
- **disabled**: sharing is blocked.

Sharing produces a unique public URL and publishes an HTML view of the session.
For local-only use, the share URL is served by aishd. For remote sharing, a
pluggable "share backend" can push to a hosted service or S3-style bucket.
Shared views must respect redaction rules (secrets/PII), and should never expose
raw input streams unless explicitly enabled.

Commands:
- `/share` -> creates a share link
- `/unshare` -> removes the share link and deletes shared artifacts

## Data Model (Draft)
**Session**
- id, title, created_at, updated_at
- status (idle, running, error)
- share_state (manual/auto/disabled)
- tmux_session_name

**Message**
- id, session_id, role (user/assistant/tool/system)
- parts (text, code, tool_call, tool_result, image)
- timestamps, model info, token usage

**Agent**
- id, session_id, parent_agent_id (for subagents)
- model/provider, tools enabled
- worktree path

**Log**
- raw PTY stream (binary or text)
- resize + terminal state events
- structured events (JSONL)
- tool call logs
- command provenance (cwd/env/exit/timestamps)

## Logging Strategy
1. **PTY capture (output)**: tmux `pipe-pane` streams each pane's stdout/stderr to aishd.
2. **Terminal state**: record resize events, alternate-screen toggles, and session metadata needed to replay full terminal context.
3. **Shell hooks**: zsh `preexec`, `precmd`, `zshaddhistory` record commands, exit status, and timing.
4. **Structured events**: aishd writes JSONL for prompt/response/tool usage with correlation IDs across human/agent sessions.

All logs are timestamped and associated with session + agent IDs.
Retention is configurable by size/time.

## Tmux Integration
- aish requires tmux to run. If `$TMUX` is unset, aish-launch creates/attaches.
- Each agent/subagent runs in its own tmux session named like:
  `aish-<session-id>-<agent-id>`
- Users can attach any time: `tmux -a -t aish-...`
- aishd tracks session <-> tmux mapping.
- Both human and agent tmux sessions must emit the same logging signals (stdout/stderr + resize + metadata) to ensure full context capture.

## Tool Runtime (LLMVM-inspired)
Tools are Python helpers executed by aishd:
- Helpers are registered with metadata (name, args, description).
- LLM responses interleave natural language with code that calls helpers.
- aishd validates and executes helper code in a sandboxed Python worker.
- Results are streamed back into the session context.

## Agent Execution (Detailed)
When the user runs `llm ...`, aishd builds a per-request execution context:
1. Resolve toolset: global defaults -> session overrides -> request overrides.
2. Resolve prompt pack: global -> session -> request.
3. Spawn (or reuse) agent runtime in its own tmux session.
4. Call LLM with prompt + tool schemas + session context.
5. Execute tool calls via the Tool Runtime (approval policy enforced).
6. Stream results back to the session (SSE + logs).

**Important tmux rule**: the **main agent loop** always runs in a *separate* tmux
session from the human shell, created from the user's current context (cwd/env).
Subagents follow the same pattern.

### Tool Registry
- aishd maintains a registry of tool adapters:
  - `name`, `description`
  - `args_schema`
  - `policy` (`allow|ask|deny`)
  - `handler` (executes tool)

### OpenCode Tool Adapter
- aishd can expose an OpenCode-style tool set by wrapping equivalents:
  - `read_file`, `write_file`, `list_dir`, `grep`, `shell`, `patch`, `search`
  - Each tool maps into aishd's Tool Registry with schemas + handlers.

### Prompt Packs
- Prompt packs are versioned bundles containing:
  - `system` text
  - tool usage instructions
  - formatting rules
  - safety/policy text

- Prompt packs are selected at three layers:
  - **Global**: `~/.config/aish/prompts/default.toml`
  - **Session**: per-session override in aishd metadata
  - **Request**: `llm --prompt @name`

### Toolset Selection
- Toolsets are also layered:
  - **Global**: default enabled tools in config
  - **Session**: `aish session config set tools=@minimal`
  - **Request**: `llm --tools shell,fs,git`

### Tool Policy & Approval
- Default policy: `ask`
- Overrides per tool or namespace (`fs.write`, `network`)
- Approval decisions are logged with user + timestamp

## APIs (Draft)
aishd exposes:
- **OpenAPI spec** at `/doc`.
- **Sessions**: list/create/update/share/unshare.
- **Messages**: send prompt, stream response (SSE/websocket).
- **Agents**: list, create subagent, attach/detach.
- **Tools**: list helpers, execute helper.
- **Logs**: download or stream session logs.
- **TUI**: `/tui` endpoint to drive terminal clients programmatically.

## OpenAPI v0 Draft (Expanded)
Base URL: `http://127.0.0.1:4096`

### Auth
Header: `Authorization: Bearer <token>`

### Sessions
- `GET /v1/sessions`
  - response: `[{ id, title, status, created_at, updated_at }]`
- `POST /v1/sessions`
  - body: `{ title?, share_state? }`
  - response: `{ id, title, status }`
- `GET /v1/sessions/{id}`
  - response: session detail + latest metadata
- `PATCH /v1/sessions/{id}`
  - body: `{ title?, share_state? }`
- `POST /v1/sessions/{id}/share`
  - response: `{ share_url, shared_at }`
- `POST /v1/sessions/{id}/unshare`
  - response: `{ ok: true }`

### Messages
- `POST /v1/sessions/{id}/messages`
  - body:
    ```json
    {
      "role": "user",
      "content": "Write a hello world in C",
      "agent_id": null,
      "model": "anthropic/claude-3-5-sonnet",
      "tools": ["shell", "edit", "search"]
    }
    ```
  - response: `{ message_id }`
- `GET /v1/sessions/{id}/messages`
  - response: `[{ id, role, parts, created_at }]`

### Streaming (SSE)
- `GET /v1/sessions/{id}/events?since=<cursor>`
  - event types:
    - `message.start`
    - `message.delta`
    - `message.end`
    - `tool.start`
    - `tool.delta`
    - `tool.end`
    - `agent.status`
    - `log.append`
  - cursor is opaque; server returns `next_cursor`

### Agents
- `GET /v1/sessions/{id}/agents`
  - response: `[{ id, parent_agent_id, model, status }]`
- `POST /v1/sessions/{id}/agents`
  - body: `{ parent_agent_id?, model?, tools? }`
  - response: `{ id }`

### Tools
- `GET /v1/tools`
  - response: `[{ name, description, args_schema, policy }]`
- `POST /v1/tools/{name}/call`
  - body: `{ args, session_id, agent_id }`
  - response: `{ result, duration_ms }`

### Logs
- `GET /v1/sessions/{id}/logs/raw`
  - response: raw PTY stream (text/binary)
- `GET /v1/sessions/{id}/logs/events`
  - response: JSONL events

## Event Stream Format (SSE)
All SSE payloads are JSON with a stable envelope:
```json
{
  "type": "message.delta",
  "session_id": "sess_123",
  "agent_id": "agent_abc",
  "seq": 42,
  "ts": "2026-01-23T18:42:01Z",
  "payload": {
    "text": "partial output",
    "message_id": "msg_456"
  }
}
```

Example SSE frame:
```
event: message.delta
data: {"type":"message.delta","session_id":"sess_123","agent_id":"agent_abc","seq":42,"ts":"2026-01-23T18:42:01Z","payload":{"text":"partial output","message_id":"msg_456"}}

```

## Configuration
Config file: `aish.json`
```json
{
  "server": {
    "hostname": "127.0.0.1",
    "port": 4096,
    "password": "",
    "cors": []
  },
  "share": "manual",
  "tmux": {
    "session_prefix": "aish",
    "attach_on_start": true
  },
  "logging": {
    "dir": "~/.local/share/aish/logs",
    "retention_days": 30
  },
  "tools": {
    "python_helpers_dir": "~/.local/share/aish/helpers",
    "enabled": ["shell", "fs", "git", "search", "open_code"],
    "policy": {
      "shell": "ask",
      "fs.write": "ask",
      "network": "deny"
    }
  },
  "prompts": {
    "directory": "~/.config/aish/prompts",
    "default": "default.toml"
  }
}
```

## Security & Privacy
- Server is bound to localhost by default.
- Optional basic auth with password.
- Sharing is opt-in (manual).
- Logs and share exports should be stored locally by default.

## CLI Surface (Draft)
- `aish` -> launch shell + server
- `aish serve` -> start headless server
- `aish web` -> start web UI
- `aish attach <url>` -> attach client to server
- `aish session list` -> list sessions
- `aish session share <id>` -> share session
- `aish session unshare <id>` -> unshare session
- `aish session config get <id>` -> show session tool/prompt overrides
- `aish session config set <id> tools=@minimal prompt=@coding` -> set overrides

Inside shell:
- `llm <prompt>` -> run agentic loop in current session
- `llm --tools shell,fs,git` -> override toolset for a request
- `llm --prompt @coding` -> override prompt pack for a request
- `llm --new` -> create new session
- `llm --attach <id>` -> attach to session

## Resume Notes (Codex)
- Project root: `/Users/gab/aish_codex`
- Rust workspace: `Cargo.toml` at repo root, crates in `crates/`
- Binaries: `aish` (launcher) and `aishd` (daemon)
- Config module: `crates/aish_core/src/config.rs`
- Server stub exposes: `/health` and `/version`

### Quick commands
- Build: `cargo build`
- Run server: `cargo run -p aishd`
- Run launcher: `cargo run -p aish`
- Health check: `curl http://127.0.0.1:4096/health`

### Next steps (implementation)
1. Add a default `aish.json` generator and load from `~/.config/aish/aish.json` if present.
2. Implement minimal session store (SQLite or JSONL) and add `/v1/sessions` endpoints.
3. Wire tmux pipe-pane logging and zsh plugin hooks.
4. Implement tool registry + OpenCode adapter (shell/fs/grep/patch).
5. Add SSE event stream for message deltas.

## Milestones
1. **MVP**
   - tmux enforcement
   - zsh plugin (hooks + `llm` command)
   - aishd with HTTP API
   - log pipeline (PTY + JSONL)
2. **Web UI**
   - session list, detail view, share page rendering
3. **Agents & Tools**
   - subagents, worktrees, python helpers
4. **Sharing Backends**
   - local-only share
   - optional hosted share

## Open Questions
- Should aish replace the login shell or only run as a wrapper?
- What is the desired default model/provider?
- How strict should tool execution approvals be?
- Do we need MCP support in v1?
- Do we need alternate-screen replay or just raw output + metadata for v1?
