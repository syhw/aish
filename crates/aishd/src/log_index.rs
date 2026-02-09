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
    pub intent: String,
    pub query_tokens: Vec<String>,
    pub incidents: Vec<IncidentItem>,
    pub artifacts: Vec<ArtifactItem>,
    pub failing_commands: Vec<String>,
    pub failing_tools: Vec<String>,
    pub related_commands: Vec<String>,
    pub related_output: Vec<String>,
    pub recent_edits: Vec<String>,
    pub selected_evidence: Vec<EvidenceItem>,
    pub context_text: String,
    pub explain: Option<ContextExplain>,
}

#[derive(Debug, Serialize, Clone)]
pub struct EvidenceItem {
    pub category: String,
    pub text: String,
    pub score: f32,
    pub reasons: Vec<String>,
    pub source_table: Option<String>,
    pub source_id: Option<i64>,
    pub ts_ms: Option<i64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct LlmSelectionCorpus {
    pub session_id: String,
    pub last_stdin: Vec<String>,
    pub last_stdout: Vec<String>,
    pub last_stderr: Vec<String>,
    pub old_context_chunks: Vec<String>,
    pub total_old_lines: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct BuildLlmSelectionCorpusOptions {
    pub chunk_chars: usize,
    pub max_chunks: usize,
    pub include_events: bool,
}

impl Default for BuildLlmSelectionCorpusOptions {
    fn default() -> Self {
        Self {
            chunk_chars: 512_000,
            max_chunks: 16,
            include_events: true,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct IncidentItem {
    pub title: String,
    pub summary: Vec<String>,
    pub score: f32,
    pub reasons: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ArtifactItem {
    pub artifact_type: String,
    pub text: String,
    pub ts_ms: Option<i64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContextExplain {
    pub intent: String,
    pub selected_count: usize,
    pub candidate_count: usize,
    pub profile: ContextProfileExplain,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContextProfileExplain {
    pub max_evidence: usize,
    pub max_incidents: usize,
    pub category_caps: HashMap<String, usize>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum QueryIntent {
    Debug,
    WhatChanged,
    Resume,
    Status,
    General,
}

impl QueryIntent {
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "debug" | "diagnostic" => Some(Self::Debug),
            "what-changed" | "changes" | "edits" => Some(Self::WhatChanged),
            "resume" | "continue" => Some(Self::Resume),
            "status" | "summary" => Some(Self::Status),
            "general" => Some(Self::General),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::WhatChanged => "what-changed",
            Self::Resume => "resume",
            Self::Status => "status",
            Self::General => "general",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BuildContextOptions {
    pub max_lines: usize,
    pub max_chars: usize,
    pub output_window: usize,
    pub max_evidence: usize,
    pub max_incidents: usize,
    pub intent_override: Option<QueryIntent>,
    pub include_artifacts: bool,
    pub explain: bool,
}

impl Default for BuildContextOptions {
    fn default() -> Self {
        Self {
            max_lines: 120,
            max_chars: 4500,
            output_window: 1,
            max_evidence: 36,
            max_incidents: 6,
            intent_override: None,
            include_artifacts: true,
            explain: false,
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

        CREATE TABLE IF NOT EXISTS context_artifacts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            artifact_type TEXT NOT NULL,
            text TEXT NOT NULL,
            ts_ms INTEGER,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(session_id, artifact_type, text)
        );
        CREATE INDEX IF NOT EXISTS idx_context_artifacts_session_updated
            ON context_artifacts(session_id, updated_at_ms DESC);
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
    refresh_context_artifacts(&conn, session_id)?;
    let tokens = query_tokens(query);
    let intent = options
        .intent_override
        .unwrap_or_else(|| detect_query_intent(query));
    let profile = RetrievalProfile::for_intent(intent, options);

    let commands = load_command_lines(&conn, session_id, 250)?;
    let outputs = load_output_lines(&conn, session_id, 500)?;
    let events = load_events(&conn, session_id, 500)?;
    let edits = load_file_edits(&conn, session_id, 60)?;
    let artifacts = if options.include_artifacts {
        load_artifacts(&conn, session_id, 80)?
    } else {
        Vec::new()
    };

    let command_failures = correlate_command_failures(&events, 24);
    let tool_failures = correlate_tool_failures(&events, 24);
    let incidents = build_incidents(
        &command_failures,
        &tool_failures,
        &commands,
        &outputs,
        &edits,
        &tokens,
        profile,
    );

    let mut candidates = Vec::new();
    candidates.extend(score_command_failures(&command_failures, &tokens));
    candidates.extend(score_tool_failures(&tool_failures, &tokens));
    candidates.extend(score_commands(&commands, &tokens, 40));
    candidates.extend(score_output(&outputs, &tokens, 80));
    candidates.extend(score_edits(&edits, &tokens, 30));
    candidates.extend(score_incidents(&incidents, profile.incident_weight));
    candidates.extend(score_artifacts(
        &artifacts,
        &tokens,
        profile.artifact_weight,
    ));

    let candidate_count = candidates.len();
    let selected = select_candidates(candidates, options.max_evidence.max(8), profile);
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
            source_table: candidate.source_table.map(|value| value.to_string()),
            source_id: candidate.source_id,
            ts_ms: candidate.ts_ms,
        })
        .collect::<Vec<_>>();

    let mut bundle = ContextBundle {
        session_id: session_id.to_string(),
        query: query.to_string(),
        intent: intent.as_str().to_string(),
        query_tokens: tokens,
        incidents: incidents
            .into_iter()
            .take(profile.max_incidents)
            .map(|incident| IncidentItem {
                title: incident.title,
                summary: incident.summary,
                score: ((incident.score * 100.0).round() / 100.0),
                reasons: incident.reasons,
            })
            .collect(),
        artifacts: artifacts
            .iter()
            .take(profile.max_artifacts)
            .map(|artifact| ArtifactItem {
                artifact_type: artifact.artifact_type.clone(),
                text: artifact.text.clone(),
                ts_ms: artifact.ts_ms,
            })
            .collect(),
        failing_commands,
        failing_tools,
        related_commands,
        related_output,
        recent_edits,
        selected_evidence,
        context_text: String::new(),
        explain: None,
    };
    if options.explain {
        bundle.explain = Some(ContextExplain {
            intent: intent.as_str().to_string(),
            selected_count: bundle.selected_evidence.len(),
            candidate_count,
            profile: ContextProfileExplain {
                max_evidence: options.max_evidence.max(8),
                max_incidents: profile.max_incidents,
                category_caps: profile.category_caps_map(),
            },
        });
    }
    bundle.context_text = render_context_text(&bundle, options);
    Ok(bundle)
}

pub fn build_llm_selection_corpus(
    db_path: &Path,
    session_id: &str,
    options: BuildLlmSelectionCorpusOptions,
) -> Result<LlmSelectionCorpus, String> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err("invalid session_id".to_string());
    }
    let conn = open_conn(db_path)?;

    let last_stdin = load_recent_input_lines(&conn, session_id, 4)?;
    let last_stdout = load_recent_output_lines_for_stream(&conn, session_id, "stdout", 8)?;
    let last_stderr = load_recent_output_lines_for_stream(&conn, session_id, "stderr", 8)?;

    let mut line_count = 0usize;
    let mut chunks = Vec::new();
    let mut current = String::new();
    let chunk_chars = options.chunk_chars.max(64);
    let max_chunks = options.max_chunks.max(1);
    let mut truncated = false;

    push_chunk_line(
        &mut chunks,
        &mut current,
        "### COMMAND_INPUTS",
        chunk_chars,
        max_chunks,
        &mut truncated,
    );
    let mut stmt = conn
        .prepare(
            "
            SELECT id, line_text
            FROM command_inputs
            WHERE session_id = ?1
            ORDER BY id ASC
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?;
    for row in rows {
        let Ok((id, line)) = row else {
            continue;
        };
        line_count += 1;
        if !push_chunk_line(
            &mut chunks,
            &mut current,
            &format!("[stdin#{id}] {}", line.trim_end()),
            chunk_chars,
            max_chunks,
            &mut truncated,
        ) {
            break;
        }
    }

    if !truncated {
        push_chunk_line(
            &mut chunks,
            &mut current,
            "### PANE_OUTPUT",
            chunk_chars,
            max_chunks,
            &mut truncated,
        );
        let mut stmt = conn
            .prepare(
                "
                SELECT id, stream, line_text
                FROM pane_output
                WHERE session_id = ?1
                ORDER BY id ASC
                ",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|err| err.to_string())?;
        for row in rows {
            let Ok((id, stream, line)) = row else {
                continue;
            };
            line_count += 1;
            if !push_chunk_line(
                &mut chunks,
                &mut current,
                &format!("[{stream}#{id}] {}", line.trim_end()),
                chunk_chars,
                max_chunks,
                &mut truncated,
            ) {
                break;
            }
        }
    }

    if !truncated {
        push_chunk_line(
            &mut chunks,
            &mut current,
            "### FILE_EDITS",
            chunk_chars,
            max_chunks,
            &mut truncated,
        );
        let mut stmt = conn
            .prepare(
                "
                SELECT id, path, write_mode, bytes
                FROM file_edits
                WHERE session_id = ?1
                ORDER BY id ASC
                ",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .map_err(|err| err.to_string())?;
        for row in rows {
            let Ok((id, path, mode, bytes)) = row else {
                continue;
            };
            line_count += 1;
            let text = format!(
                "[edit#{id}] path={} mode={} bytes={}",
                path.unwrap_or_else(|| "<unknown>".to_string()),
                mode.unwrap_or_else(|| "unknown".to_string()),
                bytes.unwrap_or(0)
            );
            if !push_chunk_line(
                &mut chunks,
                &mut current,
                &text,
                chunk_chars,
                max_chunks,
                &mut truncated,
            ) {
                break;
            }
        }
    }

    if options.include_events && !truncated {
        push_chunk_line(
            &mut chunks,
            &mut current,
            "### EVENTS",
            chunk_chars,
            max_chunks,
            &mut truncated,
        );
        let mut stmt = conn
            .prepare(
                "
                SELECT id, ts_ms, kind, data_json
                FROM events
                WHERE session_id = ?1
                ORDER BY id ASC
                ",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|err| err.to_string())?;
        for row in rows {
            let Ok((id, ts_ms, kind, data_json)) = row else {
                continue;
            };
            line_count += 1;
            let data = truncate_line(&data_json, 360);
            let text = format!(
                "[event#{id} ts={}] {kind} {data}",
                ts_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".to_string())
            );
            if !push_chunk_line(
                &mut chunks,
                &mut current,
                &text,
                chunk_chars,
                max_chunks,
                &mut truncated,
            ) {
                break;
            }
        }
    }

    if !current.trim().is_empty() {
        chunks.push(current);
    }

    Ok(LlmSelectionCorpus {
        session_id: session_id.to_string(),
        last_stdin,
        last_stdout,
        last_stderr,
        old_context_chunks: chunks,
        total_old_lines: line_count,
        truncated,
    })
}

fn render_context_text(bundle: &ContextBundle, options: BuildContextOptions) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Session: {}", bundle.session_id));
    lines.push(format!("Intent: {}", bundle.intent));
    lines.push(format!("Query: {}", truncate_line(&bundle.query, 220)));

    if !bundle.incidents.is_empty() {
        lines.push("Incident summaries:".to_string());
        for item in bundle.incidents.iter().take(options.max_incidents.max(1)) {
            lines.push(format!(
                "- {} [{}]",
                truncate_line(&item.title, 180),
                truncate_line(&item.reasons.join(", "), 120)
            ));
            for detail in item.summary.iter().take(3) {
                lines.push(format!("  {}", truncate_line(detail, 210)));
            }
        }
    }

    if !bundle.artifacts.is_empty() {
        lines.push("Context artifacts:".to_string());
        for item in bundle.artifacts.iter().take(10) {
            lines.push(format!(
                "- [{}] {}",
                item.artifact_type,
                truncate_line(&item.text, 220)
            ));
        }
    }

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
    command: Option<String>,
    exit: i64,
    text: String,
    ts_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct ToolFailure {
    tool: String,
    error: String,
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
    source_table: Option<&'static str>,
    source_id: Option<i64>,
    ts_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct IncidentCandidate {
    title: String,
    summary: Vec<String>,
    score: f32,
    reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct ArtifactRecord {
    id: i64,
    artifact_type: String,
    text: String,
    ts_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct RetrievalProfile {
    max_incidents: usize,
    max_artifacts: usize,
    max_failure_commands: usize,
    max_failure_tools: usize,
    max_commands: usize,
    max_output: usize,
    max_edits: usize,
    incident_weight: f32,
    artifact_weight: f32,
}

impl RetrievalProfile {
    fn for_intent(intent: QueryIntent, options: BuildContextOptions) -> Self {
        let mut profile = match intent {
            QueryIntent::Debug => Self {
                max_incidents: 8,
                max_artifacts: 8,
                max_failure_commands: 10,
                max_failure_tools: 10,
                max_commands: 8,
                max_output: 20,
                max_edits: 6,
                incident_weight: 1.3,
                artifact_weight: 1.1,
            },
            QueryIntent::WhatChanged => Self {
                max_incidents: 5,
                max_artifacts: 10,
                max_failure_commands: 6,
                max_failure_tools: 4,
                max_commands: 12,
                max_output: 8,
                max_edits: 14,
                incident_weight: 0.9,
                artifact_weight: 1.2,
            },
            QueryIntent::Resume => Self {
                max_incidents: 7,
                max_artifacts: 12,
                max_failure_commands: 8,
                max_failure_tools: 8,
                max_commands: 10,
                max_output: 10,
                max_edits: 10,
                incident_weight: 1.1,
                artifact_weight: 1.25,
            },
            QueryIntent::Status | QueryIntent::General => Self {
                max_incidents: 6,
                max_artifacts: 8,
                max_failure_commands: 8,
                max_failure_tools: 8,
                max_commands: 10,
                max_output: 12,
                max_edits: 8,
                incident_weight: 1.0,
                artifact_weight: 1.0,
            },
        };
        profile.max_incidents = profile.max_incidents.min(options.max_incidents.max(1));
        profile
    }

    fn category_cap(&self, category: &str) -> usize {
        match category {
            "incident" => self.max_incidents,
            "artifact" => self.max_artifacts,
            "failure.command" => self.max_failure_commands,
            "failure.tool" => self.max_failure_tools,
            "command" => self.max_commands,
            "output" => self.max_output,
            "edit" => self.max_edits,
            _ => 0,
        }
    }

    fn category_caps_map(&self) -> HashMap<String, usize> {
        let mut out = HashMap::new();
        out.insert("incident".to_string(), self.max_incidents);
        out.insert("artifact".to_string(), self.max_artifacts);
        out.insert("failure.command".to_string(), self.max_failure_commands);
        out.insert("failure.tool".to_string(), self.max_failure_tools);
        out.insert("command".to_string(), self.max_commands);
        out.insert("output".to_string(), self.max_output);
        out.insert("edit".to_string(), self.max_edits);
        out
    }
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
                let (command, text) = if let Some((cmd, _)) = pending_cmds.pop() {
                    (Some(cmd.clone()), format!("{cmd} -> exit {exit}"))
                } else {
                    (None, format!("command ended with exit {exit}"))
                };
                failures.push(CommandFailure {
                    command,
                    exit,
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
                    tool: tool.clone(),
                    error: error.clone(),
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
                source_table: Some("events"),
                source_id: None,
                ts_ms: failure.ts_ms,
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
                source_table: Some("events"),
                source_id: None,
                ts_ms: failure.ts_ms,
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
                source_table: Some("command_inputs"),
                source_id: None,
                ts_ms: None,
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
                source_table: Some("pane_output"),
                source_id: Some(rec.id),
                ts_ms: rec.ts_ms,
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
                source_table: Some("file_edits"),
                source_id: Some(rec.id),
                ts_ms: rec.ts_ms,
            })
        })
        .collect()
}

fn build_incidents(
    command_failures: &[CommandFailure],
    tool_failures: &[ToolFailure],
    commands: &[String],
    outputs: &[OutputRecord],
    edits: &[FileEditRecord],
    query_tokens: &[String],
    profile: RetrievalProfile,
) -> Vec<IncidentCandidate> {
    let mut incidents = Vec::new();
    let mut used_tools = HashSet::new();
    let mut used_edits = HashSet::new();
    let mut used_output = HashSet::new();

    for (idx, failure) in command_failures.iter().enumerate() {
        if incidents.len() >= profile.max_incidents {
            break;
        }
        let command_text = failure
            .command
            .clone()
            .unwrap_or_else(|| failure.text.clone());
        let mut summary = Vec::new();
        summary.push(format!("failure: {}", failure.text));

        if let Some(cmd) = commands
            .iter()
            .find(|cmd| token_overlap_count(cmd, &query_tokens_for_incident(&command_text)) > 0)
        {
            summary.push(format!("command: {}", truncate_line(cmd, 160)));
        }

        let related_tool_idx = tool_failures
            .iter()
            .enumerate()
            .find_map(|(tool_idx, failure)| {
                if used_tools.contains(&tool_idx) {
                    return None;
                }
                let overlap =
                    token_overlap_count(&failure.text, &query_tokens_for_incident(&command_text));
                if overlap > 0 {
                    Some(tool_idx)
                } else {
                    None
                }
            });
        if let Some(tool_idx) = related_tool_idx {
            used_tools.insert(tool_idx);
            summary.push(format!(
                "tool: {}",
                truncate_line(&tool_failures[tool_idx].text, 180)
            ));
        }

        for output in outputs.iter().take(120) {
            if summary.len() >= 5 {
                break;
            }
            if used_output.contains(&output.id) {
                continue;
            }
            let lowered = output.line_text.to_lowercase();
            let overlap = token_overlap_count(&lowered, &query_tokens_for_incident(&command_text));
            if overlap == 0 && !contains_error_signal(&lowered) {
                continue;
            }
            used_output.insert(output.id);
            summary.push(format!(
                "output: [{}] {}",
                output.stream,
                truncate_line(output.line_text.trim(), 160)
            ));
        }

        for (edit_idx, edit) in edits.iter().enumerate() {
            if summary.len() >= 6 {
                break;
            }
            if used_edits.contains(&edit_idx) {
                continue;
            }
            let Some(path) = edit.path.as_deref() else {
                continue;
            };
            if token_overlap_count(path, &query_tokens_for_incident(&command_text)) == 0 {
                continue;
            }
            used_edits.insert(edit_idx);
            summary.push(format!(
                "edit: {} ({})",
                path,
                edit.write_mode.as_deref().unwrap_or("unknown")
            ));
        }

        let query_overlap = token_overlap_count(&failure.text, query_tokens) as f32;
        let recency = recency_boost(idx, command_failures.len().max(1) as f32);
        incidents.push(IncidentCandidate {
            title: failure.text.clone(),
            summary,
            score: 9.5 + query_overlap * 2.0 + recency,
            reasons: vec![
                "command failure anchor".to_string(),
                if query_overlap > 0.0 {
                    "query token overlap".to_string()
                } else {
                    "recent failure".to_string()
                },
            ],
        });
    }

    for (idx, failure) in tool_failures.iter().enumerate() {
        if incidents.len() >= profile.max_incidents {
            break;
        }
        let mut summary = vec![format!("tool failure: {}", failure.text)];
        if let Some(cmd) = commands
            .iter()
            .find(|cmd| token_overlap_count(cmd, &query_tokens_for_incident(&failure.text)) > 0)
        {
            summary.push(format!("command: {}", truncate_line(cmd, 160)));
        }

        let query_overlap = token_overlap_count(&failure.text, query_tokens) as f32;
        let recency = recency_boost(idx, tool_failures.len().max(1) as f32);
        incidents.push(IncidentCandidate {
            title: failure.text.clone(),
            summary,
            score: 8.5 + query_overlap * 1.8 + recency,
            reasons: vec![
                "tool failure anchor".to_string(),
                if query_overlap > 0.0 {
                    "query token overlap".to_string()
                } else {
                    "recent failure".to_string()
                },
            ],
        });
    }

    if incidents.is_empty() {
        let mut summary = Vec::new();
        if let Some(cmd) = commands.first() {
            summary.push(format!("latest command: {}", truncate_line(cmd, 180)));
        }
        for output in outputs.iter().take(2) {
            summary.push(format!(
                "output: [{}] {}",
                output.stream,
                truncate_line(output.line_text.trim(), 180)
            ));
        }
        if !summary.is_empty() {
            incidents.push(IncidentCandidate {
                title: "recent activity snapshot".to_string(),
                summary,
                score: 2.0,
                reasons: vec!["fallback activity incident".to_string()],
            });
        }
    }

    incidents.sort_by(|a, b| b.score.total_cmp(&a.score));
    incidents.truncate(profile.max_incidents);
    incidents
}

fn score_incidents(incidents: &[IncidentCandidate], weight: f32) -> Vec<ScoredCandidate> {
    incidents
        .iter()
        .enumerate()
        .map(|(idx, incident)| {
            let mut text = incident.title.clone();
            if !incident.summary.is_empty() {
                text.push_str(" | ");
                text.push_str(&incident.summary.join(" | "));
            }
            ScoredCandidate {
                category: "incident",
                text,
                score: incident.score * weight + recency_boost(idx, incidents.len().max(1) as f32),
                output_id: None,
                reasons: incident.reasons.clone(),
                source_table: Some("derived.incident"),
                source_id: None,
                ts_ms: None,
            }
        })
        .collect()
}

fn score_artifacts(
    artifacts: &[ArtifactRecord],
    query_tokens: &[String],
    weight: f32,
) -> Vec<ScoredCandidate> {
    let total = artifacts.len().max(1) as f32;
    artifacts
        .iter()
        .enumerate()
        .map(|(idx, artifact)| {
            let overlap = token_overlap_count(&artifact.text, query_tokens) as f32;
            let base = match artifact.artifact_type.as_str() {
                "blocker" => 6.0,
                "todo" => 4.0,
                "decision" => 4.5,
                "edit" => 3.5,
                _ => 2.5,
            };
            ScoredCandidate {
                category: "artifact",
                text: format!("[{}] {}", artifact.artifact_type, artifact.text),
                score: (base + overlap * 2.0 + recency_boost(idx, total)) * weight,
                output_id: None,
                reasons: vec![
                    "persistent context artifact".to_string(),
                    if overlap > 0.0 {
                        "query token overlap".to_string()
                    } else {
                        "recent artifact".to_string()
                    },
                ],
                source_table: Some("context_artifacts"),
                source_id: Some(artifact.id),
                ts_ms: artifact.ts_ms,
            }
        })
        .collect()
}

fn query_tokens_for_incident(text: &str) -> Vec<String> {
    let mut tokens = query_tokens(text);
    if tokens.len() > 6 {
        tokens.truncate(6);
    }
    tokens
}

fn select_candidates(
    mut candidates: Vec<ScoredCandidate>,
    max_total: usize,
    profile: RetrievalProfile,
) -> Vec<ScoredCandidate> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut category_counts: HashMap<&'static str, usize> = HashMap::new();

    for candidate in candidates {
        if !seen.insert(candidate.text.clone()) {
            continue;
        }
        let cap = profile.category_cap(candidate.category);
        if cap == 0 {
            continue;
        }
        let count = category_counts.entry(candidate.category).or_insert(0);
        if *count >= cap {
            continue;
        }
        *count += 1;
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

fn load_recent_input_lines(
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
    out.reverse();
    Ok(out)
}

fn load_recent_output_lines_for_stream(
    conn: &Connection,
    session_id: &str,
    stream: &str,
    limit: i64,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT line_text
            FROM pane_output
            WHERE session_id = ?1 AND stream = ?2
            ORDER BY id DESC
            LIMIT ?3
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, stream, limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(value) = row {
            out.push(value);
        }
    }
    out.reverse();
    Ok(out)
}

fn push_chunk_line(
    chunks: &mut Vec<String>,
    current: &mut String,
    line: &str,
    chunk_chars: usize,
    max_chunks: usize,
    truncated: &mut bool,
) -> bool {
    let cleaned = line.trim_end();
    if cleaned.is_empty() {
        return true;
    }
    let line_with_newline = if current.is_empty() {
        cleaned.to_string()
    } else {
        format!("\n{cleaned}")
    };
    let projected = current.chars().count() + line_with_newline.chars().count();
    if projected > chunk_chars {
        if current.trim().is_empty() {
            current.push_str(&truncate_line(cleaned, chunk_chars.saturating_sub(4)));
            return true;
        }
        chunks.push(std::mem::take(current));
        if chunks.len() >= max_chunks {
            *truncated = true;
            if let Some(last) = chunks.last_mut() {
                let marker = "\n... [truncated: additional older context omitted] ...";
                if last.chars().count() + marker.chars().count() <= chunk_chars {
                    last.push_str(marker);
                }
            }
            return false;
        }
        current.push_str(cleaned);
        return true;
    }
    current.push_str(&line_with_newline);
    true
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

    refresh_context_artifacts(&conn, session_id)?;

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
    ts_ms: Option<i64>,
    stream: String,
    line_text: String,
}

#[derive(Debug)]
struct FileEditRecord {
    id: i64,
    ts_ms: Option<i64>,
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
            SELECT id, ts_ms, stream, line_text
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
                ts_ms: row.get::<_, Option<i64>>(1)?,
                stream: row.get::<_, String>(2)?,
                line_text: row.get::<_, String>(3)?,
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
            SELECT id, ts_ms, path, write_mode
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
                id: row.get::<_, i64>(0)?,
                ts_ms: row.get::<_, Option<i64>>(1)?,
                path: row.get::<_, Option<String>>(2)?,
                write_mode: row.get::<_, Option<String>>(3)?,
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

fn load_artifacts(
    conn: &Connection,
    session_id: &str,
    limit: i64,
) -> Result<Vec<ArtifactRecord>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT id, artifact_type, text, ts_ms
            FROM context_artifacts
            WHERE session_id = ?1
            ORDER BY updated_at_ms DESC, id DESC
            LIMIT ?2
            ",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![session_id, limit], |row| {
            Ok(ArtifactRecord {
                id: row.get::<_, i64>(0)?,
                artifact_type: row.get::<_, String>(1)?,
                text: row.get::<_, String>(2)?,
                ts_ms: row.get::<_, Option<i64>>(3)?,
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

fn refresh_context_artifacts(conn: &Connection, session_id: &str) -> Result<(), String> {
    let events = load_events(conn, session_id, 600)?;
    let commands = load_command_lines(conn, session_id, 120)?;
    let edits = load_file_edits(conn, session_id, 120)?;
    let command_failures = correlate_command_failures(&events, 80);
    let tool_failures = correlate_tool_failures(&events, 80);

    for failure in &command_failures {
        let text = if let Some(cmd) = failure.command.as_deref() {
            format!("Command failed: {cmd} (exit {})", failure.exit)
        } else {
            format!("Command failed: {}", failure.text)
        };
        upsert_context_artifact(conn, session_id, "blocker", &text, failure.ts_ms)?;
    }
    for failure in &tool_failures {
        let text = format!(
            "Tool failure: {} ({})",
            failure.tool,
            truncate_line(&failure.error, 160)
        );
        upsert_context_artifact(conn, session_id, "blocker", &text, failure.ts_ms)?;
    }
    for edit in edits.iter().take(30) {
        if let Some(path) = edit.path.as_deref() {
            let text = format!(
                "Edited {} ({})",
                path,
                edit.write_mode.as_deref().unwrap_or("unknown")
            );
            upsert_context_artifact(conn, session_id, "edit", &text, edit.ts_ms)?;
        }
    }
    for cmd in commands.iter().take(40) {
        let lowered = cmd.to_lowercase();
        if lowered.contains("todo")
            || lowered.contains("next")
            || lowered.contains("fixme")
            || lowered.contains("later")
        {
            let text = format!("TODO signal in command: {}", truncate_line(cmd, 180));
            upsert_context_artifact(conn, session_id, "todo", &text, None)?;
        }
        if lowered.contains("decide")
            || lowered.contains("chosen")
            || lowered.contains("use ")
            || lowered.contains("switch to")
        {
            let text = format!("Decision signal in command: {}", truncate_line(cmd, 180));
            upsert_context_artifact(conn, session_id, "decision", &text, None)?;
        }
    }
    Ok(())
}

fn upsert_context_artifact(
    conn: &Connection,
    session_id: &str,
    artifact_type: &str,
    text: &str,
    ts_ms: Option<i64>,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let now = now_ms_i64();
    conn.execute(
        "
        INSERT INTO context_artifacts (
            session_id, artifact_type, text, ts_ms, created_at_ms, updated_at_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
        ON CONFLICT(session_id, artifact_type, text) DO UPDATE SET
            ts_ms = COALESCE(excluded.ts_ms, context_artifacts.ts_ms),
            updated_at_ms = excluded.updated_at_ms
        ",
        params![session_id, artifact_type, text, ts_ms, now],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
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

fn detect_query_intent(query: &str) -> QueryIntent {
    let q = query.to_lowercase();
    if [
        "wrong",
        "error",
        "fail",
        "failed",
        "failure",
        "traceback",
        "panic",
        "debug",
        "fix",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return QueryIntent::Debug;
    }
    if [
        "resume",
        "continue",
        "pick up",
        "where were we",
        "what happened",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return QueryIntent::Resume;
    }
    if [
        "what changed",
        "which files",
        "edited",
        "diff",
        "change list",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return QueryIntent::WhatChanged;
    }
    if ["status", "state", "summary", "what is going on", "overview"]
        .iter()
        .any(|needle| q.contains(needle))
    {
        return QueryIntent::Status;
    }
    QueryIntent::General
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
            let _ = refresh_context_artifacts(conn, session_id);
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
            ..BuildContextOptions::default()
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
            ..BuildContextOptions::default()
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

    #[test]
    fn context_bundle_detects_what_changed_intent() {
        let root = test_root("intent_changed");
        let session_id = "sess_test_intent_changed";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":11,\"session_id\":\"sess_test_intent_changed\",\"kind\":\"tool.start\",\"data\":{\"tool\":\"fs.write\",\"args\":{\"path\":\"src/lib.rs\",\"append\":false,\"content\":\"pub fn add(){}\"}}}\n\
             {\"ts_ms\":12,\"session_id\":\"sess_test_intent_changed\",\"kind\":\"tool.start\",\"data\":{\"tool\":\"fs.write\",\"args\":{\"path\":\"README.md\",\"append\":true,\"content\":\"notes\"}}}\n",
        );
        write_file(&session_dir.join("stdin.log"), "git diff\n");

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "which files changed?",
            BuildContextOptions::default(),
        )
        .unwrap();
        assert_eq!(bundle.intent, "what-changed");
        assert!(!bundle.incidents.is_empty());
        assert!(!bundle.artifacts.is_empty());
        assert!(bundle.context_text.contains("Context artifacts"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_bundle_includes_explain_metadata() {
        let root = test_root("explain");
        let session_id = "sess_test_explain";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":21,\"session_id\":\"sess_test_explain\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"cargo test\"}}\n\
             {\"ts_ms\":22,\"session_id\":\"sess_test_explain\",\"kind\":\"command.end\",\"data\":{\"exit\":101}}\n",
        );
        write_file(
            &session_dir.join("stderr.log"),
            "error[E0308]: mismatched types\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "why did cargo test fail?",
            BuildContextOptions {
                explain: true,
                ..BuildContextOptions::default()
            },
        )
        .unwrap();
        let explain = bundle.explain.expect("expected explain metadata");
        assert_eq!(explain.intent, "debug");
        assert!(explain.selected_count > 0);
        assert!(explain.candidate_count >= explain.selected_count);
        assert!(explain.profile.category_caps.contains_key("incident"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_bundle_honors_intent_override() {
        let root = test_root("intent_override");
        let session_id = "sess_test_intent_override";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(&session_dir.join("stdin.log"), "echo status\n");
        write_file(&session_dir.join("stdout.log"), "all systems nominal\n");

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let bundle = build_relevant_context_bundle_with_options(
            &db_path,
            session_id,
            "what changed?",
            BuildContextOptions {
                intent_override: Some(QueryIntent::Status),
                ..BuildContextOptions::default()
            },
        )
        .unwrap();
        assert_eq!(bundle.intent, "status");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn llm_selection_corpus_includes_recent_streams_and_chunks() {
        let root = test_root("llm_corpus");
        let session_id = "sess_test_llm_corpus";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        write_file(
            &session_dir.join("events.jsonl"),
            "{\"ts_ms\":1,\"session_id\":\"sess_test_llm_corpus\",\"kind\":\"command.start\",\"data\":{\"cmd\":\"cargo test\"}}\n\
             {\"ts_ms\":2,\"session_id\":\"sess_test_llm_corpus\",\"kind\":\"command.end\",\"data\":{\"exit\":101}}\n",
        );
        write_file(
            &session_dir.join("stdin.log"),
            "echo one\necho two\necho three\n",
        );
        write_file(
            &session_dir.join("stdout.log"),
            "stdout one\nstdout two\nstdout three\n",
        );
        write_file(&session_dir.join("stderr.log"), "stderr one\nstderr two\n");

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let corpus = build_llm_selection_corpus(
            &db_path,
            session_id,
            BuildLlmSelectionCorpusOptions {
                chunk_chars: 140,
                max_chunks: 4,
                include_events: true,
            },
        )
        .unwrap();

        assert!(!corpus.old_context_chunks.is_empty());
        assert!(corpus.total_old_lines >= 5);
        assert!(corpus.last_stdout.join("\n").contains("stdout three"));
        assert!(corpus.last_stderr.join("\n").contains("stderr two"));
        assert!(corpus.last_stdin.join("\n").contains("echo three"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn llm_selection_corpus_marks_truncation() {
        let root = test_root("llm_corpus_trunc");
        let session_id = "sess_test_llm_corpus_trunc";
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        write_file(
            &session_dir.join("stdin.log"),
            "a\nb\nc\nd\ne\nf\ng\nh\ni\n",
        );

        let db_path = root.join("logs.sqlite");
        init(&db_path).unwrap();
        ingest_session(&root, &db_path, session_id).unwrap();

        let corpus = build_llm_selection_corpus(
            &db_path,
            session_id,
            BuildLlmSelectionCorpusOptions {
                chunk_chars: 32,
                max_chunks: 1,
                include_events: false,
            },
        )
        .unwrap();
        assert!(corpus.truncated);
        assert_eq!(corpus.old_context_chunks.len(), 1);
        assert!(
            corpus.old_context_chunks[0].contains("truncated")
                || corpus.old_context_chunks[0].chars().count() <= 64
        );

        let _ = fs::remove_dir_all(root);
    }
}
