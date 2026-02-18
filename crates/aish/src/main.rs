use aish_core::config::{self, Config};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "aish", version, about = "AI SHell launcher")]
struct Cli {
    /// Path to aish.json config
    #[arg(long, default_value = "~/.config/aish/aish.json")]
    config: String,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Launch interactive aish shell (tmux-enforced) and aishd if needed
    Launch,
    /// Start aishd in headless mode
    Serve,
    /// Start the web UI (served by aishd)
    Web,
    /// Attach to an existing aishd server
    Attach {
        /// Base URL of aishd (e.g. http://127.0.0.1:4096)
        url: String,
    },
    /// Call the LLM via aishd /v1/completions
    Llm {
        /// Prompt text (if omitted, read from stdin)
        prompt: Vec<String>,
        /// Provider profile name (e.g. together)
        #[arg(long)]
        provider: Option<String>,
        /// Override model
        #[arg(long)]
        model: Option<String>,
        /// Max tokens
        #[arg(long = "max-tokens")]
        max_tokens: Option<u32>,
        /// Temperature
        #[arg(long)]
        temperature: Option<f32>,
        /// Top-p
        #[arg(long = "top-p")]
        top_p: Option<f32>,
        /// Stop sequences (repeatable)
        #[arg(long = "stop")]
        stop: Vec<String>,
    },
    /// Show recent sessions and log locations
    Status,
    /// Ensure aishd is running (useful for shell startup hooks)
    EnsureDaemon {
        /// Suppress informational output
        #[arg(long, default_value_t = false)]
        quiet: bool,
    },
    /// Append a structured log event (used by shell hooks)
    Log {
        /// Event kind (e.g. command.start, command.end, terminal.resize)
        #[arg(long)]
        kind: String,
        /// Optional command string
        #[arg(long)]
        cmd: Option<String>,
        /// Optional current working directory
        #[arg(long)]
        cwd: Option<String>,
        /// Optional exit code
        #[arg(long)]
        exit: Option<i32>,
        /// Optional duration in milliseconds
        #[arg(long = "duration-ms")]
        duration_ms: Option<u64>,
        /// Optional terminal columns
        #[arg(long)]
        cols: Option<u16>,
        /// Optional terminal rows
        #[arg(long)]
        rows: Option<u16>,
        /// Override session id (otherwise from AISH_SESSION_ID)
        #[arg(long)]
        session: Option<String>,
        /// Override log directory (otherwise from AISH_LOG_DIR)
        #[arg(long = "log-dir")]
        log_dir: Option<String>,
        /// Override actor (otherwise from AISH_ACTOR)
        #[arg(long)]
        actor: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let argv = normalize_cli_args(env::args().collect());
    let cli = Cli::parse_from(argv);
    let cfg = config::load(&cli.config)?;

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            if !std::io::stdin().is_terminal() {
                CliCommand::Llm {
                    prompt: Vec::new(),
                    provider: None,
                    model: None,
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    stop: Vec::new(),
                }
            } else {
                CliCommand::Launch
            }
        }
    };

    match command {
        CliCommand::Launch => {
            ensure_aishd_running(&cli.config, &cfg)?;
            if !inside_tmux() {
                launch_tmux(&cfg)?;
                return Ok(());
            }
            println!("aish launch: already inside tmux (Phase 0 stub)");
        }
        CliCommand::Serve => {
            let status = Command::new("aishd")
                .arg("--config")
                .arg(&cli.config)
                .status()?;
            if !status.success() {
                bail!("aishd exited with status: {status}");
            }
        }
        CliCommand::Web => {
            println!("aish web (Phase 0 stub)");
        }
        CliCommand::Attach { url } => {
            println!("aish attach to {url} (Phase 0 stub)");
        }
        CliCommand::Llm {
            prompt,
            provider,
            model,
            max_tokens,
            temperature,
            top_p,
            stop,
        } => {
            run_llm(
                &cfg,
                prompt,
                provider,
                model,
                max_tokens,
                temperature,
                top_p,
                stop,
            )?;
        }
        CliCommand::Status => {
            show_status(&cfg)?;
        }
        CliCommand::EnsureDaemon { quiet } => {
            ensure_aishd_running(&cli.config, &cfg)?;
            if !quiet {
                let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
                println!("aishd is running at {base_url}");
            }
        }
        CliCommand::Log {
            kind,
            cmd,
            cwd,
            exit,
            duration_ms,
            cols,
            rows,
            session,
            log_dir,
            actor,
        } => {
            write_log_event(
                &cfg,
                LogEventArgs {
                    kind,
                    cmd,
                    cwd,
                    exit,
                    duration_ms,
                    cols,
                    rows,
                    session,
                    log_dir,
                    actor,
                },
            )?;
        }
    }

    Ok(())
}

fn normalize_cli_args(raw: Vec<String>) -> Vec<String> {
    if raw.len() <= 1 {
        return raw;
    }

    let known_subcommands = [
        "launch",
        "serve",
        "web",
        "attach",
        "llm",
        "status",
        "log",
        "ensure-daemon",
        "ensure_daemon",
        "help",
    ];

    let mut idx = 1usize;
    while idx < raw.len() {
        let token = raw[idx].as_str();
        if token == "--config" {
            idx += 1;
            if idx < raw.len() {
                idx += 1;
            }
            continue;
        }
        if token.starts_with("--config=") {
            idx += 1;
            continue;
        }
        break;
    }

    if idx >= raw.len() {
        return raw;
    }
    let first = raw[idx].as_str();
    if known_subcommands.contains(&first)
        || first == "-h"
        || first == "--help"
        || first == "--version"
    {
        return raw;
    }

    let mut out = Vec::with_capacity(raw.len() + 1);
    out.extend_from_slice(&raw[..idx]);
    out.push("llm".to_string());
    out.extend_from_slice(&raw[idx..]);
    out
}

fn inside_tmux() -> bool {
    env::var("TMUX").is_ok()
}

fn launch_tmux(cfg: &Config) -> Result<()> {
    let session_name = format!("{}-main", cfg.tmux.session_prefix);
    let cwd = env::current_dir()?;
    let session_id = generate_session_id();
    let log_dir = prepare_log_dir(cfg, &session_id)?;
    let zdotdir = ensure_zdotdir()?;

    if !tmux_has_session(&session_name) {
        println!("aish: session {session_id}, logs at {}", log_dir.display());
        let mut cmd = Command::new("tmux");
        cmd.arg("new-session")
            .arg("-d")
            .arg("-A")
            .arg("-s")
            .arg(&session_name)
            .arg("-c")
            .arg(&cwd)
            .arg("-e")
            .arg(format!("AISH_SESSION_ID={session_id}"))
            .arg("-e")
            .arg(format!("AISH_LOG_DIR={}", log_dir.display()))
            .arg("-e")
            .arg("AISH_ACTOR=human")
            .arg("-e")
            .arg(format!("ZDOTDIR={}", zdotdir.display()));

        if let Ok(bin) = env::current_exe() {
            cmd.arg("-e").arg(format!("AISH_BIN={}", bin.display()));
        }

        cmd.arg("zsh").arg("-l");

        let status = cmd.status()?;
        if !status.success() {
            bail!("tmux exited with status: {status}");
        }

        write_log_event(
            cfg,
            LogEventArgs {
                kind: "session.start".to_string(),
                cmd: None,
                cwd: Some(cwd.to_string_lossy().to_string()),
                exit: None,
                duration_ms: None,
                cols: None,
                rows: None,
                session: Some(session_id.clone()),
                log_dir: Some(log_dir.to_string_lossy().to_string()),
                actor: Some("human".to_string()),
            },
        )?;

        let output_path = log_dir.join("output.log");
        ensure_file(&output_path)?;
        let target = format!("{session_name}:0.0");
        let cmd = format!("cat >> {}", shell_quote(&output_path));
        let status = Command::new("tmux")
            .arg("pipe-pane")
            .arg("-o")
            .arg("-t")
            .arg(&target)
            .arg(&cmd)
            .status()?;
        if !status.success() {
            bail!("tmux pipe-pane failed with status: {status}");
        }
    }

    let status = Command::new("tmux")
        .arg("attach-session")
        .arg("-t")
        .arg(&session_name)
        .status()?;
    if !status.success() {
        bail!("tmux attach-session failed with status: {status}");
    }
    Ok(())
}

fn ensure_aishd_running(config_path: &str, cfg: &Config) -> Result<()> {
    let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
    if health_ok(&base_url) {
        return Ok(());
    }

    Command::new("aishd")
        .arg("--config")
        .arg(config_path)
        .spawn()?;

    for _ in 0..20 {
        if health_ok(&base_url) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }

    bail!("aishd did not become healthy at {base_url}");
}

fn health_ok(base_url: &str) -> bool {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(400))
        .build();
    let url = format!("{base_url}/health");
    match agent.get(&url).call() {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

#[derive(Debug)]
struct LogEventArgs {
    kind: String,
    cmd: Option<String>,
    cwd: Option<String>,
    exit: Option<i32>,
    duration_ms: Option<u64>,
    cols: Option<u16>,
    rows: Option<u16>,
    session: Option<String>,
    log_dir: Option<String>,
    actor: Option<String>,
}

fn write_log_event(cfg: &Config, args: LogEventArgs) -> Result<()> {
    let mut data = Map::new();
    if let Some(cmd) = args.cmd {
        data.insert("cmd".to_string(), json!(cmd));
    }
    if let Some(cwd) = args.cwd {
        data.insert("cwd".to_string(), json!(cwd));
    }
    if let Some(exit) = args.exit {
        data.insert("exit".to_string(), json!(exit));
    }
    if let Some(duration_ms) = args.duration_ms {
        data.insert("duration_ms".to_string(), json!(duration_ms));
    }
    if let Some(cols) = args.cols {
        data.insert("cols".to_string(), json!(cols));
    }
    if let Some(rows) = args.rows {
        data.insert("rows".to_string(), json!(rows));
    }

    write_log_event_value(
        cfg,
        args.kind,
        Value::Object(data),
        args.session,
        args.log_dir,
        args.actor,
    )?;
    Ok(())
}

fn write_log_event_value(
    cfg: &Config,
    kind: String,
    data: Value,
    session: Option<String>,
    log_dir: Option<String>,
    actor: Option<String>,
) -> Result<()> {
    let (session_id, log_dir, actor) = resolve_session_and_dir(cfg, session, log_dir, actor)?;

    let event = json!({
        "ts_ms": now_ms(),
        "session_id": session_id,
        "actor": actor,
        "kind": kind,
        "data": data,
        "pid": std::process::id(),
    });

    let events_path = log_dir.join("events.jsonl");
    append_jsonl(&events_path, &event)?;
    trigger_log_ingest(cfg, &session_id);
    Ok(())
}

fn trigger_log_ingest(cfg: &Config, session_id: &str) {
    let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
    let url = format!("{base_url}/v1/logs/ingest/{session_id}");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    let _ = agent.post(&url).call();
}

fn resolve_session_and_dir(
    cfg: &Config,
    session: Option<String>,
    log_dir: Option<String>,
    actor: Option<String>,
) -> Result<(String, PathBuf, String)> {
    let session_id = session
        .or_else(|| env::var("AISH_SESSION_ID").ok())
        .context("missing session id (set AISH_SESSION_ID or --session)")?;

    let log_dir = match log_dir.or_else(|| env::var("AISH_LOG_DIR").ok()) {
        Some(dir) => resolve_path(&dir),
        None => {
            let base = resolve_path(&cfg.logging.dir);
            base.join(&session_id)
        }
    };
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;

    let actor = actor
        .or_else(|| env::var("AISH_ACTOR").ok())
        .unwrap_or_else(|| "unknown".to_string());

    Ok((session_id, log_dir, actor))
}

fn append_jsonl(path: &Path, value: &Value) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open log file: {}", path.display()))?;
    let line = serde_json::to_string(value)?;
    use std::io::Write;
    writeln!(file, "{}", line)?;
    Ok(())
}

fn generate_session_id() -> String {
    let ts = now_ms();
    let pid = std::process::id();
    format!("{ts}-{pid}")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
}

fn resolve_path(path: &str) -> PathBuf {
    if let Some(home) = env::var("HOME").ok() {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn prepare_log_dir(cfg: &Config, session_id: &str) -> Result<PathBuf> {
    let base = resolve_path(&cfg.logging.dir);
    let dir = base.join(session_id);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create log dir: {}", dir.display()))?;
    let events_path = dir.join("events.jsonl");
    ensure_file(&events_path)?;
    Ok(dir)
}

fn ensure_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(path, b"")?;
    }
    Ok(())
}

fn tmux_has_session(session: &str) -> bool {
    Command::new("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ensure_zdotdir() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    let base = Path::new(&home).join(".config/aish");
    fs::create_dir_all(&base)?;
    let zdotdir = base.join("zdot");
    fs::create_dir_all(&zdotdir)?;

    let zshenv_path = zdotdir.join(".zshenv");
    if !zshenv_path.exists() {
        fs::write(
            &zshenv_path,
            "if [[ -f \"$HOME/.zshenv\" ]]; then\n  source \"$HOME/.zshenv\"\nfi\n",
        )?;
    }

    let zshrc_path = zdotdir.join(".zshrc");
    if !zshrc_path.exists() {
        fs::write(
            &zshrc_path,
            "if [[ -f \"$HOME/.zshrc\" ]]; then\n  source \"$HOME/.zshrc\"\nfi\nif [[ -f \"$HOME/.config/aish/aish.zsh\" ]]; then\n  source \"$HOME/.config/aish/aish.zsh\"\nfi\n",
        )?;
    }

    let hook_path = base.join("aish.zsh");
    match fs::read_to_string(&hook_path) {
        Ok(contents) => {
            if contents.contains("aish hooks (auto-generated)") {
                fs::write(&hook_path, default_hook_script())?;
            }
        }
        Err(_) => {
            fs::write(&hook_path, default_hook_script())?;
        }
    }

    Ok(zdotdir)
}

fn default_hook_script() -> &'static str {
    r#"# aish hooks (auto-generated)
autoload -Uz add-zsh-hook

typeset -g AISH_CMD_START=""
typeset -g AISH_IN_LOG=0

_aish_log() {
  if [[ -z "$AISH_LOG_DIR" || -z "$AISH_SESSION_ID" ]]; then
    return
  fi
  if (( AISH_IN_LOG )); then
    return
  fi
  AISH_IN_LOG=1
  local bin="${AISH_BIN:-aish}"
  command "$bin" log "$@"
  AISH_IN_LOG=0
}

llm() {
  local bin="${AISH_BIN:-aish}"
  command "$bin" llm "$@"
}

_aish_preexec() {
  if (( AISH_IN_LOG )); then
    return
  fi
  AISH_CMD_START="${EPOCHREALTIME}"
  if [[ -n "$AISH_LOG_DIR" ]]; then
    print -r -- "$1" >> "$AISH_LOG_DIR/stdin.log"
  fi
  _aish_log --kind "command.start" --cmd "$1" --cwd "$PWD"
}

_aish_precmd() {
  if (( AISH_IN_LOG )); then
    return
  fi
  local exit_code=$?
  local end="${EPOCHREALTIME}"
  local duration_ms=""
  if [[ -n "$AISH_CMD_START" && -n "$end" ]]; then
    local -F start=$AISH_CMD_START
    local -F finish=$end
    local -F diff=$(( (finish - start) * 1000 ))
    duration_ms=$(printf "%.0f" "$diff")
  fi
  if [[ -n "$duration_ms" ]]; then
    _aish_log --kind "command.end" --exit "$exit_code" --cwd "$PWD" --duration-ms "$duration_ms"
  else
    _aish_log --kind "command.end" --exit "$exit_code" --cwd "$PWD"
  fi
}

TRAPWINCH() {
  _aish_log --kind "terminal.resize" --cols "$COLUMNS" --rows "$LINES"
}

add-zsh-hook preexec _aish_preexec
add-zsh-hook precmd _aish_precmd
"#
}

fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn run_llm(
    cfg: &Config,
    prompt_parts: Vec<String>,
    provider: Option<String>,
    model: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Vec<String>,
) -> Result<()> {
    let prompt = if prompt_parts.is_empty() {
        read_stdin_to_string()?
    } else {
        prompt_parts.join(" ")
    };
    if prompt.trim().is_empty() {
        bail!("prompt is empty");
    }
    let prompt = maybe_attach_workspace_context(&prompt);
    let opts = CompletionOverrides {
        provider,
        model,
        max_tokens,
        temperature,
        top_p,
        stop,
    };
    let session_id = env::var("AISH_SESSION_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let _ = write_log_event_value(
        cfg,
        "llm.request".to_string(),
        json!({
            "prompt": prompt,
            "model": opts.model.clone().map(Value::String).unwrap_or(Value::Null),
            "provider": opts.provider.clone().map(Value::String).unwrap_or(Value::Null),
            "agentic_inline": true,
        }),
        None,
        None,
        None,
    );

    match run_llm_inline_agentic(cfg, &prompt, &opts, session_id.as_deref()) {
        Ok(out) => {
            if !out.is_empty() && !out.ends_with('\n') {
                println!();
            }
            let _ = write_log_event_value(
                cfg,
                "llm.response".to_string(),
                json!({ "text": out }),
                None,
                None,
                None,
            );
            return Ok(());
        }
        Err(err) => {
            eprintln!("ai inline tool loop failed ({err}); falling back to single completion");
        }
    }

    let out = run_llm_single_completion(cfg, &prompt, &opts, session_id.as_deref())?;
    if !out.is_empty() && !out.ends_with('\n') {
        println!();
    }
    let _ = write_log_event_value(
        cfg,
        "llm.response".to_string(),
        json!({
            "text": out,
            "fallback": "single_completion",
        }),
        None,
        None,
        None,
    );
    Ok(())
}

#[derive(Clone)]
struct CompletionOverrides {
    provider: Option<String>,
    model: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Vec<String>,
}

#[derive(Debug, Clone)]
struct InlineToolCall {
    tool: String,
    args: Value,
}

#[derive(Debug, Deserialize)]
struct InlineShellArgs {
    cmd: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InlineFsReadArgs {
    path: String,
    max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct InlineFsWriteArgs {
    path: String,
    content: String,
    append: Option<bool>,
    create_dirs: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct InlineFsListArgs {
    path: String,
}

#[derive(Debug, Clone)]
struct InlineToolState {
    cwd: PathBuf,
}

fn run_llm_inline_agentic(
    cfg: &Config,
    prompt: &str,
    opts: &CompletionOverrides,
    session_id: Option<&str>,
) -> Result<String> {
    if env_flag_enabled("AISH_NO_AGENTIC") {
        bail!("agentic mode disabled by AISH_NO_AGENTIC");
    }

    let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
    let url = format!("{base_url}/v1/completions");
    let cwd = env::current_dir().context("failed to resolve cwd")?;
    let commands = detect_installed_commands();
    let mut state = InlineToolState { cwd };
    let max_steps = inline_max_steps();
    let mut progress = ProgressLine::new(std::io::stderr().is_terminal() && ai_progress_enabled());
    let mut messages = vec![
        json!({
            "role": "system",
            "content": inline_agent_system_prompt(&state.cwd, &commands),
        }),
        json!({
            "role": "user",
            "content": prompt,
        }),
    ];

    let mut final_output = String::new();
    for step in 1..=max_steps {
        progress.set_status(&format!("Planning next step ({step}/{max_steps})..."));
        let body =
            build_chat_completion_body(&messages, opts, if step == 1 { session_id } else { None });
        let response = call_completion_json(&url, &body)?;
        let assistant = extract_completion_text(&response)
            .or_else(|| {
                response
                    .get("raw")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| response.to_string());
        let tool_calls = parse_inline_tool_calls(&assistant);
        messages.push(json!({
            "role": "assistant",
            "content": assistant,
        }));

        if tool_calls.is_empty() {
            progress.stop();
            print!("{}", assistant);
            std::io::stdout().flush()?;
            final_output = assistant;
            break;
        }

        for call in tool_calls {
            progress.set_status(&format!("Running {}", call.tool));
            let result = execute_inline_tool(&base_url, &call, &mut state, session_id);
            let rendered = match result {
                Ok(value) => value,
                Err(err) => json!({
                    "success": false,
                    "error": err.to_string(),
                }),
            };
            messages.push(json!({
                "role": "user",
                "content": format!("Tool result for {}:\n{}", call.tool, rendered),
            }));
        }
    }

    progress.stop();
    if final_output.is_empty() {
        bail!("max agentic steps reached without final answer");
    }
    Ok(final_output)
}

fn run_llm_single_completion(
    cfg: &Config,
    prompt: &str,
    opts: &CompletionOverrides,
    session_id: Option<&str>,
) -> Result<String> {
    let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
    let url = format!("{base_url}/v1/completions");
    let stream_url = format!("{base_url}/v1/completions/stream");

    let mut body = json!({
        "prompt": prompt,
    });
    if let Some(session_id) = session_id {
        body["session_id"] = json!(session_id);
        body["context_mode"] = json!("diagnostic");
    }
    if let Some(provider) = opts.provider.clone() {
        body["provider"] = json!(provider);
    }
    if let Some(model) = opts.model.clone() {
        body["model"] = json!(model);
    }
    if let Some(max_tokens) = opts.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = opts.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = opts.top_p {
        body["top_p"] = json!(top_p);
    }
    if !opts.stop.is_empty() {
        body["stop"] = json!(opts.stop);
    }

    let mut stream_body = body.clone();
    stream_body["stream"] = json!(true);
    if let Ok(out) = stream_completion_response(&stream_url, &stream_body) {
        return Ok(out);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let resp = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .with_context(|| "failed to call aishd /v1/completions")?;

    let text = resp.into_string().unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    if let Some(out) = extract_completion_text(&value) {
        print!("{out}");
        std::io::stdout().flush()?;
        return Ok(out);
    }
    let pretty = serde_json::to_string_pretty(&value).unwrap_or(text);
    print!("{pretty}");
    std::io::stdout().flush()?;
    Ok(pretty)
}

fn call_completion_json(url: &str, body: &Value) -> Result<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build();
    match agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(resp) => {
            let text = resp.into_string().unwrap_or_default();
            Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text })))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let parsed: Value =
                serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Err(anyhow!(
                "completion request failed with status {status}: {}",
                parsed
            ))
        }
        Err(err) => Err(anyhow!("failed to call aishd /v1/completions: {err}")),
    }
}

fn build_chat_completion_body(
    messages: &[Value],
    opts: &CompletionOverrides,
    session_id: Option<&str>,
) -> Value {
    let mut body = json!({
        "messages": messages,
    });
    if let Some(session_id) = session_id {
        body["session_id"] = json!(session_id);
        body["context_mode"] = json!("diagnostic");
    }
    if let Some(provider) = opts.provider.clone() {
        body["provider"] = json!(provider);
    }
    if let Some(model) = opts.model.clone() {
        body["model"] = json!(model);
    }
    if let Some(max_tokens) = opts.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = opts.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = opts.top_p {
        body["top_p"] = json!(top_p);
    }
    if !opts.stop.is_empty() {
        body["stop"] = json!(opts.stop);
    }
    body
}

fn inline_agent_system_prompt(cwd: &Path, commands: &[String]) -> String {
    let command_list = if commands.is_empty() {
        "<none detected>".to_string()
    } else {
        commands.join(", ")
    };
    [
        "You are `ai`, an agentic coding assistant running on the local machine.",
        "You have direct tool access. Do not claim filesystem/shell access is unavailable.",
        "",
        "Preferred strategy:",
        "- Use `shell` for search/build/test/run workflows (rg, git, python3, cargo, etc.).",
        "- Use `fs.read` / `fs.write` / `fs.list` for precise file operations.",
        "- Handle failures by inspecting stderr and iterating.",
        "",
        "Execution model:",
        "- The shell tool runs non-interactive commands.",
        "- Default shell cwd is carried between tool calls. Current cwd:",
        &format!("  {}", cwd.display()),
        "- Installed command hints:",
        &format!("  {}", command_list),
        "",
        "Response contract:",
        "- If another tool action is needed, respond with ONLY valid JSON.",
        "- JSON formats:",
        "  {\"tool\":\"shell\",\"args\":{\"cmd\":\"rg -n \\\"foo\\\" .\"}}",
        "  {\"tool_calls\":[{\"tool\":\"fs.read\",\"args\":{\"path\":\"README.md\"}}]}",
        "- If task is complete, provide a normal final answer for the user (not JSON).",
        "",
        "Tool args:",
        "- shell: {\"cmd\":\"...\",\"cwd\":\"optional\",\"env\":{\"K\":\"V\"}}",
        "- fs.read: {\"path\":\"...\",\"max_bytes\":optional}",
        "- fs.write: {\"path\":\"...\",\"content\":\"...\",\"append\":false,\"create_dirs\":true}",
        "- fs.list: {\"path\":\"...\"}",
        "- mcp.list_tools / mcp.web_search are also available through the daemon.",
    ]
    .join("\n")
}

fn parse_inline_tool_calls(text: &str) -> Vec<InlineToolCall> {
    let json = match extract_json_value(text) {
        Some(value) => value,
        None => return Vec::new(),
    };
    inline_tool_calls_from_value(&json)
}

fn inline_tool_calls_from_value(value: &Value) -> Vec<InlineToolCall> {
    if let Some(obj) = value.as_object() {
        if let Some(tool) = obj.get("tool").and_then(|v| v.as_str()) {
            let args = obj.get("args").cloned().unwrap_or(Value::Null);
            return vec![InlineToolCall {
                tool: tool.to_string(),
                args,
            }];
        }
        if let Some(calls) = obj.get("tool_calls").and_then(|v| v.as_array()) {
            return calls
                .iter()
                .filter_map(|item| {
                    let tool = item.get("tool").or_else(|| item.get("name"))?.as_str()?;
                    let args = item.get("args").cloned().unwrap_or(Value::Null);
                    Some(InlineToolCall {
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
                let tool = item.get("tool").or_else(|| item.get("name"))?.as_str()?;
                let args = item.get("args").cloned().unwrap_or(Value::Null);
                Some(InlineToolCall {
                    tool: tool.to_string(),
                    args,
                })
            })
            .collect();
    }
    Vec::new()
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

fn execute_inline_tool(
    base_url: &str,
    call: &InlineToolCall,
    state: &mut InlineToolState,
    session_id: Option<&str>,
) -> Result<Value> {
    match call.tool.as_str() {
        "shell" => run_inline_shell(call.args.clone(), state),
        "fs.read" => run_inline_fs_read(call.args.clone(), state),
        "fs.write" => run_inline_fs_write(call.args.clone(), state),
        "fs.list" => run_inline_fs_list(call.args.clone(), state),
        "mcp.list_tools" | "mcp.web_search" => {
            call_daemon_tool(base_url, &call.tool, call.args.clone(), session_id)
        }
        other => Err(anyhow!("unknown tool `{other}`")),
    }
}

fn run_inline_shell(args: Value, state: &mut InlineToolState) -> Result<Value> {
    let args: InlineShellArgs = serde_json::from_value(args)
        .with_context(|| "invalid shell args (expected cmd/cwd/env)")?;
    let cmd_text = args.cmd.trim();
    if cmd_text.is_empty() {
        bail!("shell cmd is empty");
    }

    if let Some(next_cwd) = parse_simple_cd(cmd_text, &state.cwd) {
        state.cwd = next_cwd;
        return Ok(json!({
            "status": 0,
            "success": true,
            "stdout": "",
            "stderr": "",
            "cwd": state.cwd.display().to_string(),
            "note": "cwd updated via cd",
        }));
    }

    let cwd = args
        .cwd
        .map(|value| resolve_tool_path(&state.cwd, &value))
        .unwrap_or_else(|| state.cwd.clone());
    if !cwd.exists() {
        bail!("shell cwd does not exist: {}", cwd.display());
    }
    if !cwd.is_dir() {
        bail!("shell cwd is not a directory: {}", cwd.display());
    }
    state.cwd = cwd.clone();

    let (shell, shell_args) = select_inline_shell();
    let mut cmd = Command::new(shell);
    cmd.args(shell_args);
    cmd.arg(cmd_text);
    cmd.current_dir(&state.cwd);
    if let Some(envs) = args.env {
        cmd.envs(envs);
    }
    let output = cmd.output().context("failed to run shell command")?;
    Ok(json!({
        "status": output.status.code(),
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "cwd": state.cwd.display().to_string(),
    }))
}

fn run_inline_fs_read(args: Value, state: &InlineToolState) -> Result<Value> {
    let args: InlineFsReadArgs =
        serde_json::from_value(args).with_context(|| "invalid fs.read args (expected path)")?;
    let path = resolve_tool_path(&state.cwd, &args.path);
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let max_bytes = args.max_bytes.unwrap_or(256_000).clamp(1, 2_000_000) as usize;
    let mut data = bytes;
    let truncated = data.len() > max_bytes;
    if truncated {
        data.truncate(max_bytes);
    }
    Ok(json!({
        "path": path.display().to_string(),
        "truncated": truncated,
        "content": String::from_utf8_lossy(&data),
    }))
}

fn run_inline_fs_write(args: Value, state: &InlineToolState) -> Result<Value> {
    let args: InlineFsWriteArgs = serde_json::from_value(args)
        .with_context(|| "invalid fs.write args (expected path/content)")?;
    let path = resolve_tool_path(&state.cwd, &args.path);
    if args.create_dirs.unwrap_or(false) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directories for {}", path.display())
            })?;
        }
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true);
    if args.append.unwrap_or(false) {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let mut file = opts
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    use std::io::Write as _;
    file.write_all(args.content.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes_written": args.content.len(),
        "appended": args.append.unwrap_or(false),
    }))
}

fn run_inline_fs_list(args: Value, state: &InlineToolState) -> Result<Value> {
    let args: InlineFsListArgs =
        serde_json::from_value(args).with_context(|| "invalid fs.list args (expected path)")?;
    let path = resolve_tool_path(&state.cwd, &args.path);
    let mut entries = Vec::new();
    for entry in fs::read_dir(&path)
        .with_context(|| format!("failed to list {}", path.display()))?
        .flatten()
    {
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            name.push('/');
        }
        entries.push(name);
    }
    entries.sort();
    Ok(json!({
        "path": path.display().to_string(),
        "entries": entries,
    }))
}

fn call_daemon_tool(
    base_url: &str,
    name: &str,
    args: Value,
    session_id: Option<&str>,
) -> Result<Value> {
    let url = format!("{base_url}/v1/tools/{name}/call");
    let mut body = json!({
        "args": args,
        "approved": true,
        "approval_reason": "inline ai tool loop",
    });
    if let Some(session_id) = session_id {
        body["session_id"] = json!(session_id);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    match agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(resp) => {
            let text = resp.into_string().unwrap_or_default();
            let value: Value =
                serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Ok(value.get("result").cloned().unwrap_or(value))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let parsed: Value =
                serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Err(anyhow!("daemon tool call failed ({status}): {}", parsed))
        }
        Err(err) => Err(anyhow!("daemon tool call error: {err}")),
    }
}

fn resolve_tool_path(base: &Path, input: &str) -> PathBuf {
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn parse_simple_cd(cmd: &str, cwd: &Path) -> Option<PathBuf> {
    let trimmed = cmd.trim();
    if trimmed == "cd" {
        return env::var("HOME").ok().map(PathBuf::from);
    }
    let rest = trimmed.strip_prefix("cd ")?;
    if rest.contains("&&")
        || rest.contains("||")
        || rest.contains(';')
        || rest.contains('|')
        || rest.contains('\n')
        || rest.contains('\t')
    {
        return None;
    }
    let target = rest.trim().trim_matches('"').trim_matches('\'');
    if target.is_empty() {
        return None;
    }
    let resolved = if target == "~" {
        env::var("HOME").ok().map(PathBuf::from)?
    } else if let Some(suffix) = target.strip_prefix("~/") {
        PathBuf::from(env::var("HOME").ok()?).join(suffix)
    } else {
        resolve_tool_path(cwd, target)
    };
    if resolved.is_dir() {
        Some(resolved)
    } else {
        None
    }
}

fn select_inline_shell() -> (String, Vec<String>) {
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

fn inline_max_steps() -> u32 {
    env::var("AISH_INLINE_MAX_STEPS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.clamp(1, 40))
        .unwrap_or(10)
}

fn env_flag_enabled(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn detect_installed_commands() -> Vec<String> {
    let candidates = [
        "rg", "fd", "git", "python3", "cargo", "rustc", "uv", "pytest", "node", "npm", "pnpm",
        "go", "make", "just", "docker", "tmux", "jq", "curl", "sed", "awk",
    ];
    candidates
        .iter()
        .filter(|name| command_exists(name))
        .map(|name| (*name).to_string())
        .collect()
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", name))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn maybe_attach_workspace_context(prompt: &str) -> String {
    if !should_attach_workspace_context(prompt) {
        return prompt.to_string();
    }
    match collect_workspace_context() {
        Some(ctx) => format!(
            "You are answering a question about the user's local workspace.\nUse the provided workspace context as source material.\nIf information is missing, say so briefly.\n\nWorkspace context:\n{ctx}\n\nUser request:\n{prompt}"
        ),
        None => prompt.to_string(),
    }
}

fn should_attach_workspace_context(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    [
        "this repository",
        "this repo",
        "this folder",
        "this directory",
        "current directory",
        "current folder",
        "current repo",
        "this project",
        "summarize this repository",
        "summarize this folder",
    ]
    .iter()
    .any(|needle| p.contains(needle))
}

fn collect_workspace_context() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let mut lines = Vec::new();
    lines.push(format!("- cwd: {}", cwd.display()));

    let repo_root = run_command_capture(Some(&cwd), "git", &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from);
    if let Some(root) = &repo_root {
        lines.push(format!("- repo_root: {}", root.display()));
        if let Some(branch) =
            run_command_capture(Some(root), "git", &["rev-parse", "--abbrev-ref", "HEAD"])
        {
            lines.push(format!("- git_branch: {}", branch));
        }
        if let Some(status) = run_command_capture(Some(root), "git", &["status", "--short"]) {
            let status = truncate_chars(&status, 2_000);
            if !status.trim().is_empty() {
                lines.push("- git_status_short:".to_string());
                lines.push(indented_block(&status));
            }
        }
        if let Some(files) = run_command_capture(Some(root), "git", &["ls-files"]) {
            let mut listed = Vec::new();
            for line in files.lines().take(200) {
                listed.push(line.to_string());
            }
            if !listed.is_empty() {
                lines.push("- tracked_files_sample:".to_string());
                lines.push(indented_block(&listed.join("\n")));
            }
        }
    }

    let list_target = repo_root.as_ref().unwrap_or(&cwd);
    if let Some(entries) = list_dir_entries(list_target, 120) {
        lines.push("- top_level_entries:".to_string());
        lines.push(indented_block(&entries.join("\n")));
    }

    if let Some(readme) = read_readme_excerpt(list_target, 4_000) {
        lines.push("- readme_excerpt:".to_string());
        lines.push(indented_block(&readme));
    }

    Some(lines.join("\n"))
}

fn run_command_capture(cwd: Option<&Path>, program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn list_dir_entries(path: &Path, limit: usize) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut names = Vec::new();
    for entry in fs::read_dir(path).ok()?.flatten() {
        let p = entry.path();
        let mut name = entry.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    for name in names.into_iter().take(limit) {
        out.push(name);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn read_readme_excerpt(root: &Path, max_chars: usize) -> Option<String> {
    let candidates = ["README.md", "Readme.md", "readme.md"];
    for name in candidates {
        let path = root.join(name);
        if let Ok(text) = fs::read_to_string(path) {
            let trimmed = truncate_chars(text.trim(), max_chars);
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn indented_block(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {}", line))
        .collect::<Vec<_>>()
        .join("\n")
}

enum StreamChannelMessage {
    Connected,
    Event {
        event_type: Option<String>,
        data: String,
    },
    End,
}

struct ProgressLine {
    enabled: bool,
    current_status: String,
    started_at: Instant,
    last_rendered_at: Instant,
    spinner_idx: usize,
}

impl ProgressLine {
    fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            current_status: "Packing context...".to_string(),
            started_at: now,
            last_rendered_at: now.checked_sub(Duration::from_millis(120)).unwrap_or(now),
            spinner_idx: 0,
        }
    }

    fn set_status(&mut self, status: &str) {
        if self.current_status == status {
            return;
        }
        self.current_status = status.to_string();
        self.render(true);
    }

    fn tick(&mut self) {
        self.render(false);
    }

    fn clear(&mut self) {
        if !self.enabled {
            return;
        }
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }

    fn stop(&mut self) {
        self.clear();
        self.enabled = false;
    }

    fn render(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if !force && now.duration_since(self.last_rendered_at) < Duration::from_millis(120) {
            return;
        }
        let spinner = ["|", "/", "-", "\\"];
        let frame = spinner[self.spinner_idx % spinner.len()];
        self.spinner_idx = self.spinner_idx.wrapping_add(1);
        let elapsed = now.duration_since(self.started_at).as_secs();
        eprint!(
            "\r\x1b[2K[ai] {} {} ({}s)",
            frame, self.current_status, elapsed
        );
        let _ = std::io::stderr().flush();
        self.last_rendered_at = now;
    }
}

fn stream_completion_response(url: &str, body: &Value) -> Result<String> {
    let mut progress = ProgressLine::new(std::io::stderr().is_terminal() && ai_progress_enabled());
    progress.render(true);
    let (tx, rx) = mpsc::channel::<StreamChannelMessage>();
    let request_url = url.to_string();
    let request_body = body.to_string();
    let worker = thread::spawn(move || stream_completion_worker(&request_url, &request_body, tx));

    let mut full_text = String::new();
    let mut done = false;

    loop {
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(StreamChannelMessage::Connected) => {
                progress.set_status("Waiting for first token from provider...");
            }
            Ok(StreamChannelMessage::Event { event_type, data }) => {
                if handle_sse_event(event_type.as_deref(), &data, &mut full_text, &mut progress)? {
                    done = true;
                    break;
                }
            }
            Ok(StreamChannelMessage::End) => break,
            Err(RecvTimeoutError::Timeout) => progress.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    progress.clear();
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if !done && full_text.is_empty() {
                return Err(err);
            }
        }
        Err(_) => bail!("stream worker panicked"),
    }
    if !done && full_text.is_empty() {
        bail!("no completion text returned");
    }
    Ok(full_text)
}

fn ai_progress_enabled() -> bool {
    match env::var("AISH_UI_PROGRESS") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn stream_completion_worker(url: &str, body: &str, tx: Sender<StreamChannelMessage>) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    let resp = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .send_string(body)
        .with_context(|| "failed to call aishd /v1/completions/stream")?;
    let _ = tx.send(StreamChannelMessage::Connected);

    let mut reader = BufReader::new(resp.into_reader());
    let mut line = String::new();
    let mut event_type: Option<String> = None;
    let mut data = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            if !data.is_empty() {
                let _ = tx.send(StreamChannelMessage::Event {
                    event_type: event_type.clone(),
                    data: data.clone(),
                });
            }
            let _ = tx.send(StreamChannelMessage::End);
            break;
        }

        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            if !data.is_empty() {
                let _ = tx.send(StreamChannelMessage::Event {
                    event_type: event_type.clone(),
                    data: data.clone(),
                });
                data.clear();
            }
            event_type = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            event_type = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }

    Ok(())
}

fn handle_sse_event(
    event_type: Option<&str>,
    data: &str,
    full_text: &mut String,
    progress: &mut ProgressLine,
) -> Result<bool> {
    match event_type.unwrap_or("delta") {
        "status" => {
            let parsed: Value = serde_json::from_str(data).unwrap_or_else(|_| json!({}));
            if let Some(status) = parsed.get("status").and_then(|v| v.as_str()) {
                progress.set_status(status);
            }
            Ok(false)
        }
        "delta" => {
            let parsed: Value = serde_json::from_str(data).unwrap_or_else(|_| json!({}));
            let delta = parsed
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !delta.is_empty() {
                if full_text.is_empty() {
                    progress.stop();
                }
                print!("{delta}");
                std::io::stdout().flush()?;
                full_text.push_str(delta);
            }
            Ok(false)
        }
        "done" => {
            if full_text.is_empty() {
                let parsed: Value = serde_json::from_str(data).unwrap_or_else(|_| json!({}));
                if let Some(text) = parsed.get("text").and_then(|v| v.as_str()) {
                    progress.stop();
                    full_text.push_str(text);
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
            }
            progress.stop();
            Ok(true)
        }
        "error" => {
            let parsed: Value = serde_json::from_str(data).unwrap_or_else(|_| json!({}));
            let msg = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("streaming request failed");
            progress.stop();
            bail!("{msg}");
        }
        _ => Ok(false),
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

fn read_stdin_to_string() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn show_status(cfg: &Config) -> Result<()> {
    let base = resolve_path(&cfg.logging.dir);
    if !base.exists() {
        println!("no logs at {}", base.display());
        return Ok(());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().ok();
        let modified = meta.and_then(|m| m.modified().ok());
        let last_event = read_last_event(&path.join("events.jsonl"));
        sessions.push((session_id, path, modified, last_event));
    }

    sessions.sort_by_key(|item| item.2);
    sessions.reverse();

    for (session_id, path, modified, last_event) in sessions {
        let ts = modified
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last_kind = last_event
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!(
            "{}  last={}  kind={}  dir={}",
            session_id,
            ts,
            last_kind,
            path.display()
        );
    }
    Ok(())
}

fn read_last_event(path: &Path) -> Option<Value> {
    let contents = fs::read_to_string(path).ok()?;
    let line = contents.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        handle_sse_event, inline_agent_system_prompt, normalize_cli_args, parse_inline_tool_calls,
        parse_simple_cd, should_attach_workspace_context, ProgressLine,
    };
    use std::path::Path;

    #[test]
    fn normalize_cli_args_inserts_llm_for_prompt() {
        let args = vec!["ai".to_string(), "Count to 5".to_string()];
        let out = normalize_cli_args(args);
        assert_eq!(out, vec!["ai", "llm", "Count to 5"]);
    }

    #[test]
    fn normalize_cli_args_keeps_known_subcommand() {
        let args = vec!["ai".to_string(), "launch".to_string()];
        let out = normalize_cli_args(args);
        assert_eq!(out, vec!["ai", "launch"]);
    }

    #[test]
    fn normalize_cli_args_handles_config_prefix() {
        let args = vec![
            "ai".to_string(),
            "--config".to_string(),
            "/tmp/aish.json".to_string(),
            "--provider".to_string(),
            "local-3000".to_string(),
            "Count".to_string(),
        ];
        let out = normalize_cli_args(args);
        assert_eq!(
            out,
            vec![
                "ai",
                "--config",
                "/tmp/aish.json",
                "llm",
                "--provider",
                "local-3000",
                "Count"
            ]
        );
    }

    #[test]
    fn handle_sse_event_delta_appends_text() {
        let mut full = String::new();
        let mut progress = ProgressLine::new(false);
        let done = handle_sse_event(
            Some("delta"),
            r#"{"delta":"abc"}"#,
            &mut full,
            &mut progress,
        )
        .unwrap();
        assert!(!done);
        assert_eq!(full, "abc");
    }

    #[test]
    fn handle_sse_event_done_stops_stream() {
        let mut full = String::new();
        let mut progress = ProgressLine::new(false);
        let done = handle_sse_event(
            Some("done"),
            r#"{"text":"final"}"#,
            &mut full,
            &mut progress,
        )
        .unwrap();
        assert!(done);
        assert_eq!(full, "final");
    }

    #[test]
    fn handle_sse_event_status_updates_progress_label() {
        let mut full = String::new();
        let mut progress = ProgressLine::new(false);
        let done = handle_sse_event(
            Some("status"),
            r#"{"status":"Selecting relevant context..."}"#,
            &mut full,
            &mut progress,
        )
        .unwrap();
        assert!(!done);
        assert_eq!(progress.current_status, "Selecting relevant context...");
    }

    #[test]
    fn should_attach_workspace_context_detects_repo_phrasing() {
        assert!(should_attach_workspace_context(
            "Summarize this repository in 5 bullets."
        ));
        assert!(should_attach_workspace_context(
            "Summarize this folder / repository in 5 bullets."
        ));
    }

    #[test]
    fn should_attach_workspace_context_ignores_general_prompts() {
        assert!(!should_attach_workspace_context("Count to 5 slowly."));
    }

    #[test]
    fn inline_agent_system_prompt_includes_command_hints() {
        let prompt = inline_agent_system_prompt(
            Path::new("/tmp/work"),
            &["rg".to_string(), "git".to_string()],
        );
        assert!(prompt.contains("fs.write"));
        assert!(prompt.contains("rg, git"));
        assert!(prompt.contains("/tmp/work"));
    }

    #[test]
    fn parse_inline_tool_calls_handles_single_and_array() {
        let single = parse_inline_tool_calls(r#"{"tool":"shell","args":{"cmd":"ls"}}"#);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].tool, "shell");

        let array = parse_inline_tool_calls(
            r#"{"tool_calls":[{"tool":"fs.read","args":{"path":"README.md"}}]}"#,
        );
        assert_eq!(array.len(), 1);
        assert_eq!(array[0].tool, "fs.read");
    }

    #[test]
    fn parse_simple_cd_supports_home_and_relative() {
        let root = std::env::temp_dir().join(format!("aish_test_cd_{}", std::process::id()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cwd = root.as_path();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let target_home = parse_simple_cd("cd", cwd).unwrap();
        assert_eq!(target_home, Path::new(&home));
        assert_eq!(parse_simple_cd("cd repo", cwd).unwrap(), repo.as_path());
        assert!(parse_simple_cd("cd a && ls", cwd).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
