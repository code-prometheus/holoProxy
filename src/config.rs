use crate::types::{LLMConfig, Settings};
use indexmap::IndexMap;
use std::path::PathBuf;
use tracing::{error, info};

/// Lazily-initialized path to the settings configuration file.
/// Searches in order: executable directory, assets/, then current directory.
static SETTINGS_PATH: once_cell::sync::Lazy<PathBuf> = once_cell::sync::Lazy::new(|| {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.parent().unwrap_or(std::path::Path::new(".")).join("settings.json");
        if p.exists() { return p; }
    }
    let p = PathBuf::from("assets/settings.json");
    if p.exists() { return p; }
    PathBuf::from("settings.json")
});

impl Settings {
    /// Creates a default configuration with a single local LLM entry
    fn default_config() -> Self {
        let mut llms = IndexMap::new();
        llms.insert(
            "默认本地大模型".into(),
            LLMConfig {
                base_url: "http://127.0.0.1:8000/v1".into(),
                model_name: "default-model".into(),
                context_max_length: crate::types::default_context_max_length(),
                verify_ssl: false,
                api_key: "none".into(),
                auth_header: crate::types::default_auth_header(),
                auth_prefix: crate::types::default_auth_prefix(),
                supports_native_function_calling: false,
                supports_reasoning_content: false,
            },
        );
        Settings { active_llm: "默认本地大模型".into(), llms }
    }

    /// Loads settings from disk, creating defaults if file doesn't exist or is invalid
    pub fn load() -> Self {
        let path = SETTINGS_PATH.as_path();
        if !path.exists() {
            info!("Generating default config: {}", path.display());
            let d = Self::default_config();
            let _ = d.save_to(path);
            return d;
        }
        match std::fs::read_to_string(path) {
            Ok(data) => match serde_json::from_str::<Settings>(&data) {
                Ok(cfg) => cfg,
                Err(e) => { error!("Config parse error: {} — using default", e); Self::default_config() }
            },
            Err(e) => { error!("Config read error: {} — using default", e); Self::default_config() }
        }
    }

    /// Persists current settings to disk
    pub fn save(&self) {
        let _ = self.save_to(SETTINGS_PATH.as_path());
    }

    /// Saves settings to a specific path
    fn save_to(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// Returns all configured LLM names
pub fn get_llm_names() -> Vec<String> {
    Settings::load().llms.keys().cloned().collect()
}

/// Returns the name of the currently active LLM
pub fn get_active_llm_name() -> String {
    // Always loads fresh from disk to ensure latest state
    Settings::load().active_llm
}

/// Returns the configuration of the active LLM, if available
pub fn get_active_llm_config() -> Option<LLMConfig> {
    let s = Settings::load();
    let key = &s.active_llm;
    if key.is_empty() { return None; }
    s.llms.get(key).cloned().or_else(|| {
        error!("LLM '{}' not found in config", key);
        None
    })
}

/// Switches the active LLM to the specified name
pub fn switch_active_llm(name: &str) -> Result<(), String> {
    // Loads fresh config from disk before modifying to avoid stale state
    let mut s = Settings::load();
    
    if !s.llms.contains_key(name) {
        return Err(format!("LLM '{}' not found", name));
    }
    s.active_llm = name.to_string();
    s.save();
    info!("Switched active LLM → {}", name);
    Ok(())
}
