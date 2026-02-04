use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use aish_core::config::{self, Config};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use serde_json::{json, Map, Value};

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
    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;

    match cli.command.unwrap_or(CliCommand::Launch) {
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
            model,
            max_tokens,
            temperature,
            top_p,
            stop,
        } => {
            run_llm(&cfg, prompt, model, max_tokens, temperature, top_p, stop)?;
        }
        CliCommand::Status => {
            show_status(&cfg)?;
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
    let (session_id, log_dir, actor) =
        resolve_session_and_dir(cfg, session, log_dir, actor)?;

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
    Ok(())
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

    let base_url = format!("http://{}:{}", cfg.server.hostname, cfg.server.port);
    let url = format!("{base_url}/v1/completions");

    let mut body = json!({
        "prompt": prompt,
    });
    if let Some(model) = model.clone() {
        body["model"] = json!(model);
    }
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = top_p {
        body["top_p"] = json!(top_p);
    }
    if !stop.is_empty() {
        body["stop"] = json!(stop);
    }

    let _ = write_log_event_value(
        cfg,
        "llm.request".to_string(),
        json!({
            "prompt": body["prompt"],
            "model": body.get("model").cloned().unwrap_or(Value::Null),
        }),
        None,
        None,
        None,
    );

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
        println!("{out}");
        let _ = write_log_event_value(
            cfg,
            "llm.response".to_string(),
            json!({ "text": out }),
            None,
            None,
            None,
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or(text));
    }
    Ok(())
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
