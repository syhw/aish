use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub tmux: TmuxConfig,
    pub logging: LoggingConfig,
    pub share: String,
    pub providers: ProvidersConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            tmux: TmuxConfig::default(),
            logging: LoggingConfig::default(),
            share: "manual".to_string(),
            providers: ProvidersConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub hostname: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            hostname: "127.0.0.1".to_string(),
            port: 4096,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TmuxConfig {
    pub session_prefix: String,
    pub attach_on_start: bool,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            session_prefix: "aish".to_string(),
            attach_on_start: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub dir: String,
    pub retention_days: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: "~/.local/share/aish/logs".to_string(),
            retention_days: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub openai_compat: Option<OpenAICompatConfig>,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self { openai_compat: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAICompatConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub completions_path: String,
}

impl Default for OpenAICompatConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: "glm-4.7".to_string(),
            completions_path: "/completions".to_string(),
        }
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<Config> {
    let mut path = resolve_path(path.as_ref());
    if !path.exists() {
        if let Some(default_path) = default_config_path() {
            if default_path != path && default_path.exists() {
                path = default_path;
            } else {
                let mut cfg = Config::default();
                apply_env_overrides(&mut cfg);
                return Ok(cfg);
            }
        } else {
            let mut cfg = Config::default();
            apply_env_overrides(&mut cfg);
            return Ok(cfg);
        }
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    let mut cfg: Config = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse config: {}", path.display()))?;
    apply_env_overrides(&mut cfg);
    Ok(cfg)
}

fn default_config_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(Path::new(&home).join(".config/aish/aish.json"))
}

fn resolve_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(home) = env::var("HOME").ok() {
        if raw == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return Path::new(&home).join(rest);
        }
    }
    path.to_path_buf()
}

fn apply_env_overrides(cfg: &mut Config) {
    let base_url = env::var("AISH_OPENAI_COMPAT_BASE_URL").ok();
    let api_key = env::var("AISH_OPENAI_COMPAT_API_KEY")
        .ok()
        .or_else(|| env::var("ZAI_API_KEY").ok());
    let model = env::var("AISH_OPENAI_COMPAT_MODEL").ok();
    let completions_path = env::var("AISH_OPENAI_COMPAT_COMPLETIONS_PATH").ok();

    if base_url.is_some() || api_key.is_some() || model.is_some() || completions_path.is_some() {
        let provider = cfg
            .providers
            .openai_compat
            .get_or_insert_with(OpenAICompatConfig::default);
        if let Some(value) = base_url {
            provider.base_url = value;
        }
        if let Some(value) = api_key {
            provider.api_key = value;
        }
        if let Some(value) = model {
            provider.model = value;
        }
        if let Some(value) = completions_path {
            provider.completions_path = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_config_path(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let pid = std::process::id();
        let filename = format!("aish_test_{name}_{ts}_{pid}.json");
        std::env::temp_dir().join(filename)
    }

    #[test]
    fn zai_api_key_populates_provider() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("ZAI_API_KEY", "test-zai-key");
        std::env::remove_var("AISH_OPENAI_COMPAT_API_KEY");

        let path = temp_config_path("zai_key");
        fs::write(&path, "{}").unwrap();
        let cfg = load(&path).unwrap();
        fs::remove_file(&path).ok();

        let provider = cfg.providers.openai_compat.expect("provider missing");
        assert_eq!(provider.api_key, "test-zai-key");

        std::env::remove_var("ZAI_API_KEY");
    }

    #[test]
    fn aish_api_key_overrides_zai_api_key() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("ZAI_API_KEY", "zai-key");
        std::env::set_var("AISH_OPENAI_COMPAT_API_KEY", "aish-key");

        let path = temp_config_path("aish_key");
        fs::write(&path, "{}").unwrap();
        let cfg = load(&path).unwrap();
        fs::remove_file(&path).ok();

        let provider = cfg.providers.openai_compat.expect("provider missing");
        assert_eq!(provider.api_key, "aish-key");

        std::env::remove_var("ZAI_API_KEY");
        std::env::remove_var("AISH_OPENAI_COMPAT_API_KEY");
    }
}
