use crate::types::{LLMConfig, Settings};
use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use tracing::{error, info};

/// Global cached config — loaded once at startup, updated on model switch.
static CONFIG: OnceLock<Arc<RwLock<Settings>>> = OnceLock::new();

/// Initialize the global config cache. Must be called once at startup.
pub fn init_config() {
    let settings = Settings::load_from_disk();
    info!(
        "📋 Config loaded: active={} models={}",
        settings.active_llm,
        settings.llms.len()
    );
    CONFIG
        .set(Arc::new(RwLock::new(settings)))
        .expect("config already initialized");
}

/// Returns the path to settings.json, searching executable dir then cwd.
fn settings_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("settings.json");
        if p.exists() {
            return p;
        }
    }
    let p = PathBuf::from("assets/settings.json");
    if p.exists() {
        return p;
    }
    PathBuf::from("settings.json")
}

/// Reads config from the cache (no disk I/O).
fn with_config<F, R>(f: F) -> R
where
    F: FnOnce(&Settings) -> R,
{
    let guard = CONFIG
        .get()
        .expect("config not initialized — call init_config() first")
        .read()
        .expect("config RwLock poisoned");
    f(&guard)
}

/// Mutates config in the cache + persists to disk.
fn update_config<F>(f: F) -> Result<(), String>
where
    F: FnOnce(&mut Settings),
{
    let arc = CONFIG
        .get()
        .expect("config not initialized — call init_config() first");
    let mut guard = arc.write().expect("config RwLock poisoned");
    f(&mut guard);
    guard
        .save_to(&settings_path())
        .map_err(|e| format!("Failed to save config: {}", e))
}

impl Settings {
    fn default_config() -> Self {
        let mut llms = IndexMap::new();
        llms.insert(
            "默认本地大模型".into(),
            LLMConfig {
                base_url: "http://127.0.0.1:8000/v1".into(),
                model_name: "default-model".into(),
                context_max_length: crate::types::default_context_max_length(),
                api_key: "none".into(),
                auth_header: crate::types::default_auth_header(),
                auth_prefix: crate::types::default_auth_prefix(),
                supports_native_function_calling: false,
                thinking: true,
                reasoning_effort: crate::types::default_reasoning_effort(),
                stream: true,
            },
        );
        Settings {
            active_llm: "默认本地大模型".into(),
            llms,
        }
    }

    /// Loads settings from disk, creating defaults if file doesn't exist or is invalid.
    fn load_from_disk() -> Self {
        let path = settings_path();
        if !path.exists() {
            info!("Generating default config: {}", path.display());
            let d = Self::default_config();
            let _ = d.save_to(&path);
            return d;
        }
        match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<Settings>(&data) {
                Ok(cfg) => cfg,
                Err(e) => {
                    error!("Config parse error: {} — using default", e);
                    Self::default_config()
                }
            },
            Err(e) => {
                error!("Config read error: {} — using default", e);
                Self::default_config()
            }
        }
    }

    /// Atomic save: write to temp file, then rename to avoid corruption.
    fn save_to(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let data = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Returns all configured LLM names (from cache).
pub fn get_llm_names() -> Vec<String> {
    with_config(|s| s.llms.keys().cloned().collect())
}

/// Returns the name of the currently active LLM (from cache).
pub fn get_active_llm_name() -> String {
    with_config(|s| s.active_llm.clone())
}

/// Returns the configuration of the active LLM, if available (from cache).
pub fn get_active_llm_config() -> Option<LLMConfig> {
    with_config(|s| {
        let key = &s.active_llm;
        if key.is_empty() {
            return None;
        }
        s.llms.get(key).cloned().or_else(|| {
            error!("LLM '{}' not found in config", key);
            None
        })
    })
}

/// Atomically switches the active LLM and persists to disk.
pub fn switch_active_llm(name: &str) -> Result<(), String> {
    // Validate existence before acquiring write lock
    let exists = with_config(|s| s.llms.contains_key(name));
    if !exists {
        return Err(format!("LLM '{}' not found", name));
    }

    update_config(|s| {
        s.active_llm = name.to_string();
    })?;

    info!("Switched active LLM → {}", name);
    Ok(())
}
