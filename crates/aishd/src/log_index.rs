use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone, Copy)]
pub struct IngestStats {
    pub events_inserted: u64,
    pub stdin_lines_inserted: u64,
    pub output_lines_inserted: u64,
    pub file_edits_inserted: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContextBundle {
    pub session_id: String,
    pub query: String,
    pub query_tokens: Vec<String>,
    pub failing_commands: Vec<String>,
    pub failing_tools: Vec<String>,
    pub related_commands: Vec<String>,
    pub related_output: Vec<String>,
    pub recent_edits: Vec<String>,
    pub selected_evidence: Vec<EvidenceItem>,
    pub context_text: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct EvidenceItem {
    pub category: String,
    pub text: String,
    pub score: f32,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct BuildContextOptions {
    pub max_lines: usize,
    pub max_chars: usize,
    pub output_window: usize,
    pub max_evidence: usize,
}

impl Default for BuildContextOptions {
    fn default() -> Self {
        Self {
            max_lines: 120,
            max_chars: 4500,
            output_window: 1,
            max_evidence: 36,
        }
    }
}

impl IngestStats {
    pub fn add(&mut self, other: IngestStats) {
        self.events_inserted += other.events_inserted;
        self.stdin_lines_inserted += other.stdin_lines_inserted;
        self.output_lines_inserted += other.output_lines_inserted;
        self.file_edits_inserted += other.file_edits_inserted;
    }
}

pub fn init(db_path: &Path) -> Result<(), String> {
    let conn = open_conn(db_path)?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            status TEXT NOT NULL,
            share_state TEXT NOT NULL,
            tmux_session_name TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_seen_ms INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS agents (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            parent_agent_id TEXT,
            model TEXT,
            status TEXT NOT NULL,
            tmux_session_name TEXT,
            worktree TEXT,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            ended_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_ms INTEGER,
            session_id TEXT,
            agent_id TEXT,
            actor TEXT,
            kind TEXT NOT NULL,
            data_json TEXT NOT NULL,
            source TEXT,
            source_line INTEGER,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(source, source_line)
        );
        CREATE INDEX IF NOT EXISTS idx_events_session_ts ON events(session_id, ts_ms);
        CREATE INDEX IF NOT EXISTS idx_events_kind_ts ON events(kind, ts_ms);

        CREATE TABLE IF NOT EXISTS command_inputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            ts_ms INTEGER,
            line_text TEXT NOT NULL,
            source TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(source, source_line)
        );
        CREATE INDEX IF NOT EXISTS idx_command_inputs_session ON command_inputs(session_id, id);

        CREATE TABLE IF NOT EXISTS pane_output (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            ts_ms INTEGER,
            stream TEXT NOT NULL,
            line_text TEXT NOT NULL,
            source TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(source, source_line)
        );
        CREATE INDEX IF NOT EXISTS idx_pane_output_session ON pane_output(session_id, id);

        CREATE TABLE IF NOT EXISTS file_edits (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_ms INTEGER,
            session_id TEXT,
            agent_id TEXT,
            tool TEXT NOT NULL,
            path TEXT,
            write_mode TEXT,
            bytes INTEGER,
            source TEXT,
            source_line INTEGER,
            data_json TEXT NOT NULL,
            UNIQUE(source, source_line)
        );
        CREATE INDEX IF NOT EXISTS idx_file_edits_session_ts ON file_edits(session_id, ts_ms);
        ",
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn ingest_all(log_root: &Path, db_path: &Path) -> Result<IngestStats, String> {
    let mut total = IngestStats::default();
    let entries = fs::read_dir(log_root).map_err(|err| err.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "worktrees" {
            continue;
        }
        let stats = ingest_session_from_dir(db_path, &name, &path)?;
        total.add(stats);
    }
    Ok(total)
}

pub fn ingest_session(
    log_root: &Path,
    db_path: &Path,
    session_id: &str,
) -> Result<IngestStats, String> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err("invalid session_id".to_string());
    }
    let dir = log_root.join(session_id);
    ingest_session_from_dir(db_path, session_id, &dir)
}

pub fn append_event(db_path: &Path, event: &Value) -> Result<IngestStats, String> {
    let conn = open_conn(db_path)?;
    insert_event(&conn, event, None, None, None)
}

pub fn upsert_session(
    db_path: &Path,
    id: &str,
    title: &str,
    status: &str,
    share_state: &str,
    tmux_session_name: Option<&str>,
    created_at: u128,
    updated_at: u128,
) -> Result<(), String> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "
        INSERT INTO sessions (
            id, title, status, share_state, tmux_session_name, created_at, updated_at, last_seen_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            status = excluded.status,
            share_state = excluded.share_state,
            tmux_session_name = excluded.tmux_session_name,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            last_seen_ms = excluded.last_seen_ms
        ",
        params![
            id,
            title,
            status,
            share_state,
            tmux_session_name,
            to_i64_u128(created_at),
            to_i64_u128(updated_at),
            to_i64_u128(updated_at),
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn upsert_agent(
    db_path: &Path,
    id: &str,
    session_id: &str,
    parent_agent_id: Option<&str>,
    model: Option<&str>,
    status: &str,
    tmux_session_name: Option<&str>,
    worktree: Option<&str>,
    updated_at: u128,
) -> Result<(), String> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "
        INSERT INTO agents (
            id, session_id, parent_agent_id, model, status, tmux_session_name, worktree, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(id) DO UPDATE SET
            session_id = excluded.session_id,
            parent_agent_id = excluded.parent_agent_id,
            model = excluded.model,
            status = excluded.status,
            tmux_session_name = excluded.tmux_session_name,
            worktree = excluded.worktree,
            updated_at = excluded.updated_at
        ",
        params![
            id,
            session_id,
            parent_agent_id,
            model,
            status,
            tmux_session_name,
            worktree,
            to_i64_u128(updated_at),
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn upsert_run(
    db_path: &Path,
    id: &str,
    session_id: &str,
    agent_id: &str,
    mode: &str,
    status: &str,
    started_at: u128,
    ended_at: Option<u128>,
) -> Result<(), String> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "
        INSERT INTO runs (
            id, session_id, agent_id, mode, status, started_at, ended_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(id) DO UPDATE SET
            session_id = excluded.session_id,
            agent_id = excluded.agent_id,
            mode = excluded.mode,
            status = excluded.status,
            started_at = excluded.started_at,
            ended_at = excluded.ended_at
        ",
        params![
            id,
            session_id,
            agent_id,
            mode,
            status,
            to_i64_u128(started_at),
            ended_at.map(to_i64_u128),
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn build_relevant_context_bundle_with_options(
    db_path: &Path,
    session_id: &str,
    query: &str,
    options: BuildContextOptions,
) -> Result<ContextBundle, String> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err("invalid session_id".to_string());
    }

    let conn = open_conn(db_path)?;
    let tokens = query_tokens(query);

    let commands = load_command_lines(&conn, session_id, 250)?;
    let outputs = load_output_lines(&conn, session_id, 500)?;
    let events = load_events(&conn, session_id, 500)?;
    let edits = load_file_edits(&conn, session_id, 60)?;

    let command_failures = correlate_command_failures(&events, 24);
    let tool_failures = correlate_tool_failures(&events, 24);

    let mut candidates = Vec::new();
    candidates.extend(score_command_failures(&command_failures, &tokens));
    candidates.extend(score_tool_failures(&tool_failures, &tokens));
    candidates.extend(score_commands(&commands, &tokens, 40));
    candidates.extend(score_output(&outputs, &tokens, 80));
    candidates.extend(score_edits(&edits, &tokens, 30));

    let selected = select_candidates(candidates, options.max_evidence.max(8));
    let failing_commands = selected_lines_by_category(&selected, "failure.command", 12);
    let failing_tools = selected_lines_by_category(&selected, "failure.tool", 12);
    let related_commands = selected_lines_by_category(&selected, "command", 15);
    let selected_output_ids = selected_output_ids(&selected, 24);
    let related_output = if selected_output_ids.is_empty() {
        selected_lines_by_category(&selected, "output", 30)
    } else {
        expand_output_windows(
            &outputs,
            &selected_output_ids,
            options.output_window.min(5),
            30,
        )
    };
    let recent_edits = selected_lines_by_category(&selected, "edit", 12);
    let selected_evidence = selected
        .iter()
        .map(|candidate| EvidenceItem {
            category: candidate.category.to_string(),
            text: candidate.text.clone(),
            score: ((candidate.score * 100.0).round() / 100.0),
            reasons: candidate.reasons.clone(),
        })
        .collect::<Vec<_>>();

    let mut bundle = ContextBundle {
        session_id: session_id.to_string(),
        query: query.to_string(),
        query_tokens: tokens,
        failing_commands,
        failing_tools,
        related_commands,
        related_output,
        recent_edits,
        selected_evidence,
        context_text: String::new(),
    };
    bundle.context_text = render_context_text(&bundle, options);
    Ok(bundle)
}

fn render_context_text(bundle: &ContextBundle, options: BuildContextOptions) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Session: {}", bundle.session_id));
    lines.push(format!("Query: {}", truncate_line(&bundle.query, 220)));

    if !bundle.failing_commands.is_empty() {
        lines.push("Recent command failures:".to_string());
        for item in &bundle.failing_commands {
            lines.push(format!("- {}", truncate_line(&item, 220)));
        }
    }

    if !bundle.failing_tools.is_empty() {
        lines.push("Recent tool failures:".to_string());
        for item in &bundle.failing_tools {
            lines.push(format!("- {}", truncate_line(&item, 220)));
        }
    }

    if !bundle.related_commands.is_empty() {
        lines.push("Related commands:".to_string());
        for line in &bundle.related_commands {
            lines.push(format!("- {}", truncate_line(&line, 220)));
        }
    }

    if !bundle.related_output.is_empty() {
        lines.push("Related output lines:".to_string());
        for line in &bundle.related_output {
            lines.push(format!("- {}", truncate_line(&line, 220)));
        }
    }

    if !bundle.recent_edits.is_empty() {
        lines.push("Recent edited files:".to_string());
        for line in &bundle.recent_edits {
            lines.push(format!("- {}", truncate_line(&line, 220)));
        }
    }

    if !bundle.selected_evidence.is_empty() {
        lines.push("Top evidence:".to_string());
        for item in bundle.selected_evidence.iter().take(8) {
            lines.push(format!(
                "- [{} {:.1}] {}",
                item.category,
                item.score,
                truncate_line(&item.text, 200)
            ));
        }
    }

    if lines.len() <= 2 {
        lines.push("No relevant indexed shell context was found.".to_string());
    }

    let packed =
        pack_lines_with_budget(lines, options.max_lines.max(10), options.max_chars.max(120));
    packed.join("\n")
}

#[derive(Debug, Clone)]
struct CommandFailure {
    text: String,
    ts_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct ToolFailure {
    text: String,
    ts_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct ScoredCandidate {
    category: &'static str,
    text: String,
    score: f32,
    output_id: Option<i64>,
    reasons: Vec<String>,
}

fn correlate_command_failures(events: &[EventRecord], max: usize) -> Vec<CommandFailure> {
    let mut pending_cmds: Vec<(String, Option<i64>)> = Vec::new();
    let mut failures = Vec::new();

    for event in events.iter().rev() {
        match event.kind.as_str() {
            "command.start" => {
                let cmd = event
                    .data
                    .get("cmd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !cmd.is_empty() {
                    pending_cmds.push((cmd, event.ts_ms));
                }
            }
            "command.end" => {
                let exit = event.data.get("exit").and_then(value_to_i64).unwrap_or(0);
                if exit == 0 {
                    continue;
                }
                let text = if let Some((cmd, _)) = pending_cmds.pop() {
                    format!("{cmd} -> exit {exit}")
                } else {
                    format!("command ended with exit {exit}")
                };
                failures.push(CommandFailure {
                    text,
                    ts_ms: event.ts_ms,
                });
            }
            _ => {}
        }
    }

    failures.reverse();
    failures.truncate(max);
    failures
}

fn correlate_tool_failures(events: &[EventRecord], max: usize) -> Vec<ToolFailure> {
    let mut pending_tools: Vec<String> = Vec::new();
    let mut failures = Vec::new();

    for event in events.iter().rev() {
        match event.kind.as_str() {
            "tool.start" => {
                if let Some(tool) = event.data.get("tool").and_then(|v| v.as_str()) {
                    pending_tools.push(tool.to_string());
                }
            }
            "tool.end" => {
                let ok = event
                    .data
                    .get("ok")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if ok {
                    continue;
                }
                let tool = event
                    .data
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| pending_tools.pop())
                    .unwrap_or_else(|| "unknown".to_string());
                let error = event
                    .data
                    .get("error")
                    .map(value_to_searchable_text)
                    .unwrap_or_else(|| "unknown error".to_string());
                failures.push(ToolFailure {
                    text: format!("{tool} failed: {error}"),
                    ts_ms: event.ts_ms,
                });
            }
            _ => {}
        }
    }

    failures.reverse();
    failures.truncate(max);
    failures
}

fn score_command_failures(failures: &[CommandFailure], tokens: &[String]) -> Vec<ScoredCandidate> {
    let total = failures.len().max(1) as f32;
    failures
        .iter()
        .enumerate()
        .map(|(idx, failure)| {
            let overlap = token_overlap_count(&failure.text, tokens) as f32;
            let recency = recency_boost(idx, total);
            ScoredCandidate {
                category: "failure.command",
                text: failure.text.clone(),
                score: 8.0 + overlap * 2.5 + recency,
                output_id: None,
                reasons: vec![
                    "non-zero exit".to_string(),
                    if overlap > 0.0 {
                        "query token overlap".to_string()
                    } else {
                        "recent failure".to_string()
                    },
                    ts_reason(failure.ts_ms),
                ],
            }
        })
        .collect()
}

fn score_tool_failures(failures: &[ToolFailure], tokens: &[String]) -> Vec<ScoredCandidate> {
    let total = failures.len().max(1) as f32;
    failures
        .iter()
        .enumerate()
        .map(|(idx, failure)| {
            let overlap = token_overlap_count(&failure.text, tokens) as f32;
            let recency = recency_boost(idx, total);
            ScoredCandidate {
                category: "failure.tool",
                text: failure.text.clone(),
                score: 7.0 + overlap * 2.0 + recency,
                output_id: None,
                reasons: vec![
                    "tool failure".to_string(),
                    if overlap > 0.0 {
                        "query token overlap".to_string()
                    } else {
                        "recent failure".to_string()
                    },
                    ts_reason(failure.ts_ms),
                ],
            }
        })
        .collect()
}

fn score_commands(lines: &[String], tokens: &[String], max_scan: usize) -> Vec<ScoredCandidate> {
    let total = max_scan.min(lines.len()).max(1) as f32;
    lines
        .iter()
        .take(max_scan)
        .enumerate()
        .filter_map(|(idx, line)| {
            let text = line.trim();
            if text.is_empty() {
                return None;
            }
            let overlap = token_overlap_count(text, tokens) as f32;
            let score = overlap * 2.2 + recency_boost(idx, total) * 0.8;
            if overlap <= 0.0 && !tokens.is_empty() {
                return None;
            }
            Some(ScoredCandidate {
                category: "command",
                text: text.to_string(),
                score,
                output_id: None,
                reasons: vec![if overlap > 0.0 {
                    "query token overlap".to_string()
                } else {
                    "recent command".to_string()
                }],
            })
        })
        .collect()
}

fn score_output(
    lines: &[OutputRecord],
    tokens: &[String],
    max_scan: usize,
) -> Vec<ScoredCandidate> {
    let total = max_scan.min(lines.len()).max(1) as f32;
    lines
        .iter()
        .take(max_scan)
        .enumerate()
        .filter_map(|(idx, rec)| {
            let text = rec.line_text.trim();
            if text.is_empty() {
                return None;
            }
            let lowered = text.to_lowercase();
            let overlap = token_overlap_count(&lowered, tokens) as f32;
            let has_error = contains_error_signal(&lowered);
            let stderr_boost = if rec.stream == "stderr" { 2.5 } else { 0.0 };
            let error_boost = if has_error { 3.0 } else { 0.0 };
            let score = overlap * 2.0 + stderr_boost + error_boost + recency_boost(idx, total);
            if score < 2.5 {
                return None;
            }

            let mut reasons = Vec::new();
            if rec.stream == "stderr" {
                reasons.push("stderr stream".to_string());
            }
            if has_error {
                reasons.push("error signal".to_string());
            }
            if overlap > 0.0 {
                reasons.push("query token overlap".to_string());
            }
            if reasons.is_empty() {
                reasons.push("recent output".to_string());
            }

            Some(ScoredCandidate {
                category: "output",
                text: format!("[{}] {}", rec.stream, text),
                score,
                output_id: Some(rec.id),
                reasons,
            })
        })
        .collect()
}

fn score_edits(
    edits: &[FileEditRecord],
    tokens: &[String],
    max_scan: usize,
) -> Vec<ScoredCandidate> {
    let total = max_scan.min(edits.len()).max(1) as f32;
    edits
        .iter()
        .take(max_scan)
        .enumerate()
        .filter_map(|(idx, rec)| {
            let path = rec.path.as_deref()?.trim();
            if path.is_empty() {
                return None;
            }
            let mode = rec.write_mode.as_deref().unwrap_or("unknown");
            let text = format!("{path} ({mode})");
            let overlap = token_overlap_count(path, tokens) as f32;
            let score = 1.6 + overlap * 1.8 + recency_boost(idx, total) * 0.5;
            let reason = if overlap > 0.0 {
                "query token overlap"
            } else {
                "recent edit"
            };
            Some(ScoredCandidate {
                category: "edit",
                text,
                score,
                output_id: None,
                reasons: vec![reason.to_string()],
            })
        })
        .collect()
}

fn select_candidates(
    mut candidates: Vec<ScoredCandidate>,
    max_total: usize,
) -> Vec<ScoredCandidate> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut failure_cmd_count = 0usize;
    let mut failure_tool_count = 0usize;
    let mut command_count = 0usize;
    let mut output_count = 0usize;
    let mut edit_count = 0usize;

    for candidate in candidates {
        if !seen.insert(candidate.text.clone()) {
            continue;
        }
        let within_cap = match candidate.category {
            "failure.command" => {
                if failure_cmd_count >= 8 {
                    false
                } else {
                    failure_cmd_count += 1;
                    true
                }
            }
            "failure.tool" => {
                if failure_tool_count >= 8 {
                    false
                } else {
                    failure_tool_count += 1;
                    true
                }
            }
            "command" => {
                if command_count >= 10 {
                    false
                } else {
                    command_count += 1;
                    true
                }
            }
            "output" => {
                if output_count >= 16 {
                    false
                } else {
                    output_count += 1;
                    true
                }
            }
            "edit" => {
                if edit_count >= 8 {
                    false
                } else {
                    edit_count += 1;
                    true
                }
            }
            _ => false,
        };
        if !within_cap {
            continue;
        }
        out.push(candidate);
        if out.len() >= max_total {
            break;
        }
    }
    out
}

fn selected_lines_by_category(
    selected: &[ScoredCandidate],
    category: &str,
    max: usize,
) -> Vec<String> {
    selected
        .iter()
        .filter(|item| item.category == category)
        .take(max)
        .map(|item| item.text.clone())
        .collect()
}

fn selected_output_ids(selected: &[ScoredCandidate], max: usize) -> Vec<i64> {
    selected
        .iter()
        .filter_map(|item| {
            if item.category == "output" {
                item.output_id
            } else {
                None
            }
        })
        .take(max)
        .collect()
}

fn expand_output_windows(
    lines: &[OutputRecord],
    selected_ids: &[i64],
    window: usize,
    max: usize,
) -> Vec<String> {
    let mut by_id = HashMap::new();
    for (idx, rec) in lines.iter().enumerate() {
        by_id.insert(rec.id, idx);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in selected_ids {
        let Some(&center) = by_id.get(id) else {
            continue;
        };
        let start = center.saturating_sub(window);
        let end = (center + window).min(lines.len().saturating_sub(1));
        for idx in start..=end {
            let rec = &lines[idx];
            let text = rec.line_text.trim();
            if text.is_empty() {
                continue;
            }
            let decorated = format!("[{}] {}", rec.stream, text);
            if seen.insert(decorated.clone()) {
                out.push(decorated);
            }
            if out.len() >= max {
                return out;
            }
        }
    }
    out
}

fn pack_lines_with_budget(lines: Vec<String>, max_lines: usize, max_chars: usize) -> Vec<String> {
    let total_lines = lines.len();
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        if out.len() >= max_lines {
            break;
        }
        let line_chars = line.chars().count();
        let extra = if out.is_empty() {
            line_chars
        } else {
            line_chars + 1
        };
        if used + extra > max_chars {
            break;
        }
        used += extra;
        out.push(line);
    }
    if out.is_empty() {
        return vec!["No relevant indexed shell context was found.".to_string()];
    }
    let trunc = "... context truncated for budget ...";
    let trunc_extra = trunc.chars().count() + 1;
    if out.len() < total_lines && out.len() < max_lines && used + trunc_extra <= max_chars {
        out.push(trunc.to_string());
    }
    out
}

fn recency_boost(idx: usize, total: f32) -> f32 {
    if total <= 1.0 {
        return 1.0;
    }
    let idx = idx as f32;
    ((total - idx) / total).max(0.0)
}

fn token_overlap_count(text: &str, tokens: &[String]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    let lowered = text.to_lowercase();
    tokens
        .iter()
        .filter(|token| lowered.contains(token.as_str()))
        .count()
}

fn ts_reason(ts_ms: Option<i64>) -> String {
    match ts_ms {
        Some(ts) => format!("ts={ts}"),
        None => "ts=unknown".to_string(),
    }
}

fn ingest_session_from_dir(
    db_path: &Path,
    session_id: &str,
    dir: &Path,
) -> Result<IngestStats, String> {
    if !dir.exists() || !dir.is_dir() {
        return Ok(IngestStats::default());
    }
    let mut stats = IngestStats::default();
    let conn = open_conn(db_path)?;

    let events = ingest_event_file(&conn, session_id, &dir.join("events.jsonl"))?;
    stats.add(events);

    let stdin_stats = ingest_text_file(
        &conn,
        session_id,
        &dir.join("stdin.log"),
        "command_inputs",
        None,
    )?;
    stats.add(stdin_stats);

    let output_stats = ingest_text_file(
        &conn,
        session_id,
        &dir.join("output.log"),
        "pane_output",
        Some("merged"),
    )?;
    stats.add(output_stats);

    let stdout_stats = ingest_text_file(
        &conn,
        session_id,
        &dir.join("stdout.log"),
        "pane_output",
        Some("stdout"),
    )?;
    stats.add(stdout_stats);

    let stderr_stats = ingest_text_file(
        &conn,
        session_id,
        &dir.join("stderr.log"),
        "pane_output",
        Some("stderr"),
    )?;
    stats.add(stderr_stats);

    Ok(stats)
}

#[derive(Debug)]
struct EventRecord {
    ts_ms: Option<i64>,
    kind: String,
    data: Value,
}

#[derive(Debug)]
struct OutputRecord {
    id: i64,
    stream: String,
    line_text: String,
}

#[derive(Debug)]
struct FileEditRecord {
    path: Option<String>,
    write_mode: Option<String>,
}

fn load_command_lines(
    conn: &Connection,
    session_id: &str,
    limit: i64,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT line_text
            FROM command_inputs
            WHERE session_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, limit], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(value) = row {
            out.push(value);
        }
    }
    Ok(out)
}

fn load_output_lines(
    conn: &Connection,
    session_id: &str,
    limit: i64,
) -> Result<Vec<OutputRecord>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT id, stream, line_text
            FROM pane_output
            WHERE session_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, limit], |row| {
            Ok(OutputRecord {
                id: row.get::<_, i64>(0)?,
                stream: row.get::<_, String>(1)?,
                line_text: row.get::<_, String>(2)?,
            })
        })
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(value) = row {
            out.push(value);
        }
    }
    Ok(out)
}

fn load_events(
    conn: &Connection,
    session_id: &str,
    limit: i64,
) -> Result<Vec<EventRecord>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT ts_ms, kind, data_json
            FROM events
            WHERE session_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, limit], |row| {
            let ts_ms: Option<i64> = row.get(0)?;
            let kind: String = row.get(1)?;
            let data_json: String = row.get(2)?;
            let data = serde_json::from_str::<Value>(&data_json).unwrap_or(Value::Null);
            Ok(EventRecord { ts_ms, kind, data })
        })
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(value) = row {
            out.push(value);
        }
    }
    Ok(out)
}

fn load_file_edits(
    conn: &Connection,
    session_id: &str,
    limit: i64,
) -> Result<Vec<FileEditRecord>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT path, write_mode
            FROM file_edits
            WHERE session_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, limit], |row| {
            Ok(FileEditRecord {
                path: row.get::<_, Option<String>>(0)?,
                write_mode: row.get::<_, Option<String>>(1)?,
            })
        })
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(value) = row {
            out.push(value);
        }
    }
    Ok(out)
}

fn query_tokens(query: &str) -> Vec<String> {
    let stopwords = [
        "what", "did", "i", "me", "my", "the", "a", "an", "to", "of", "in", "on", "for", "with",
        "is", "are", "it", "this", "that", "be", "and", "or", "was", "were", "you", "we",
    ];
    let stop: HashSet<&str> = stopwords.into_iter().collect();
    query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|raw| {
            let t = raw.trim().to_lowercase();
            if t.len() < 3 || stop.contains(t.as_str()) {
                None
            } else {
                Some(t)
            }
        })
        .collect()
}

fn contains_error_signal(text: &str) -> bool {
    [
        "error",
        "failed",
        "failure",
        "panic",
        "exception",
        "traceback",
        "cannot",
        "not found",
        "permission denied",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn value_to_searchable_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if value.is_null() {
        return "null".to_string();
    }
    value.to_string()
}

fn truncate_line(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect::<String>() + "..."
}

fn ingest_event_file(
    conn: &Connection,
    fallback_session_id: &str,
    path: &Path,
) -> Result<IngestStats, String> {
    if !path.exists() {
        return Ok(IngestStats::default());
    }
    let file = fs::File::open(path).map_err(|err| err.to_string())?;
    let reader = BufReader::new(file);
    let source = path.to_string_lossy().to_string();
    let mut stats = IngestStats::default();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| err.to_string())?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let line_no = (idx + 1) as i64;
        let delta = insert_event(
            conn,
            &value,
            Some(fallback_session_id),
            Some(&source),
            Some(line_no),
        )?;
        stats.add(delta);
    }
    Ok(stats)
}

fn ingest_text_file(
    conn: &Connection,
    session_id: &str,
    path: &Path,
    table: &str,
    stream: Option<&str>,
) -> Result<IngestStats, String> {
    if !path.exists() {
        return Ok(IngestStats::default());
    }
    let file = fs::File::open(path).map_err(|err| err.to_string())?;
    let reader = BufReader::new(file);
    let source = path.to_string_lossy().to_string();
    let mut stats = IngestStats::default();

    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| err.to_string())?;
        let line_no = (idx + 1) as i64;
        let inserted = match table {
            "command_inputs" => conn
                .execute(
                    "
                    INSERT OR IGNORE INTO command_inputs (
                        session_id, ts_ms, line_text, source, source_line, created_at_ms
                    ) VALUES (?1, NULL, ?2, ?3, ?4, ?5)
                    ",
                    params![session_id, line, source, line_no, now_ms_i64()],
                )
                .map_err(|err| err.to_string())?,
            "pane_output" => conn
                .execute(
                    "
                    INSERT OR IGNORE INTO pane_output (
                        session_id, ts_ms, stream, line_text, source, source_line, created_at_ms
                    ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)
                    ",
                    params![
                        session_id,
                        stream.unwrap_or("merged"),
                        line,
                        source,
                        line_no,
                        now_ms_i64()
                    ],
                )
                .map_err(|err| err.to_string())?,
            _ => 0,
        };
        if inserted > 0 {
            if table == "command_inputs" {
                stats.stdin_lines_inserted += 1;
            } else if table == "pane_output" {
                stats.output_lines_inserted += 1;
            }
        }
    }
    Ok(stats)
}

fn insert_event(
    conn: &Connection,
    event: &Value,
    fallback_session_id: Option<&str>,
    source: Option<&str>,
    source_line: Option<i64>,
) -> Result<IngestStats, String> {
    let ts_ms = event
        .get("ts_ms")
        .and_then(value_to_i64)
        .unwrap_or_else(now_ms_i64);
    let session_id = event
        .get("session_id")
        .and_then(|v| v.as_str())
        .or(fallback_session_id)
        .map(|s| s.to_string());
    let agent_id = event
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let actor = event
        .get("actor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let kind = event
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let data = event.get("data").cloned().unwrap_or(Value::Null);
    let data_json = serde_json::to_string(&data).unwrap_or_else(|_| "null".to_string());
    let source_owned = source.map(|s| s.to_string());
    let inserted = conn
        .execute(
            "
            INSERT OR IGNORE INTO events (
                ts_ms, session_id, agent_id, actor, kind, data_json, source, source_line, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                ts_ms,
                session_id,
                agent_id,
                actor,
                kind,
                data_json,
                source_owned,
                source_line,
                now_ms_i64(),
            ],
        )
        .map_err(|err| err.to_string())?;

    let mut stats = IngestStats::default();
    if inserted > 0 {
        stats.events_inserted += 1;
        if let Some(session_id) = event
            .get("session_id")
            .and_then(|v| v.as_str())
            .or(fallback_session_id)
        {
            let _ = touch_session_conn(conn, session_id, ts_ms);
        }
        if is_file_write_event(&kind, &data) {
            let path = data
                .get("args")
                .and_then(|args| args.get("path"))
                .and_then(|v| v.as_str());
            let mode = data
                .get("args")
                .and_then(|args| args.get("append"))
                .and_then(|v| v.as_bool())
                .map(|append| if append { "append" } else { "overwrite" })
                .unwrap_or("overwrite");
            let bytes = data
                .get("args")
                .and_then(|args| args.get("content"))
                .and_then(|v| v.as_str())
                .map(|text| text.len() as i64);
            let source_value = source.map(|s| s.to_string());
            let edit_inserted = conn
                .execute(
                    "
                    INSERT OR IGNORE INTO file_edits (
                        ts_ms, session_id, agent_id, tool, path, write_mode, bytes, source, source_line, data_json
                    ) VALUES (?1, ?2, ?3, 'fs.write', ?4, ?5, ?6, ?7, ?8, ?9)
                    ",
                    params![
                        ts_ms,
                        session_id,
                        agent_id,
                        path,
                        mode,
                        bytes,
                        source_value,
                        source_line,
                        serde_json::to_string(&data).unwrap_or_else(|_| "null".to_string())
                    ],
                )
                .map_err(|err| err.to_string())?;
            if edit_inserted > 0 {
                stats.file_edits_inserted += 1;
            }
        }
    }

    Ok(stats)
}

fn is_file_write_event(kind: &str, data: &Value) -> bool {
    if kind != "tool.start" {
        return false;
    }
    data.get("tool")
        .and_then(|v| v.as_str())
        .map(|tool| tool == "fs.write")
        .unwrap_or(false)
}

fn touch_session_conn(conn: &Connection, id: &str, ts_ms: i64) -> Result<(), String> {
    conn.execute(
        "
        INSERT INTO sessions (
            id, title, status, share_state, tmux_session_name, created_at, updated_at, last_seen_ms
        ) VALUES (?1, 'unknown', 'unknown', 'manual', NULL, 0, 0, ?2)
        ON CONFLICT(id) DO UPDATE SET
            last_seen_ms = CASE
                WHEN excluded.last_seen_ms > sessions.last_seen_ms THEN excluded.last_seen_ms
                ELSE sessions.last_seen_ms
            END
        ",
        params![id, ts_ms],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn open_conn(db_path: &Path) -> Result<Connection, String> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let conn = Connection::open(db_path).map_err(|err| err.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| err.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|err| err.to_string())?;
    Ok(conn)
}

fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn to_i64_u128(value: u128) -> i64 {
    value.min(i64::MAX as u128) as i64
}

fn value_to_i64(value: &Value) -> Option<i64> {
    if let Some(v) = value.as_i64() {
        return Some(v);
    }
    if let Some(v) = value.as_u64() {
        return Some(v.min(i64::MAX as u64) as i64);
    }
    if let Some(v) = value.as_str() {
        if let Ok(parsed) = v.parse::<i64>() {
            return Some(parsed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "aishd-log-index-test-{}-{}-{}",
            name,
            std::process::id(),
            now_ms_i64()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn ingest_indexes_stdin_and_merged_output() {
        let root = test_root("merged");
        let session_id = "sess_test_merged";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":1,\"session_id\":\"sess_test_merged\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"echo hi\"}}\n\
             {\"ts_ms\":2,\"session_id\":\"sess_test_merged\",\"kind\":\"command.end\",\"data\":{\"exit\":0}}\n",
        );
        write_file(&session_dir.join("stdin.log"), "echo hi\nls -la\n");
        write_file(
            &session_dir.join("output.log"),
            "hello stdout\nhello stderr\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        let stats = ingest_session(&root, &db_path, session_id).unwrap();
        assert_eq!(stats.stdin_lines_inserted, 2);
        assert_eq!(stats.output_lines_inserted, 2);
        assert_eq!(stats.events_inserted, 2);

        let conn = Connection::open(&db_path).unwrap();
        let stdin_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM command_inputs WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stdin_count, 2);

        let merged_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pane_output WHERE session_id = ?1 AND stream = 'merged'",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merged_count, 2);

        let events_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events_count, 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ingest_indexes_stdout_and_stderr_streams() {
        let root = test_root("split");
        let session_id = "sess_test_split";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(&session_dir.join("stdout.log"), "out line 1\nout line 2\n");
        write_file(&session_dir.join("stderr.log"), "err line 1\n");

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        let stats = ingest_session(&root, &db_path, session_id).unwrap();
        assert_eq!(stats.output_lines_inserted, 3);

        let conn = Connection::open(&db_path).unwrap();
        let stdout_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pane_output WHERE session_id = ?1 AND stream = 'stdout'",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stdout_count, 2);

        let stderr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pane_output WHERE session_id = ?1 AND stream = 'stderr'",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stderr_count, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_relevant_context_surfaces_failures_and_related_lines() {
        let root = test_root("context");
        let session_id = "sess_test_context";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":9,\"session_id\":\"sess_test_context\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"cargo build\"}}\n\
             {\"ts_ms\":10,\"session_id\":\"sess_test_context\",\"kind\":\"command.end\",\"data\":{\"exit\":101}}\n\
             {\"ts_ms\":11,\"session_id\":\"sess_test_context\",\"kind\":\"tool.end\",\"data\":{\"tool\":\"shell\",\"ok\":false,\"error\":\"build failed\"}}\n\
             {\"ts_ms\":12,\"session_id\":\"sess_test_context\",\"kind\":\"tool.start\",\"data\":{\"tool\":\"fs.write\",\"args\":{\"path\":\"src/main.rs\",\"content\":\"fn main(){}\"}}}\n",
        );
        write_file(&session_dir.join("stdin.log"), "cargo build\ncargo test\n");
        write_file(
            &session_dir.join("stderr.log"),
            "error[E0425]: cannot find value `x` in this scope\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let context = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "what did i do wrong with cargo build?",
            BuildContextOptions {
                max_lines: 120,
                ..BuildContextOptions::default()
            },
        )
        .unwrap()
        .context_text;

        assert!(context.contains("Recent command failures"));
        assert!(context.contains("cargo build -> exit 101"));
        assert!(context.contains("exit 101"));
        assert!(context.contains("Related commands"));
        assert!(context.contains("cargo build"));
        assert!(context.contains("Related output lines"));
        assert!(context.contains("error[E0425]"));
        assert!(context.contains("Recent edited files"));
        assert!(context.contains("src/main.rs"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_relevant_context_bundle_includes_sections() {
        let root = test_root("bundle");
        let session_id = "sess_test_bundle";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":4,\"session_id\":\"sess_test_bundle\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"git status\"}}\n\
             {\"ts_ms\":5,\"session_id\":\"sess_test_bundle\",\"kind\":\"command.end\",\"data\":{\"exit\":2}}\n\
             {\"ts_ms\":6,\"session_id\":\"sess_test_bundle\",\"kind\":\"tool.start\",\"data\":{\"tool\":\"fs.write\",\"args\":{\"path\":\"README.md\",\"content\":\"x\"}}}\n",
        );
        write_file(&session_dir.join("stdin.log"), "git status\n");
        write_file(
            &session_dir.join("stderr.log"),
            "fatal: not a git repository\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "git failed",
            BuildContextOptions {
                max_lines: 120,
                ..BuildContextOptions::default()
            },
        )
        .unwrap();
        assert_eq!(bundle.session_id, session_id);
        assert!(!bundle.query_tokens.is_empty());
        assert!(!bundle.failing_commands.is_empty());
        assert!(!bundle.recent_edits.is_empty());
        assert!(!bundle.related_output.is_empty());
        assert!(!bundle.selected_evidence.is_empty());
        assert!(bundle.failing_commands[0].contains("git status"));
        assert!(bundle.context_text.contains("Recent command failures"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_bundle_respects_max_chars_budget() {
        let root = test_root("budget");
        let session_id = "sess_test_budget";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":4,\"session_id\":\"sess_test_budget\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"cargo test --workspace --all-features\"}}\n\
             {\"ts_ms\":5,\"session_id\":\"sess_test_budget\",\"kind\":\"command.end\",\"data\":{\"exit\":101}}\n",
        );
        write_file(
            &session_dir.join("stderr.log"),
            "error: this is a long synthetic error line for budget testing\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let options = BuildContextOptions {
            max_lines: 120,
            max_chars: 180,
            output_window: 1,
            max_evidence: 36,
        };
        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "what failed?",
            options,
        )
        .unwrap();
        assert!(bundle.context_text.chars().count() <= 180);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn output_window_adds_neighbor_lines() {
        let root = test_root("window");
        let session_id = "sess_test_window";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("output.log"),
            "build started\nTOKEN_MATCH_FAILURE\nhint: check imports\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let options = BuildContextOptions {
            max_lines: 120,
            max_chars: 4500,
            output_window: 1,
            max_evidence: 36,
        };
        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "token_match_failure",
            options,
        )
        .unwrap();

        let output = bundle.related_output.join("\n");
        assert!(output.contains("[merged] TOKEN_MATCH_FAILURE"));
        assert!(output.contains("[merged] build started"));
        assert!(output.contains("[merged] hint: check imports"));

        let _ = fs::remove_dir_all(root);
    }
}
