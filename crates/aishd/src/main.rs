use anyhow::{bail, Result};
use axum::{
    extract::State,
    extract::Path as AxumPath,
    routing::{get, post},
    Json, Router,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use clap::Parser;
use aish_core::config::{self, Config, OpenAICompatConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

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

    let app = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/completions", post(completions))
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/:name/call", post(call_tool))
        .with_state(cfg);

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

async fn completions(
    State(cfg): State<Config>,
    Json(req): Json<CompletionRequest>,
) -> Response {
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

async fn list_tools(State(cfg): State<Config>) -> impl IntoResponse {
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
    State(cfg): State<Config>,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<ToolCallRequest>,
) -> Response {
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
