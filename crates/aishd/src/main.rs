use anyhow::{bail, Result};
use axum::{
    extract::Path as AxumPath,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::response::sse::{Event, Sse};
use clap::Parser;
use aish_core::config::{self, Config, OpenAICompatConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use chrono::Utc;

#[derive(Parser)]
#[command(name = "aishd", version, about = "aish server daemon")]
struct Cli {
    /// Path to aish.json config
    #[arg(long, default_value = "~/.config/aish/aish.json")]
    config: String,
    /// Bind address (overrides config), e.g. 127.0.0.1:4096 or 4096
    #[arg(long)]
    bind: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let mut cfg = config::load(&cli.config)?;
    if let Some(bind) = cli.bind.as_deref() {
        apply_bind_override(&mut cfg, bind)?;
    }

    let addr = format!("{}:{}", cfg.server.hostname, cfg.server.port);
    let listener = TcpListener::bind(&addr).await?;
    println!("aishd listening on http://{addr}");

    let store_path = resolve_path(&cfg.logging.dir).join("sessions.jsonl");
    let store = load_store(&store_path).unwrap_or_default();
    let state = AppState {
        cfg,
        store: std::sync::Arc::new(std::sync::Mutex::new(store)),
        store_path,
        run_cancels: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/completions", post(completions))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/:id", get(get_session).patch(patch_session))
        .route("/v1/sessions/:id/agents", get(list_agents).post(create_agent))
        .route("/v1/agents/:id/run", post(run_agent))
        .route("/v1/agents/:id/run/stream", post(run_agent_stream))
        .route("/v1/agents/:id/subagents", post(subagents))
        .route("/v1/agents/:id/flow", post(run_flow))
        .route("/v1/agents/:id/flow/stream", post(run_flow_stream))
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/:id", get(get_run))
        .route("/v1/runs/:id/cancel", post(cancel_run))
        .route("/v1/diagnostics/tmux", get(diagnostics_tmux))
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/:name/call", post(call_tool))
        .with_state(state);

    axum::serve(listener, app).await?;
    Ok(())
}

fn apply_bind_override(cfg: &mut Config, bind: &str) -> Result<()> {
    if let Some((host, port_str)) = bind.rsplit_once(':') {
        if host.is_empty() {
            bail!("invalid bind override: {bind}");
        }
        cfg.server.hostname = host.to_string();
        cfg.server.port = port_str.parse()?;
        return Ok(());
    }

    cfg.server.port = bind.parse()?;
    Ok(())
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

#[derive(Serialize)]
struct Version {
    name: &'static str,
    version: &'static str,
}

async fn version() -> Json<Version> {
    Json(Version {
        name: "aishd",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Debug, Deserialize)]
struct CompletionRequest {
    prompt: Option<String>,
    messages: Option<Vec<ChatMessage>>,
    model: Option<String>,
    provider: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ToolInfo {
    name: &'static str,
    description: &'static str,
    args_schema: Value,
    policy: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    args: Value,
    session_id: Option<String>,
    agent_id: Option<String>,
    approved: Option<bool>,
    approval_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolCallResponse {
    result: Value,
    duration_ms: u128,
}

#[derive(Clone)]
struct AppState {
    cfg: Config,
    store: std::sync::Arc<std::sync::Mutex<Store>>,
    store_path: PathBuf,
    run_cancels: Arc<std::sync::Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
}

#[derive(Debug, Serialize)]
struct DiagnosticStep {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DiagnosticReport {
    ok: bool,
    steps: Vec<DiagnosticStep>,
}

#[derive(Debug, Deserialize)]
struct RunAgentRequest {
    prompt: String,
    provider: Option<String>,
    model: Option<String>,
    max_steps: Option<u32>,
    tools: Option<Vec<String>>,
    approved_tools: Option<Vec<String>>,
    approved: Option<bool>,
    approval_reason: Option<String>,
    context_editing: Option<ContextEditingConfig>,
}

#[derive(Debug, Serialize)]
struct RunAgentResponse {
    status: String,
    output: Option<String>,
    steps: u32,
    pending_tool_call: Option<ToolCall>,
    run_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ToolCall {
    tool: String,
    args: Value,
}

#[derive(Debug, Deserialize, Clone)]
struct ContextEditingConfig {
    clear_tool_uses: Option<ClearToolUses>,
    compact: Option<CompactConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct ClearToolUses {
    keep_last: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
struct CompactConfig {
    max_messages: Option<u32>,
}

async fn completions(
    State(state): State<AppState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let cfg = &state.cfg;
    let mut model = req.model.clone().unwrap_or_default();
    let mut provider_name = req.provider.clone();

    if provider_name.is_none() {
        if let Some(model_name) = req.model.as_ref() {
            if let Some(alias) = cfg.providers.model_aliases.get(model_name) {
                provider_name = Some(alias.provider.clone());
                model = alias.model.clone();
            }
        }
    }

    let provider = match resolve_provider(&cfg, provider_name.as_deref()) {
        Ok(provider) => provider,
        Err(resp) => return resp,
    };

    if provider.base_url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "provider base_url is required"})),
        )
            .into_response();
    }

    if model.trim().is_empty() {
        model = provider.model.clone();
    }
    if model.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "model is required"})),
        )
            .into_response();
    }

    let api_key = match resolve_api_key(provider, provider_name.as_deref()) {
        Ok(key) => key,
        Err(resp) => return resp,
    };
    let provider = provider.clone();
    let result =
        tokio::task::spawn_blocking(move || {
            call_openai_compat_completions(&provider, &api_key, &model, &req)
        })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err((status, value))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, Json(value)).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("join error: {err}")})),
        )
            .into_response(),
    }
}

async fn list_tools(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = &state.cfg;
    let infos = tool_defs()
        .into_iter()
        .map(|def| ToolInfo {
            name: def.name,
            description: def.description,
            args_schema: def.args_schema,
            policy: policy_for(&cfg, def.name).as_str().to_string(),
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(infos))
}

async fn call_tool(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<ToolCallRequest>,
) -> Response {
    let cfg = &state.cfg;
    let def = match tool_defs().into_iter().find(|d| d.name == name) {
        Some(def) => def,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "unknown tool"})),
            )
                .into_response();
        }
    };

    let policy = policy_for(&cfg, def.name);
    match policy {
        ToolPolicy::Deny => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "tool denied", "tool": def.name})),
            )
                .into_response();
        }
        ToolPolicy::Ask => {
            if req.approved != Some(true) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "approval_required",
                        "tool": def.name,
                        "policy": "ask"
                    })),
                )
                    .into_response();
            }
        }
        ToolPolicy::Allow => {}
    }

    let session_id = req.session_id.clone();
    let agent_id = req.agent_id.clone();
    let start = now_ms();
    let _ = log_tool_event(
        &cfg,
        session_id.as_deref(),
        agent_id.as_deref(),
        "tool.start",
        json!({
            "tool": def.name,
            "args": req.args,
            "approval_reason": req.approval_reason,
        }),
    );

    let result = match def.name {
        "shell" => run_shell(req.args),
        "fs.read" => run_fs_read(req.args),
        "fs.write" => run_fs_write(req.args),
        "fs.list" => run_fs_list(req.args),
        _ => Err((StatusCode::NOT_FOUND, json!({"error": "unknown tool"}))),
    };

    let duration_ms = now_ms().saturating_sub(start);
    match result {
        Ok(value) => {
            let _ = log_tool_event(
                &cfg,
                session_id.as_deref(),
                agent_id.as_deref(),
                "tool.end",
                json!({
                    "tool": def.name,
                    "ok": true,
                    "duration_ms": duration_ms,
                }),
            );
            (
                StatusCode::OK,
                Json(ToolCallResponse {
                    result: value,
                    duration_ms,
                }),
            )
                .into_response()
        }
        Err((status, err)) => {
            let _ = log_tool_event(
                &cfg,
                session_id.as_deref(),
                agent_id.as_deref(),
                "tool.end",
                json!({
                    "tool": def.name,
                    "ok": false,
                    "duration_ms": duration_ms,
                    "error": err,
                }),
            );
            (status, Json(err)).into_response()
        }
    }
}

async fn list_sessions(State(state): State<AppState>) -> impl IntoResponse {
    let store = state.store.lock().unwrap();
    let sessions = store.sessions.values().cloned().collect::<Vec<_>>();
    (StatusCode::OK, Json(sessions))
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    title: Option<String>,
    share_state: Option<String>,
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    let mut store = state.store.lock().unwrap();
    let id = store.next_id("sess");
    let now = now_ms();
    let session = Session {
        id: id.clone(),
        title: req.title.unwrap_or_else(|| "untitled".to_string()),
        created_at: now,
        updated_at: now,
        status: "idle".to_string(),
        share_state: req.share_state.unwrap_or_else(|| "manual".to_string()),
        tmux_session_name: None,
    };
    store.sessions.insert(id.clone(), session.clone());
    if let Err(err) = append_store_event(
        &state.store_path,
        StoreEvent::SessionUpsert(session.clone()),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(session)).into_response()
}

async fn get_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let store = state.store.lock().unwrap();
    match store.sessions.get(&id) {
        Some(session) => (StatusCode::OK, Json(session)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct PatchSessionRequest {
    title: Option<String>,
    share_state: Option<String>,
    status: Option<String>,
}

async fn patch_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<PatchSessionRequest>,
) -> Response {
    let mut store = state.store.lock().unwrap();
    let session = match store.sessions.get_mut(&id) {
        Some(session) => session,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            )
                .into_response();
        }
    };
    if let Some(title) = req.title {
        session.title = title;
    }
    if let Some(share_state) = req.share_state {
        session.share_state = share_state;
    }
    if let Some(status) = req.status {
        session.status = status;
    }
    session.updated_at = now_ms();
    let updated = session.clone();
    if let Err(err) = append_store_event(&state.store_path, StoreEvent::SessionUpsert(updated.clone())) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(updated)).into_response()
}

async fn list_agents(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let store = state.store.lock().unwrap();
    let agents = store
        .agents
        .values()
        .filter(|agent| agent.session_id == id)
        .cloned()
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(agents)).into_response()
}

async fn list_runs(State(state): State<AppState>) -> Response {
    let store = state.store.lock().unwrap();
    let runs = store.runs.values().cloned().collect::<Vec<_>>();
    (StatusCode::OK, Json(runs)).into_response()
}

async fn get_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let store = state.store.lock().unwrap();
    match store.runs.get(&id) {
        Some(run) => (StatusCode::OK, Json(run)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "run not found"})),
        )
            .into_response(),
    }
}

async fn cancel_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let cancel = {
        let mut cancels = state.run_cancels.lock().unwrap();
        cancels
            .entry(id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };
    cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

#[derive(Debug, Deserialize)]
struct CreateAgentRequest {
    parent_agent_id: Option<String>,
    model: Option<String>,
    worktree_mode: Option<String>,
    repo_path: Option<String>,
}

async fn create_agent(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
    Json(req): Json<CreateAgentRequest>,
) -> Response {
    match create_agent_internal(&state, &session_id, req) {
        Ok(agent) => (StatusCode::OK, Json(agent)).into_response(),
        Err(resp) => resp,
    }
}

fn create_agent_internal(
    state: &AppState,
    session_id: &str,
    req: CreateAgentRequest,
) -> Result<Agent, Response> {
    let mut store = state.store.lock().unwrap();
    if !store.sessions.contains_key(session_id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        )
            .into_response());
    }
    let id = store.next_id("agent");

    let parent_worktree = req
        .parent_agent_id
        .as_ref()
        .and_then(|parent_id| store.agents.get(parent_id))
        .and_then(|agent| agent.worktree.clone());

    let worktree = match resolve_worktree_mode(
        req.worktree_mode.as_deref(),
        req.repo_path.as_deref(),
        parent_worktree.as_deref(),
    ) {
        Ok(mode) => match mode {
            WorktreeMode::None => None,
            WorktreeMode::Inherit => parent_worktree.clone(),
            WorktreeMode::New(repo_path) => match create_worktree(
                &state.cfg,
                session_id,
                &id,
                &repo_path,
            ) {
                Ok(path) => Some(path),
                Err(err) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": err})),
                    )
                        .into_response());
                }
            },
        },
        Err(err) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": err})),
            )
                .into_response());
        }
    };

    let tmux_name = format!("{}-{}-{}", state.cfg.tmux.session_prefix, session_id, id);
    let tmux_cwd = worktree.as_deref();
    if let Err(err) = spawn_tmux_session(&tmux_name, tmux_cwd) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response());
    }

    let agent = Agent {
        id: id.clone(),
        session_id: session_id.to_string(),
        parent_agent_id: req.parent_agent_id,
        model: req.model,
        status: "idle".to_string(),
        tmux_session_name: Some(tmux_name),
        worktree,
    };
    store.agents.insert(id.clone(), agent.clone());
    if let Err(err) = append_store_event(&state.store_path, StoreEvent::AgentUpsert(agent.clone())) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response());
    }
    Ok(agent)
}

fn create_run(state: &AppState, session_id: &str, agent_id: &str, mode: &str) -> Run {
    let mut store = state.store.lock().unwrap();
    let id = store.next_id("run");
    let now = now_ms();
    let run = Run {
        id: id.clone(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        mode: mode.to_string(),
        status: "running".to_string(),
        started_at: now,
        ended_at: None,
    };
    store.runs.insert(id.clone(), run.clone());
    let _ = append_store_event(&state.store_path, StoreEvent::RunUpsert(run.clone()));
    run
}

fn update_run_status(state: &AppState, run_id: &str, status: &str) {
    let mut store = state.store.lock().unwrap();
    if let Some(run) = store.runs.get_mut(run_id) {
        run.status = status.to_string();
        if status != "running" {
            run.ended_at = Some(now_ms());
        }
        let updated = run.clone();
        let _ = append_store_event(&state.store_path, StoreEvent::RunUpsert(updated));
    }
}

fn cancel_token(state: &AppState, run_id: &str) -> Arc<AtomicBool> {
    let mut cancels = state.run_cancels.lock().unwrap();
    cancels
        .entry(run_id.to_string())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

#[derive(Debug, Deserialize)]
struct SubagentRequest {
    tasks: Vec<SubagentTask>,
    mode: Option<String>,
    max_concurrency: Option<u32>,
    approved_tools: Option<Vec<String>>,
    aggregate: Option<bool>,
    aggregate_prompt: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct SubagentTask {
    prompt: String,
    model: Option<String>,
    provider: Option<String>,
    tools: Option<Vec<String>>,
    repo_path: Option<String>,
    worktree_mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct SubagentResult {
    agent_id: String,
    status: String,
    output: Option<String>,
    error: Option<String>,
    run_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SubagentsResponse {
    results: Vec<SubagentResult>,
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlowRequest {
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    provider: Option<String>,
    model: Option<String>,
    max_concurrency: Option<u32>,
    tools: Option<Vec<String>>,
    approved_tools: Option<Vec<String>>,
    approved: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct FlowNode {
    id: String,
    kind: String,
    prompt: Option<String>,
    tool: Option<String>,
    args: Option<Value>,
    inputs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
struct FlowEdge {
    from: String,
    to: String,
}

#[derive(Debug, Serialize)]
struct FlowResponse {
    status: String,
    outputs: BTreeMap<String, Value>,
    run_id: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct RunEvent {
    r#type: String,
    run_id: String,
    session_id: String,
    agent_id: String,
    ts_ms: u128,
    payload: Value,
}

#[derive(Clone)]
struct RunContext {
    run_id: String,
    session_id: String,
    agent_id: String,
    sender: Option<mpsc::Sender<RunEvent>>,
    cancel: Arc<AtomicBool>,
}

impl RunContext {
    async fn emit(&self, kind: &str, payload: Value) -> Result<(), ()> {
        let sender = match &self.sender {
            Some(sender) => sender,
            None => return Ok(()),
        };
        let event = RunEvent {
            r#type: kind.to_string(),
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            ts_ms: now_ms(),
            payload,
        };
        sender.send(event).await.map_err(|_| ())
    }
}

async fn subagents(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
    Json(req): Json<SubagentRequest>,
) -> Response {
    let (session_id, has_parent, lead_model, lead_provider) = {
        let store = state.store.lock().unwrap();
        let agent = match store.agents.get(&agent_id) {
            Some(agent) => agent,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response();
            }
        };
        (
            agent.session_id.clone(),
            agent.parent_agent_id.is_some(),
            agent.model.clone(),
            None::<String>,
        )
    };

    if has_parent {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "subagents cannot spawn subagents"})),
        )
            .into_response();
    }

    let mode = req.mode.unwrap_or_else(|| "parallel".to_string());
    let max_concurrency = req.max_concurrency.unwrap_or(4).max(1) as usize;
    let approved_tools = req.approved_tools.clone();
    let tasks = req.tasks.clone();

    if tasks.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "tasks are required"})),
        )
            .into_response();
    }

    if mode == "sequential" {
        let mut results = Vec::new();
        for task in tasks {
            let result = run_subagent_task(
                state.clone(),
                &session_id,
                &agent_id,
                task,
                approved_tools.clone(),
            )
            .await;
            results.push(result);
        }
        let summary = if req.aggregate == Some(true) {
            aggregate_subagent_results(
                &state.cfg,
                lead_provider.clone(),
                lead_model.clone(),
                req.aggregate_prompt.clone(),
                &results,
            )
            .ok()
        } else {
            None
        };
        return (
            StatusCode::OK,
            Json(SubagentsResponse { results, summary }),
        )
            .into_response();
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency));
    let mut handles = Vec::new();
    for task in tasks {
        let state = state.clone();
        let session_id = session_id.clone();
        let parent_id = agent_id.clone();
        let approved_tools = approved_tools.clone();
        let semaphore = semaphore.clone();
        let handle = tokio::spawn(async move {
            let _permit = semaphore.acquire().await.ok();
            run_subagent_task(state, &session_id, &parent_id, task, approved_tools).await
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(err) => results.push(SubagentResult {
                agent_id: "unknown".to_string(),
                status: "error".to_string(),
                output: None,
                error: Some(format!("join error: {err}")),
                run_id: None,
            }),
        }
    }
    let summary = if req.aggregate == Some(true) {
        aggregate_subagent_results(
            &state.cfg,
            lead_provider,
            lead_model,
            req.aggregate_prompt.clone(),
            &results,
        )
        .ok()
    } else {
        None
    };

    (
        StatusCode::OK,
        Json(SubagentsResponse { results, summary }),
    )
        .into_response()
}

async fn run_subagent_task(
    state: AppState,
    session_id: &str,
    parent_id: &str,
    task: SubagentTask,
    approved_tools: Option<Vec<String>>,
) -> SubagentResult {
    let create_req = CreateAgentRequest {
        parent_agent_id: Some(parent_id.to_string()),
        model: task.model.clone(),
        worktree_mode: task.worktree_mode.clone().or(Some("inherit".to_string())),
        repo_path: task.repo_path.clone(),
    };

    let agent = match create_agent_internal(&state, session_id, create_req) {
        Ok(agent) => agent,
        Err(_) => {
            return SubagentResult {
                agent_id: "unknown".to_string(),
                status: "error".to_string(),
                output: None,
                error: Some("failed to create agent".to_string()),
                run_id: None,
            }
        }
    };

    let agent_id = agent.id.clone();
    if agent_id == "unknown" {
        return SubagentResult {
            agent_id,
            status: "error".to_string(),
            output: None,
            error: Some("failed to create agent".to_string()),
            run_id: None,
        };
    }

    let run = create_run(&state, session_id, &agent_id, "subagent");
    let cancel = cancel_token(&state, &run.id);
    let ctx = RunContext {
        run_id: run.id.clone(),
        session_id: session_id.to_string(),
        agent_id: agent_id.clone(),
        sender: None,
        cancel,
    };

    let run_req = RunAgentRequest {
        prompt: task.prompt,
        provider: task.provider,
        model: task.model,
        max_steps: Some(6),
        tools: task.tools,
        approved_tools: approved_tools.clone(),
        approved: Some(false),
        approval_reason: Some("subagent preapproved".to_string()),
        context_editing: Some(ContextEditingConfig {
            clear_tool_uses: Some(ClearToolUses { keep_last: Some(3) }),
            compact: None,
        }),
    };

    let response = run_agent_internal(&state, &agent_id, run_req, true, ctx.clone()).await;
    match response {
        Ok(resp) => {
            update_run_status(&state, &run.id, &resp.status);
            SubagentResult {
                agent_id,
                status: resp.status,
                output: resp.output,
                error: None,
                run_id: resp.run_id.or(Some(run.id)),
            }
        }
        Err(_) => {
            update_run_status(&state, &run.id, "error");
            SubagentResult {
                agent_id,
                status: "error".to_string(),
                output: None,
                error: Some("subagent run failed".to_string()),
                run_id: Some(run.id),
            }
        }
    }
}

async fn run_flow(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
    Json(req): Json<FlowRequest>,
) -> Response {
    let (session_id, lead_model) = {
        let store = state.store.lock().unwrap();
        let agent = match store.agents.get(&agent_id) {
            Some(agent) => agent,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response();
            }
        };
        (agent.session_id.clone(), agent.model.clone())
    };

    let run = create_run(&state, &session_id, &agent_id, "flow");
    let cancel = cancel_token(&state, &run.id);
    let ctx = RunContext {
        run_id: run.id.clone(),
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        sender: None,
        cancel,
    };

    let outputs = match execute_flow(
        &state,
        &session_id,
        &agent_id,
        req,
        lead_model,
        Some(ctx.clone()),
    )
    .await
    {
        Ok(outputs) => outputs,
        Err(resp) => {
            if ctx.cancel.load(Ordering::SeqCst) {
                update_run_status(&state, &run.id, "canceled");
            } else {
                update_run_status(&state, &run.id, "error");
            }
            return resp;
        }
    };
    update_run_status(&state, &run.id, "completed");

    (StatusCode::OK, Json(FlowResponse { status: "completed".to_string(), outputs, run_id: Some(run.id) }))
        .into_response()
}

async fn run_flow_stream(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
    Json(req): Json<FlowRequest>,
) -> Response {
    let (session_id, lead_model) = {
        let store = state.store.lock().unwrap();
        let agent = match store.agents.get(&agent_id) {
            Some(agent) => agent,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response();
            }
        };
        (agent.session_id.clone(), agent.model.clone())
    };

    let run = create_run(&state, &session_id, &agent_id, "flow");
    let cancel = cancel_token(&state, &run.id);
    let (tx, rx) = mpsc::channel(100);
    let ctx = RunContext {
        run_id: run.id.clone(),
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        sender: Some(tx.clone()),
        cancel,
    };

    tokio::spawn({
        let state = state.clone();
        async move {
            let result = execute_flow(
                &state,
                &session_id,
                &agent_id,
                req,
                lead_model,
                Some(ctx.clone()),
            )
            .await;
            match result {
                Ok(_) => {
                    update_run_status(&state, &run.id, "completed");
                    let _ = ctx.emit("run.end", json!({"status": "completed"})).await;
                }
                Err(_) => {
                    if ctx.cancel.load(Ordering::SeqCst) {
                        update_run_status(&state, &run.id, "canceled");
                        let _ = ctx.emit("run.canceled", json!({})).await;
                    } else {
                        update_run_status(&state, &run.id, "error");
                        let _ = ctx.emit("run.error", json!({"error": "flow failed"})).await;
                    }
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|event| {
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Ok::<Event, std::convert::Infallible>(Event::default().event(event.r#type).data(data))
    });
    Sse::new(stream).into_response()
}

async fn run_agent(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
    Json(req): Json<RunAgentRequest>,
) -> Response {
    let session_id = {
        let store = state.store.lock().unwrap();
        match store.agents.get(&agent_id) {
            Some(agent) => agent.session_id.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response();
            }
        }
    };
    let run = create_run(&state, &session_id, &agent_id, "agent");
    let cancel = cancel_token(&state, &run.id);
    let ctx = RunContext {
        run_id: run.id.clone(),
        session_id,
        agent_id: agent_id.clone(),
        sender: None,
        cancel,
    };

    match run_agent_internal(&state, &agent_id, req, false, ctx).await {
        Ok(resp) => {
            update_run_status(&state, &run.id, &resp.status);
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(err) => {
            update_run_status(&state, &run.id, "error");
            err
        }
    }
}

async fn run_agent_stream(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
    Json(req): Json<RunAgentRequest>,
) -> Response {
    let session_id = {
        let store = state.store.lock().unwrap();
        match store.agents.get(&agent_id) {
            Some(agent) => agent.session_id.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let run = create_run(&state, &session_id, &agent_id, "agent");
    let cancel = cancel_token(&state, &run.id);
    let (tx, rx) = mpsc::channel(100);
    let ctx = RunContext {
        run_id: run.id.clone(),
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        sender: Some(tx.clone()),
        cancel,
    };

    tokio::spawn({
        let state = state.clone();
        async move {
            let result = run_agent_internal(&state, &agent_id, req, false, ctx.clone()).await;
            match result {
                Ok(resp) => {
                    update_run_status(&state, &run.id, &resp.status);
                    let _ = ctx.emit("run.end", json!({"status": resp.status, "steps": resp.steps})).await;
                }
                Err(_) => {
                    update_run_status(&state, &run.id, "error");
                    let _ = ctx.emit("run.error", json!({"error": "run failed"})).await;
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|event| {
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Ok::<Event, std::convert::Infallible>(Event::default().event(event.r#type).data(data))
    });
    Sse::new(stream).into_response()
}

async fn run_agent_internal(
    state: &AppState,
    agent_id: &str,
    mut req: RunAgentRequest,
    deny_on_missing_approval: bool,
    ctx: RunContext,
) -> Result<RunAgentResponse, Response> {
    let (session_id, model_default) = {
        let mut store = state.store.lock().unwrap();
        let agent = match store.agents.get_mut(agent_id) {
            Some(agent) => agent,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                )
                    .into_response());
            }
        };
        agent.status = "running".to_string();
        let updated = agent.clone();
        if let Err(err) =
            append_store_event(&state.store_path, StoreEvent::AgentUpsert(updated.clone()))
        {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string()})),
            )
                .into_response());
        }
        (agent.session_id.clone(), agent.model.clone())
    };

    if req.context_editing.is_none() {
        req.context_editing = Some(ContextEditingConfig {
            clear_tool_uses: Some(ClearToolUses { keep_last: Some(3) }),
            compact: None,
        });
    }

    let max_steps = req.max_steps.unwrap_or(8);
    let mut messages = Vec::new();
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: tool_system_prompt(),
    });
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: req.prompt.clone(),
    });

    let mut steps = 0u32;
    let mut output: Option<String> = None;
    let mut pending: Option<ToolCall> = None;

    ctx.emit("run.start", json!({"steps": max_steps})).await.ok();

    while steps < max_steps {
        if ctx.cancel.load(Ordering::SeqCst) {
            ctx.emit("run.canceled", json!({})).await.ok();
            update_run_status(state, &ctx.run_id, "canceled");
            return Ok(RunAgentResponse {
                status: "canceled".to_string(),
                output,
                steps,
                pending_tool_call: None,
                run_id: Some(ctx.run_id.clone()),
            });
        }
        steps += 1;
        let provider = req.provider.clone();
        let model = req.model.clone().or(model_default.clone());
        let view = apply_context_editing(
            &state.cfg,
            provider.clone(),
            model.clone(),
            &messages,
            req.context_editing.clone(),
        )
        .map_err(|err| {
            set_agent_status(state, agent_id, "error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err})),
            )
                .into_response()
        })?;

        ctx.emit("message.start", json!({"step": steps})).await.ok();
        let completion = call_llm_with_messages(
            &state.cfg,
            provider.clone(),
            model.clone(),
            &view,
        )
        .map_err(|err| {
            set_agent_status(state, agent_id, "error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err})),
            )
                .into_response()
        })?;

        messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: completion.clone(),
        });
        ctx.emit("message.end", json!({"step": steps})).await.ok();

        let tool_calls = parse_tool_calls(&completion);
        if tool_calls.is_empty() {
            output = Some(completion);
            break;
        }

        for call in tool_calls {
            if let Some(allowed) = &req.tools {
                if !allowed.iter().any(|name| name == &call.tool) {
                    set_agent_status(state, agent_id, "error");
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": "tool not allowed", "tool": call.tool})),
                    )
                        .into_response());
                }
            }

            ctx.emit("tool.start", json!({"tool": call.tool})).await.ok();
            match execute_tool(
                &state.cfg,
                &call.tool,
                call.args.clone(),
                Some(&session_id),
                Some(agent_id),
                req.approved == Some(true),
                req.approved_tools.clone(),
                deny_on_missing_approval,
                req.approval_reason.clone(),
            ) {
                Ok(result) => {
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: format!("Tool result for {}:\n{}", call.tool, result),
                    });
                    ctx.emit("tool.end", json!({"tool": call.tool, "ok": true})).await.ok();
                }
                Err(ToolExecError::ApprovalRequired) => {
                    ctx.emit("tool.end", json!({"tool": call.tool, "ok": false, "error": "approval_required"})).await.ok();
                    pending = Some(call);
                    break;
                }
                Err(ToolExecError::Failed(status, err)) => {
                    ctx.emit("tool.end", json!({"tool": call.tool, "ok": false})).await.ok();
                    set_agent_status(state, agent_id, "error");
                    return Err((status, Json(err)).into_response());
                }
            }
        }

        if pending.is_some() {
            break;
        }
    }

    set_agent_status(state, agent_id, "idle");

    if let Some(pending) = pending {
        return Ok(RunAgentResponse {
            status: "approval_required".to_string(),
            output,
            steps,
            pending_tool_call: Some(pending),
            run_id: Some(ctx.run_id.clone()),
        });
    }

    Ok(RunAgentResponse {
        status: "completed".to_string(),
        output,
        steps,
        pending_tool_call: None,
        run_id: Some(ctx.run_id.clone()),
    })
}

async fn diagnostics_tmux() -> Response {
    let mut steps = Vec::new();
    let mut ok = true;

    match run_command_output("tmux", &["-V"]) {
        Ok(output) => steps.push(DiagnosticStep {
            name: "version",
            ok: true,
            detail: output.trim().to_string(),
        }),
        Err(err) => {
            steps.push(DiagnosticStep {
                name: "version",
                ok: false,
                detail: err,
            });
            ok = false;
        }
    }

    let session_name = format!("aish-diag-{}", now_ms());
    if ok {
        match run_command_status("tmux", &["new-session", "-d", "-s", &session_name]) {
            Ok(()) => steps.push(DiagnosticStep {
                name: "create_session",
                ok: true,
                detail: session_name.clone(),
            }),
            Err(err) => {
                steps.push(DiagnosticStep {
                    name: "create_session",
                    ok: false,
                    detail: err,
                });
                ok = false;
            }
        }
    }

    if ok {
        match run_command_output("tmux", &["list-sessions"]) {
            Ok(output) => {
                let found = output.lines().any(|line| line.contains(&session_name));
                steps.push(DiagnosticStep {
                    name: "list_sessions",
                    ok: found,
                    detail: output.trim().to_string(),
                });
                if !found {
                    ok = false;
                }
            }
            Err(err) => {
                steps.push(DiagnosticStep {
                    name: "list_sessions",
                    ok: false,
                    detail: err,
                });
                ok = false;
            }
        }
    }

    let kill_result = run_command_status("tmux", &["kill-session", "-t", &session_name]);
    steps.push(DiagnosticStep {
        name: "cleanup",
        ok: kill_result.is_ok(),
        detail: kill_result.err().unwrap_or_else(|| "ok".to_string()),
    });

    (
        StatusCode::OK,
        Json(DiagnosticReport { ok, steps }),
    )
        .into_response()
}

fn call_openai_compat_completions(
    provider: &OpenAICompatConfig,
    api_key: &str,
    model: &str,
    req: &CompletionRequest,
) -> Result<Value, (u16, Value)> {
    let url = if provider.completions_path.starts_with("http://")
        || provider.completions_path.starts_with("https://")
    {
        provider.completions_path.clone()
    } else {
        let base = provider.base_url.trim_end_matches('/');
        let path = if provider.completions_path.starts_with('/') {
            provider.completions_path.clone()
        } else {
            format!("/{}", provider.completions_path)
        };
        format!("{base}{path}")
    };

    let is_chat = provider.completions_path.contains("chat/completions");
    let mut body = json!({
        "model": model,
    });
    if is_chat {
        let messages = if let Some(messages) = &req.messages {
            messages.clone()
        } else if let Some(prompt) = req.prompt.as_deref() {
            if prompt.trim().is_empty() {
                return Err((400, json!({"error": "prompt or messages required"})));
            }
            vec![ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }]
        } else {
            return Err((400, json!({"error": "prompt or messages required"})));
        };
        body["messages"] = json!(messages);
    } else {
        let prompt = req.prompt.as_deref().unwrap_or_default();
        if prompt.trim().is_empty() {
            return Err((400, json!({"error": "prompt is required"})));
        }
        body["prompt"] = json!(prompt);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = req.top_p {
        body["top_p"] = json!(top_p);
    }
    if let Some(stop) = &req.stop {
        body["stop"] = json!(stop);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());

    match response {
        Ok(resp) => {
            let text = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Ok(parsed)
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Err((status, parsed))
        }
        Err(err) => Err((502, json!({ "error": err.to_string() }))),
    }
}

fn resolve_provider<'a>(
    cfg: &'a Config,
    provider_name: Option<&str>,
) -> Result<&'a OpenAICompatConfig, Response> {
    if let Some(name) = provider_name {
        if name == "default" {
            return cfg
                .providers
                .openai_compat
                .as_ref()
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "default provider not configured"})),
                    )
                        .into_response()
                });
        }
        return cfg
            .providers
            .openai_compat_profiles
            .get(name)
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("provider '{name}' not found")})),
                )
                    .into_response()
            });
    }

    cfg.providers
        .openai_compat
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "openai_compat provider not configured"})),
            )
                .into_response()
        })
}

fn resolve_api_key(
    provider: &OpenAICompatConfig,
    provider_name: Option<&str>,
) -> Result<String, Response> {
    if !provider.api_key.trim().is_empty() {
        return Ok(provider.api_key.clone());
    }
    if let Some(env_key) = provider.api_key_env.as_deref() {
        if let Ok(value) = env::var(env_key) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }

    let fallback_env = provider_name
        .map(|name| format!("{}_API_KEY", name.to_uppercase()))
        .unwrap_or_default();
    if !fallback_env.is_empty() {
        if let Ok(value) = env::var(&fallback_env) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }

    Err((
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "api key not configured"})),
    )
        .into_response())
}

fn call_llm_with_messages(
    cfg: &Config,
    provider_name: Option<String>,
    model: Option<String>,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let provider_name_ref = provider_name.as_deref();
    let provider = resolve_provider(cfg, provider_name_ref)
        .map_err(|_| "provider not configured".to_string())?;
    if provider.base_url.trim().is_empty() {
        return Err("provider base_url is required".to_string());
    }
    let model = model.unwrap_or_else(|| provider.model.clone());
    if model.trim().is_empty() {
        return Err("model is required".to_string());
    }
    let api_key = resolve_api_key(provider, provider_name_ref)
        .map_err(|_| "api key not configured".to_string())?;

    let req = CompletionRequest {
        prompt: None,
        messages: Some(messages.to_vec()),
        model: Some(model.clone()),
        provider: provider_name,
        max_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
    };
    let response = call_openai_compat_completions(provider, &api_key, &model, &req)
        .map_err(|(_, err)| err.to_string())?;
    extract_completion_text(&response)
        .ok_or_else(|| "no completion text returned".to_string())
}

fn apply_context_editing(
    cfg: &Config,
    provider: Option<String>,
    model: Option<String>,
    messages: &[ChatMessage],
    config: Option<ContextEditingConfig>,
) -> Result<Vec<ChatMessage>, String> {
    let mut view = messages.to_vec();
    let config = config.unwrap_or(ContextEditingConfig {
        clear_tool_uses: Some(ClearToolUses { keep_last: Some(3) }),
        compact: None,
    });

    if let Some(compact) = config.compact {
        view = compact_messages(cfg, provider.clone(), model.clone(), &view, compact)?;
    }
    if let Some(clear) = config.clear_tool_uses {
        let keep = clear.keep_last.unwrap_or(3);
        view = clear_tool_uses(&view, keep);
    }
    Ok(view)
}

fn clear_tool_uses(messages: &[ChatMessage], keep_last: u32) -> Vec<ChatMessage> {
    let mut tool_indices = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        if msg.role == "user" && msg.content.starts_with("Tool result for ") {
            tool_indices.push(idx);
        }
    }
    if keep_last as usize >= tool_indices.len() {
        return messages.to_vec();
    }
    let keep_set: std::collections::HashSet<usize> = tool_indices
        .iter()
        .rev()
        .take(keep_last as usize)
        .cloned()
        .collect();
    messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if msg.role == "user" && msg.content.starts_with("Tool result for ") {
                if keep_set.contains(&idx) {
                    Some(msg.clone())
                } else {
                    None
                }
            } else {
                Some(msg.clone())
            }
        })
        .collect()
}

fn compact_messages(
    cfg: &Config,
    provider: Option<String>,
    model: Option<String>,
    messages: &[ChatMessage],
    compact: CompactConfig,
) -> Result<Vec<ChatMessage>, String> {
    let max_messages = compact.max_messages.unwrap_or(30) as usize;
    if messages.len() <= max_messages {
        return Ok(messages.to_vec());
    }
    let keep_tail = std::cmp::max(4, max_messages / 2);
    if messages.len() <= keep_tail {
        return Ok(messages.to_vec());
    }
    let split_idx = messages.len() - keep_tail;
    let (head, tail) = messages.split_at(split_idx);
    let summary = summarize_messages(cfg, provider, model, head)?;
    let mut out = Vec::new();
    out.push(ChatMessage {
        role: "system".to_string(),
        content: format!("Summary of earlier context:\n{}", summary),
    });
    out.extend_from_slice(tail);
    Ok(out)
}

fn summarize_messages(
    cfg: &Config,
    provider: Option<String>,
    model: Option<String>,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let mut summary_prompt = String::from("Summarize the following conversation for future context. Keep it concise and preserve key decisions, facts, and TODOs.\n\n");
    for msg in messages {
        summary_prompt.push_str(&format!("[{}] {}\n", msg.role, msg.content));
    }
    let summary_messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: "You are a summarizer.".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: summary_prompt,
        },
    ];
    call_llm_with_messages(cfg, provider, model, &summary_messages)
}

fn aggregate_subagent_results(
    cfg: &Config,
    provider: Option<String>,
    model: Option<String>,
    prompt: Option<String>,
    results: &[SubagentResult],
) -> Result<String, String> {
    let mut content = String::new();
    for result in results {
        content.push_str(&format!(
            "Agent {} status={}:\n{}\n\n",
            result.agent_id,
            result.status,
            result.output.clone().unwrap_or_else(|| "<no output>".to_string())
        ));
    }
    let user_prompt = prompt.unwrap_or_else(|| "Aggregate the following subagent outputs into a concise synthesis with key findings and action items.".to_string());
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: "You are a synthesis agent.".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!("{}\n\n{}", user_prompt, content),
        },
    ];
    call_llm_with_messages(cfg, provider, model, &messages)
}

async fn execute_flow(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    req: FlowRequest,
    lead_model: Option<String>,
    ctx: Option<RunContext>,
) -> Result<BTreeMap<String, Value>, Response> {
    let mut node_map: BTreeMap<String, FlowNode> = BTreeMap::new();
    for node in req.nodes {
        node_map.insert(node.id.clone(), node);
    }
    if node_map.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "flow nodes required"})),
        )
            .into_response());
    }

    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, node) in node_map.iter() {
        if let Some(inputs) = &node.inputs {
            deps.insert(id.clone(), inputs.clone());
        } else {
            deps.insert(id.clone(), Vec::new());
        }
    }
    for edge in &req.edges {
        deps.entry(edge.to.clone())
            .or_default()
            .push(edge.from.clone());
    }

    let mut outputs: BTreeMap<String, Value> = BTreeMap::new();
    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let max_concurrency = req.max_concurrency.unwrap_or(4).max(1) as usize;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency));

    loop {
        let ready: Vec<String> = deps
            .iter()
            .filter(|(id, _)| !completed.contains(*id))
            .filter(|(_, deps)| deps.iter().all(|d| completed.contains(d)))
            .map(|(id, _)| id.clone())
            .collect();

        if ready.is_empty() {
            break;
        }

        let mut handles = Vec::new();
        for node_id in ready {
            let node = node_map.get(&node_id).cloned().unwrap();
            let state = state.clone();
            let semaphore = semaphore.clone();
            let outputs_snapshot = outputs.clone();
            let req_clone = FlowRequest {
                nodes: Vec::new(),
                edges: Vec::new(),
                provider: req.provider.clone(),
                model: req.model.clone(),
                max_concurrency: req.max_concurrency,
                tools: req.tools.clone(),
                approved_tools: req.approved_tools.clone(),
                approved: req.approved,
            };
            let lead_model = lead_model.clone();
            let agent_id = agent_id.to_string();
            let session_id = session_id.to_string();
            let ctx_clone = ctx.clone();

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.ok();
                let result = run_flow_node(
                    &state,
                    &session_id,
                    &agent_id,
                    &req_clone,
                    lead_model,
                    node,
                    outputs_snapshot,
                    ctx_clone,
                )
                .await;
                (node_id, result)
            });
            handles.push(handle);
        }

        for handle in handles {
            match handle.await {
                Ok((node_id, Ok(value))) => {
                    outputs.insert(node_id.clone(), value);
                    completed.insert(node_id);
                }
                Ok((_, Err(resp))) => return Err(resp),
                Err(err) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("flow join error: {err}")})),
                    )
                        .into_response())
                }
            }
        }
    }

    if completed.len() != node_map.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "flow has unresolved dependencies"})),
        )
            .into_response());
    }

    Ok(outputs)
}

async fn run_flow_node(
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    req: &FlowRequest,
    lead_model: Option<String>,
    node: FlowNode,
    outputs: BTreeMap<String, Value>,
    ctx: Option<RunContext>,
) -> Result<Value, Response> {
    if let Some(ctx) = &ctx {
        if ctx.cancel.load(Ordering::SeqCst) {
            update_run_status(state, &ctx.run_id, "canceled");
            let _ = ctx.emit("run.canceled", json!({})).await;
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "run canceled"})),
            )
                .into_response());
        }
        let _ = ctx
            .emit(
                "flow.node.start",
                json!({"node": node.id, "kind": node.kind}),
            )
            .await;
    }
    let kind = node.kind.to_lowercase();
    match kind.as_str() {
        "llm" => {
            let mut prompt = node.prompt.unwrap_or_default();
            if let Some(inputs) = node.inputs {
                for input in inputs {
                    if let Some(value) = outputs.get(&input) {
                        prompt.push_str(&format!("\n\n[{}]\n{}", input, value));
                    }
                }
            }
            let messages = vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "You are a flow node.".to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: prompt,
                },
            ];
            let result = call_llm_with_messages(
                &state.cfg,
                req.provider.clone(),
                req.model.clone().or(lead_model),
                &messages,
            )
            .map(|text| Value::String(text))
            .map_err(|err| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": err})),
                )
                    .into_response()
            })?;
            if let Some(ctx) = &ctx {
                let _ = ctx
                    .emit("flow.node.end", json!({"node": node.id, "ok": true}))
                    .await;
            }
            Ok(result)
        }
        "tool" => {
            let tool = node.tool.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "tool node requires tool"})),
                )
                    .into_response()
            })?;
            if let Some(allowed) = &req.tools {
                if !allowed.iter().any(|name| name == &tool) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": "tool not allowed", "tool": tool})),
                    )
                        .into_response());
                }
            }
            let args = node.args.unwrap_or(Value::Null);
            let args = render_template(args, &outputs);
            if let Some(ctx) = &ctx {
                let _ = ctx.emit("tool.start", json!({"tool": tool})).await;
            }
            let result = execute_tool(
                &state.cfg,
                &tool,
                args,
                Some(session_id),
                Some(agent_id),
                req.approved == Some(true),
                req.approved_tools.clone(),
                false,
                Some("flow preapproved".to_string()),
            )
            .map_err(|err| match err {
                ToolExecError::ApprovalRequired => (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "approval_required", "tool": tool})),
                )
                    .into_response(),
                ToolExecError::Failed(status, value) => (status, Json(value)).into_response(),
            })?;
            if let Some(ctx) = &ctx {
                let _ = ctx.emit("tool.end", json!({"tool": tool, "ok": true})).await;
                let _ = ctx
                    .emit("flow.node.end", json!({"node": node.id, "ok": true}))
                    .await;
            }
            Ok(result)
        }
        "aggregate" => {
            let mut prompt = node.prompt.unwrap_or_else(|| "Aggregate the following outputs.".to_string());
            if let Some(inputs) = node.inputs {
                for input in inputs {
                    if let Some(value) = outputs.get(&input) {
                        prompt.push_str(&format!("\n\n[{}]\n{}", input, value));
                    }
                }
            }
            let messages = vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "You are an aggregator.".to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: prompt,
                },
            ];
            let result = call_llm_with_messages(
                &state.cfg,
                req.provider.clone(),
                req.model.clone().or(lead_model),
                &messages,
            )
            .map(|text| Value::String(text))
            .map_err(|err| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": err})),
                )
                    .into_response()
            })?;
            if let Some(ctx) = &ctx {
                let _ = ctx
                    .emit("flow.node.end", json!({"node": node.id, "ok": true}))
                    .await;
            }
            Ok(result)
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unknown node kind {}", node.kind)})),
        )
            .into_response()),
    }
}

fn render_template(value: Value, outputs: &BTreeMap<String, Value>) -> Value {
    match value {
        Value::String(text) => {
            let mut out = text;
            for (key, val) in outputs {
                let needle = format!("{{{{{}}}}}", key);
                let replacement = match val {
                    Value::String(s) => s.clone(),
                    _ => val.to_string(),
                };
                out = out.replace(&needle, &replacement);
            }
            Value::String(out)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| render_template(v, outputs)).collect())
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k, render_template(v, outputs));
            }
            Value::Object(out)
        }
        other => other,
    }
}

fn extract_completion_text(value: &Value) -> Option<String> {
    let choices = value.get("choices")?.as_array()?;
    let first = choices.first()?;
    if let Some(text) = first.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    if let Some(message) = first.get("message") {
        if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
            return Some(content.to_string());
        }
    }
    None
}

fn tool_system_prompt() -> String {
    [
        "You are a tool-using assistant.",
        "When you need a tool, reply with ONLY valid JSON.",
        "Formats:",
        "  {\"tool\":\"shell\",\"args\":{\"cmd\":\"ls\"}}",
        "  {\"tool_calls\":[{\"tool\":\"fs.read\",\"args\":{\"path\":\"README.md\"}}]}",
        "If no tool is needed, reply with a normal answer.",
    ]
    .join("\n")
}

fn parse_tool_calls(text: &str) -> Vec<ToolCall> {
    let json = match extract_json_value(text) {
        Some(value) => value,
        None => return Vec::new(),
    };
    tool_calls_from_value(&json)
}

fn extract_json_value(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.starts_with("```") {
        if let Some(start) = trimmed.find("```") {
            if let Some(end) = trimmed.rfind("```") {
                if end > start + 3 {
                    let inner = &trimmed[start + 3..end];
                    let inner = inner.trim_start_matches("json").trim();
                    return serde_json::from_str(inner).ok();
                }
            }
        }
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed).ok()
    } else {
        None
    }
}

fn tool_calls_from_value(value: &Value) -> Vec<ToolCall> {
    if let Some(obj) = value.as_object() {
        if let Some(tool) = obj.get("tool").and_then(|v| v.as_str()) {
            let args = obj.get("args").cloned().unwrap_or(Value::Null);
            return vec![ToolCall {
                tool: tool.to_string(),
                args,
            }];
        }
        if let Some(calls) = obj.get("tool_calls").and_then(|v| v.as_array()) {
            return calls
                .iter()
                .filter_map(|item| {
                    let tool = item.get("tool").or_else(|| item.get("name"))?;
                    let tool = tool.as_str()?;
                    let args = item.get("args").cloned().unwrap_or(Value::Null);
                    Some(ToolCall {
                        tool: tool.to_string(),
                        args,
                    })
                })
                .collect();
        }
    }
    if let Some(arr) = value.as_array() {
        return arr
            .iter()
            .filter_map(|item| {
                let tool = item.get("tool").or_else(|| item.get("name"))?;
                let tool = tool.as_str()?;
                let args = item.get("args").cloned().unwrap_or(Value::Null);
                Some(ToolCall {
                    tool: tool.to_string(),
                    args,
                })
            })
            .collect();
    }
    Vec::new()
}

enum ToolExecError {
    ApprovalRequired,
    Failed(StatusCode, Value),
}

fn execute_tool(
    cfg: &Config,
    name: &str,
    args: Value,
    session_id: Option<&str>,
    agent_id: Option<&str>,
    approved: bool,
    approved_tools: Option<Vec<String>>,
    deny_on_missing: bool,
    approval_reason: Option<String>,
) -> Result<Value, ToolExecError> {
    let policy = policy_for(cfg, name);
    match policy {
        ToolPolicy::Deny => {
            return Err(ToolExecError::Failed(
                StatusCode::FORBIDDEN,
                json!({"error": "tool denied", "tool": name}),
            ));
        }
        ToolPolicy::Ask => {
            if let Some(allowed) = &approved_tools {
                if allowed.iter().any(|tool| tool == name) {
                    // pre-approved
                } else if approved {
                    // globally approved
                } else if deny_on_missing {
                    return Err(ToolExecError::Failed(
                        StatusCode::FORBIDDEN,
                        json!({"error": "approval_required", "tool": name}),
                    ));
                } else {
                    return Err(ToolExecError::ApprovalRequired);
                }
            } else if approved {
                // globally approved
            } else if deny_on_missing {
                return Err(ToolExecError::Failed(
                    StatusCode::FORBIDDEN,
                    json!({"error": "approval_required", "tool": name}),
                ));
            } else {
                return Err(ToolExecError::ApprovalRequired);
            }
        }
        ToolPolicy::Allow => {}
    }

    let _ = log_tool_event(
        cfg,
        session_id,
        agent_id,
        "tool.start",
        json!({
            "tool": name,
            "args": args,
            "approval_reason": approval_reason,
        }),
    );

    let result = match name {
        "shell" => run_shell(args),
        "fs.read" => run_fs_read(args),
        "fs.write" => run_fs_write(args),
        "fs.list" => run_fs_list(args),
        _ => Err((StatusCode::NOT_FOUND, json!({"error": "unknown tool"}))),
    };

    match result {
        Ok(value) => {
            let _ = log_tool_event(
                cfg,
                session_id,
                agent_id,
                "tool.end",
                json!({
                    "tool": name,
                    "ok": true,
                }),
            );
            Ok(value)
        }
        Err((status, err)) => {
            let _ = log_tool_event(
                cfg,
                session_id,
                agent_id,
                "tool.end",
                json!({
                    "tool": name,
                    "ok": false,
                    "error": err,
                }),
            );
            Err(ToolExecError::Failed(status, err))
        }
    }
}

fn set_agent_status(state: &AppState, agent_id: &str, status: &str) {
    let mut store = state.store.lock().unwrap();
    if let Some(agent) = store.agents.get_mut(agent_id) {
        agent.status = status.to_string();
        agent.worktree = agent.worktree.clone();
        let updated = agent.clone();
        let _ = append_store_event(&state.store_path, StoreEvent::AgentUpsert(updated));
    }
}

fn run_command_output(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_command_status(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exit status: {status}"))
    }
}

#[derive(Clone, Copy, Debug)]
enum ToolPolicy {
    Allow,
    Ask,
    Deny,
}

impl ToolPolicy {
    fn as_str(self) -> &'static str {
        match self {
            ToolPolicy::Allow => "allow",
            ToolPolicy::Ask => "ask",
            ToolPolicy::Deny => "deny",
        }
    }
}

fn policy_for(cfg: &Config, tool: &str) -> ToolPolicy {
    let mut best: Option<(&str, &str)> = None;
    for (key, value) in cfg.tools.policies.iter() {
        if tool == key || tool.starts_with(&format!("{key}.")) {
            if best.is_none() || key.len() > best.unwrap().0.len() {
                best = Some((key.as_str(), value.as_str()));
            }
        }
    }
    if let Some((_, value)) = best {
        return parse_policy(value);
    }
    parse_policy(&cfg.tools.default_policy)
}

fn parse_policy(value: &str) -> ToolPolicy {
    match value.trim().to_lowercase().as_str() {
        "allow" => ToolPolicy::Allow,
        "deny" => ToolPolicy::Deny,
        _ => ToolPolicy::Ask,
    }
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    args_schema: Value,
}

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "shell",
            description: "Execute a shell command",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" },
                    "cwd": { "type": "string" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "fs.read",
            description: "Read a file from disk",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_bytes": { "type": "integer" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs.write",
            description: "Write a file to disk",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "append": { "type": "boolean" },
                    "create_dirs": { "type": "boolean" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "fs.list",
            description: "List a directory",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        },
    ]
}

#[derive(Deserialize)]
struct ShellArgs {
    cmd: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct FsReadArgs {
    path: String,
    max_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct FsWriteArgs {
    path: String,
    content: String,
    append: Option<bool>,
    create_dirs: Option<bool>,
}

#[derive(Deserialize)]
struct FsListArgs {
    path: String,
}

fn run_shell(args: Value) -> Result<Value, (StatusCode, Value)> {
    let args: ShellArgs = serde_json::from_value(args)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))?;
    let (shell, shell_args) = select_shell();
    let mut cmd = std::process::Command::new(shell);
    for arg in shell_args {
        cmd.arg(arg);
    }
    cmd.arg(args.cmd);
    if let Some(cwd) = args.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(envs) = args.env {
        for (key, value) in envs {
            cmd.env(key, value);
        }
    }
    let output = cmd
        .output()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
    Ok(json!({
        "status": output.status.code(),
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

fn select_shell() -> (String, Vec<String>) {
    let shell = env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let shell_lower = shell.to_lowercase();
    if shell_lower.contains("zsh") {
        return (shell, vec!["-f".to_string(), "-c".to_string()]);
    }
    if shell_lower.contains("bash") {
        return (
            shell,
            vec![
                "--noprofile".to_string(),
                "--norc".to_string(),
                "-c".to_string(),
            ],
        );
    }
    (shell, vec!["-c".to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn select_shell_uses_zsh_flags() {
        let _guard = env_lock().lock().unwrap();
        env::set_var("SHELL", "/bin/zsh");
        let (shell, args) = select_shell();
        assert_eq!(shell, "/bin/zsh");
        assert_eq!(args, vec!["-f".to_string(), "-c".to_string()]);
    }

    #[test]
    fn select_shell_uses_bash_flags() {
        let _guard = env_lock().lock().unwrap();
        env::set_var("SHELL", "/usr/bin/bash");
        let (shell, args) = select_shell();
        assert_eq!(shell, "/usr/bin/bash");
        assert_eq!(
            args,
            vec![
                "--noprofile".to_string(),
                "--norc".to_string(),
                "-c".to_string()
            ]
        );
    }

    #[test]
    fn select_shell_falls_back() {
        let _guard = env_lock().lock().unwrap();
        env::set_var("SHELL", "/bin/sh");
        let (shell, args) = select_shell();
        assert_eq!(shell, "/bin/sh");
        assert_eq!(args, vec!["-c".to_string()]);
    }

    #[test]
    fn next_id_is_human_readable() {
        let mut store = Store::default();
        let id = store.next_id("sess");
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "sess");
        assert_eq!(parts[1].len(), 8);
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[2].len(), 6);
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[3].len(), 3);
        assert!(parts[3].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn update_seq_from_id_tracks_suffix() {
        let mut store = Store::default();
        store.update_seq_from_id("sess_20260204_010203_042");
        assert_eq!(store.seq, 42);
        store.update_seq_from_id("agent_20260204_010203_007");
        assert_eq!(store.seq, 42);
        store.update_seq_from_id("sess_20260204_010203_100");
        assert_eq!(store.seq, 100);
    }

    #[test]
    fn parse_tool_calls_single() {
        let text = r#"{"tool":"shell","args":{"cmd":"ls"}}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "shell");
        assert_eq!(calls[0].args["cmd"], "ls");
    }

    #[test]
    fn parse_tool_calls_array() {
        let text = r#"{"tool_calls":[{"tool":"fs.read","args":{"path":"README.md"}}]}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "fs.read");
        assert_eq!(calls[0].args["path"], "README.md");
    }

    #[test]
    fn parse_tool_calls_non_json() {
        let calls = parse_tool_calls("hello there");
        assert!(calls.is_empty());
    }

    #[test]
    fn resolve_worktree_mode_auto_inherit() {
        let mode = resolve_worktree_mode(Some("auto"), None, Some("/tmp/wt")).unwrap();
        match mode {
            WorktreeMode::Inherit => {}
            _ => panic!("expected inherit"),
        }
    }

    #[test]
    fn resolve_worktree_mode_new_requires_repo() {
        let err = resolve_worktree_mode(Some("new"), None, None).unwrap_err();
        assert!(err.contains("repo_path"));
    }

    #[test]
    fn resolve_worktree_mode_auto_new() {
        let mode = resolve_worktree_mode(Some("auto"), Some("/tmp/repo"), None).unwrap();
        match mode {
            WorktreeMode::New(path) => assert_eq!(path, "/tmp/repo"),
            _ => panic!("expected new"),
        }
    }

    #[test]
    fn clear_tool_uses_keeps_last() {
        let messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: "Tool result for shell:\n1".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "ok".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "Tool result for fs.read:\n2".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "Tool result for fs.list:\n3".to_string(),
            },
        ];
        let cleared = clear_tool_uses(&messages, 2);
        let tool_msgs: Vec<_> = cleared
            .iter()
            .filter(|m| m.role == "user" && m.content.starts_with("Tool result for "))
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        assert!(tool_msgs[0].content.contains("fs.read"));
        assert!(tool_msgs[1].content.contains("fs.list"));
    }

    #[test]
    fn render_template_replaces_keys() {
        let mut outputs = BTreeMap::new();
        outputs.insert("node1".to_string(), Value::String("hello".to_string()));
        let value = Value::String("Value: {{node1}}".to_string());
        let rendered = render_template(value, &outputs);
        assert_eq!(rendered, Value::String("Value: hello".to_string()));
    }
}

fn run_fs_read(args: Value) -> Result<Value, (StatusCode, Value)> {
    let args: FsReadArgs = serde_json::from_value(args)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))?;
    let path = Path::new(&args.path);
    let bytes = fs::read(path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
    let mut truncated = false;
    let data = if let Some(max) = args.max_bytes {
        if bytes.len() as u64 > max {
            truncated = true;
            &bytes[..max as usize]
        } else {
            &bytes[..]
        }
    } else {
        &bytes[..]
    };
    Ok(json!({
        "path": args.path,
        "truncated": truncated,
        "content": String::from_utf8_lossy(data),
    }))
}

fn run_fs_write(args: Value) -> Result<Value, (StatusCode, Value)> {
    let args: FsWriteArgs = serde_json::from_value(args)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))?;
    let path = Path::new(&args.path);
    if args.create_dirs == Some(true) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
        }
    }
    if args.append == Some(true) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
        file.write_all(args.content.as_bytes())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
    } else {
        fs::write(path, args.content)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
    }
    Ok(json!({ "ok": true, "path": args.path }))
}

fn run_fs_list(args: Value) -> Result<Value, (StatusCode, Value)> {
    let args: FsListArgs = serde_json::from_value(args)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))?;
    let mut entries = Vec::new();
    let dir = fs::read_dir(&args.path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
    for entry in dir {
        let entry = entry
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})))?;
        let path = entry.path();
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": path.to_string_lossy(),
            "is_dir": path.is_dir(),
        }));
    }
    Ok(json!({ "entries": entries }))
}

fn log_tool_event(
    cfg: &Config,
    session_id: Option<&str>,
    agent_id: Option<&str>,
    kind: &str,
    data: Value,
) -> Result<(), std::io::Error> {
    let session_id = match session_id {
        Some(value) => value,
        None => return Ok(()),
    };
    let dir = log_dir(cfg, session_id);
    fs::create_dir_all(&dir)?;
    let event = json!({
        "ts_ms": now_ms(),
        "session_id": session_id,
        "agent_id": agent_id,
        "kind": kind,
        "data": data,
    });
    let path = dir.join("events.jsonl");
    append_jsonl(&path, &event)?;
    Ok(())
}

fn log_dir(cfg: &Config, session_id: &str) -> PathBuf {
    let base = resolve_path(&cfg.logging.dir);
    base.join(session_id)
}

fn resolve_path(path: &str) -> PathBuf {
    if let Ok(home) = env::var("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn append_jsonl(path: &Path, value: &Value) -> Result<(), std::io::Error> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    writeln!(file, "{}", line)?;
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    id: String,
    title: String,
    created_at: u128,
    updated_at: u128,
    status: String,
    share_state: String,
    tmux_session_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Agent {
    id: String,
    session_id: String,
    parent_agent_id: Option<String>,
    model: Option<String>,
    status: String,
    tmux_session_name: Option<String>,
    worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    id: String,
    session_id: String,
    agent_id: String,
    mode: String,
    status: String,
    started_at: u128,
    ended_at: Option<u128>,
}

#[derive(Debug, Default)]
struct Store {
    sessions: BTreeMap<String, Session>,
    agents: BTreeMap<String, Agent>,
    runs: BTreeMap<String, Run>,
    seq: u64,
}

impl Store {
    fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        let stamp = Utc::now().format("%Y%m%d_%H%M%S");
        format!("{prefix}_{stamp}_{:03}", self.seq)
    }

    fn update_seq_from_id(&mut self, id: &str) {
        if let Some(last) = id.rsplit('_').next() {
            if let Ok(value) = last.parse::<u64>() {
                if value > self.seq {
                    self.seq = value;
                }
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
enum StoreEvent {
    #[serde(rename = "session.upsert")]
    SessionUpsert(Session),
    #[serde(rename = "agent.upsert")]
    AgentUpsert(Agent),
    #[serde(rename = "run.upsert")]
    RunUpsert(Run),
}

fn load_store(path: &Path) -> Result<Store, std::io::Error> {
    if !path.exists() {
        return Ok(Store::default());
    }
    let content = fs::read_to_string(path)?;
    let mut store = Store::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<StoreEvent>(line) {
            apply_event(&mut store, event);
        }
    }
    Ok(store)
}

fn apply_event(store: &mut Store, event: StoreEvent) {
    match event {
        StoreEvent::SessionUpsert(session) => {
            store.update_seq_from_id(&session.id);
            store.sessions.insert(session.id.clone(), session);
        }
        StoreEvent::AgentUpsert(agent) => {
            store.update_seq_from_id(&agent.id);
            store.agents.insert(agent.id.clone(), agent);
        }
        StoreEvent::RunUpsert(run) => {
            store.update_seq_from_id(&run.id);
            store.runs.insert(run.id.clone(), run);
        }
    }
}

fn append_store_event(path: &Path, event: StoreEvent) -> Result<(), std::io::Error> {
    append_jsonl(path, &serde_json::to_value(event).unwrap_or_else(|_| json!({})))
}

fn spawn_tmux_session(name: &str, cwd: Option<&str>) -> Result<(), std::io::Error> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.arg("new-session").arg("-d").arg("-s").arg(name);
    if let Some(path) = cwd {
        cmd.arg("-c").arg(path);
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("tmux exited with status: {status}"),
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum WorktreeMode {
    None,
    Inherit,
    New(String),
}

fn resolve_worktree_mode(
    mode: Option<&str>,
    repo_path: Option<&str>,
    parent_worktree: Option<&str>,
) -> Result<WorktreeMode, String> {
    let mode = mode.unwrap_or("auto").to_lowercase();
    match mode.as_str() {
        "none" => Ok(WorktreeMode::None),
        "inherit" => {
            if parent_worktree.is_some() {
                Ok(WorktreeMode::Inherit)
            } else {
                Err("parent worktree not found".to_string())
            }
        }
        "new" => {
            let repo_path = repo_path.ok_or_else(|| "repo_path is required for new worktree".to_string())?;
            Ok(WorktreeMode::New(repo_path.to_string()))
        }
        "auto" => {
            if let Some(repo_path) = repo_path {
                Ok(WorktreeMode::New(repo_path.to_string()))
            } else if parent_worktree.is_some() {
                Ok(WorktreeMode::Inherit)
            } else {
                Ok(WorktreeMode::None)
            }
        }
        _ => Err("invalid worktree_mode (use auto|new|inherit|none)".to_string()),
    }
}

fn create_worktree(
    cfg: &Config,
    session_id: &str,
    agent_id: &str,
    repo_path: &str,
) -> Result<String, String> {
    let repo_root = git_repo_root(repo_path)?;
    let base = resolve_path(&cfg.logging.dir)
        .join("worktrees")
        .join(session_id);
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let worktree_path = base.join(agent_id);
    let branch_name = format!("aish/{agent_id}");

    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_path)
        .arg("-b")
        .arg(&branch_name)
        .status()
        .map_err(|e| e.to_string())?;

    if !status.success() {
        return Err(format!("git worktree add failed with status: {status}"));
    }

    Ok(worktree_path.to_string_lossy().to_string())
}

fn git_repo_root(path: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
