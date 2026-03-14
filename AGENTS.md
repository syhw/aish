# AGENTS.md

## Purpose
This repository is `aish` (AI SHell): a Rust workspace with:
- `aish` CLI (launcher/client)
- `aishd` daemon (HTTP API, agent runtime, tmux/log orchestration)
- `aish_core` shared config/types

Primary behavior lives in `crates/aish/src/main.rs` and `crates/aishd/src/main.rs`.

## Repo Map
- `crates/aish/`: CLI commands (`launch`, `serve`, `llm`, `status`, `ensure-daemon`, `log`).
  Important current path: `ai` prompt handling, workspace-context attachment, context-debug output, and stream/non-stream fallback all live in `crates/aish/src/main.rs`.
- `crates/aishd/`: server endpoints, runs/flows/subagents, tool execution, tmux lifecycle, SSE, log context retrieval.
  Important current path: provider routing, OpenAI-compatible fallback behavior, and `/v1/completions` + `/v1/completions/stream` live in `crates/aishd/src/main.rs`.
- `crates/aishd/src/log_index.rs`: SQLite-backed log ingest/index/retrieval logic.
- `crates/aish_core/`: config structs + loading/env overrides.
- `scripts/`: install + integration/smoke workflows.
- `README.md`: user-facing quickstart, features, endpoint overview.
- `CONTEXT_ENGINEERING.md`: deep dive on retrieval/context behavior.
- `INTEGRATION_TESTS.md`: end-to-end test matrix.
- `TODO.codex`: current implementation priorities.

## Fast Start (Dev)
1. Build:
```bash
cargo build --workspace
```
2. Unit tests:
```bash
cargo test --workspace
```
3. Run daemon on local port:
```bash
cargo run -p aishd -- --bind 127.0.0.1:5033
```
4. Health check:
```bash
curl -fsS http://127.0.0.1:5033/health
```

## Integration Scripts
- `scripts/run_integration_tests.sh`: broad HTTP/tmux/tools integration sweep.
- `scripts/test_resume_workflow.sh`: restart/recovery + tmux/session restore.
- `scripts/test_context_engineering_workflow.sh`: deterministic + `llm-select` context pipeline checks.
- `scripts/test_retrieval_ledger_http.sh`: retrieval-ledger persistence checks.
- `scripts/test_openai_compat_rig_integration.sh`: isolated mock-provider coverage for provider routing plus `/v1/completions/stream`.
- `scripts/test_mcp_mock_integration.sh`: mock MCP server integration.
- `scripts/test_mcp_web_search.sh`: live MCP web-search smoke test (requires `ZAI_API_KEY`).

Use script-specific env vars from script headers when isolating ports/log dirs.

Recommended validation slices:
- CLI/context changes in `crates/aish/src/main.rs`:
  run `cargo test -p aish` and `cargo test --workspace`.
- Retrieval or completion-path changes in `crates/aishd/src/main.rs` / `crates/aishd/src/log_index.rs`:
  run `cargo test -p aishd`, `scripts/test_retrieval_ledger_http.sh`, and `scripts/test_context_engineering_workflow.sh`.
- Provider routing or streaming changes:
  run `scripts/test_openai_compat_rig_integration.sh`.

## Editing Guidance
- Keep config schema compatibility in `aish_core::config` (JSON keys + env overrides).
- If adding/changing API behavior in `aishd`, update:
  - route handlers in `crates/aishd/src/main.rs`
  - retrieval/index logic in `crates/aishd/src/log_index.rs` (if context/log related)
  - docs in `README.md` or `CONTEXT_ENGINEERING.md` when behavior changes
  - integration scripts/tests for externally visible changes
- Prefer deterministic tests; use mock provider/MCP paths before live network-dependent tests.
- For `ai` CLI work, preserve the direct-prompt UX:
  plain `ai "..."` should still normalize to `llm` subcommand behavior.
- If changing workspace-context behavior, update both:
  - CLI tests in `crates/aish/src/main.rs`
  - docs in `README.md`, `EXAMPLES.md`, and `TODO.codex` if checklist state changed
- If changing provider response parsing, test against the isolated mock provider before assuming a real-provider issue.

## Operational Notes
- Defaults:
  - server bind: `127.0.0.1:4096` (unless overridden)
  - logs dir: `~/.local/share/aish/logs`
  - config file: `~/.config/aish/aish.json`
- `aishd` maintains:
  - `sessions.jsonl` for session store persistence
  - `logs.sqlite` for indexed retrieval context
- tmux is a core runtime dependency for agent/session behavior.
- In this workspace, `git add` / `git commit` may require sandbox escalation because `.git/index.lock` creation can be blocked.

## Current Behavior Worth Knowing
- `ai` supports:
  - `--with-workspace-context`
  - `--no-workspace-context`
  - `--context-debug`
  - `--no-stream`
- Workspace context is attached heuristically for repo/folder-oriented prompts and now includes recent commits with deterministic caps.
- `--context-debug` prints workspace source usage and, when `AISH_SESSION_ID` is set, a `/v1/logs/context` preview summary.
- `aishd` has a narrow raw-HTTP fallback for OpenAI-compatible `chat/completions` when Rig rejects an otherwise valid provider response with a strict JSON parse error.

## Current Priorities
From `TODO.codex`:
- Streaming fallback behavior tests
- Deeper workspace/log-context composition and ledger visibility
- Relevance harness expansion for repo-summary and mixed-query cases

When uncertain, optimize for retrieval correctness, reproducible tests, and API compatibility.
