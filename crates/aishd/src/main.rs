use anyhow::{bail, Result};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use clap::Parser;
use aish_core::config::{self, Config, OpenAICompatConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::time::Duration;
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
