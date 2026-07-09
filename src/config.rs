use crate::types::{LLMConfig, Settings};
use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::RwLock;
use tracing::{error, info, warn};

static SETTINGS_PATH: once_cell::sync::Lazy<PathBuf> = once_cell::sync::Lazy::new(|| {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.parent().unwrap_or(std::path::Path::new(".")).join("settings.json");
        if p.exists() { return p; }
    }
    let p = PathBuf::from("assets/settings.json");
    if p.exists() { return p; }
    PathBuf::from("settings.json")
});

static SETTINGS: once_cell::sync::Lazy<RwLock<Settings>> =
    once_cell::sync::Lazy::new(|| RwLock::new(Settings::load()));

impl Settings {
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
        Settings { auto_select: false, active_llm: "默认本地大模型".into(), llms }
    }

    pub fn load() -> Self {
        let path = SETTINGS_PATH.as_path();
        if !path.exists() {
            info!("generating default config: {}", path.display());
            let d = Self::default_config();
            let _ = d.save_to(path);
            return d;
        }
        match std::fs::read_to_string(path) {
            Ok(data) => match serde_json::from_str::<Settings>(&data) {
                Ok(cfg) => cfg,
                Err(e) => { error!("config parse error: {} — using default", e); Self::default_config() }
            },
            Err(e) => { error!("config read error: {} — using default", e); Self::default_config() }
        }
    }

    pub fn save(&self) {
        let _ = self.save_to(SETTINGS_PATH.as_path());
    }

    fn save_to(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

pub fn is_auto_select() -> bool {
    SETTINGS.read().unwrap().auto_select
}

pub fn switch_auto_select(enable: bool) {
    let mut s = SETTINGS.write().unwrap();
    s.auto_select = enable;
    s.save();
    info!("auto_select: {}", if enable { "ON" } else { "OFF" });
}

pub fn get_llm_names() -> Vec<String> {
    Settings::load().llms.keys().cloned().collect()
}

pub fn get_llm_names_cached() -> Vec<String> {
    SETTINGS.read().unwrap().llms.keys().cloned().collect()
}

pub fn get_active_llm_name() -> String {
    SETTINGS.read().unwrap().active_llm.clone()
}

pub fn get_active_llm_config() -> Option<LLMConfig> {
    let s = Settings::load();
    let key = &s.active_llm;
    if key.is_empty() { return None; }
    s.llms.get(key).cloned().or_else(|| {
        error!("LLM '{}' not found in config", key);
        None
    })
}

pub fn switch_active_llm(name: &str) -> Result<(), String> {
    let mut s = SETTINGS.write().unwrap();
    if !s.llms.contains_key(name) {
        return Err(format!("LLM '{}' not found", name));
    }
    s.active_llm = name.to_string();
    s.save();
    info!("switch LLM → {}", name);
    Ok(())
}

/// 自动 fallback：返回按顺序的下一个 LLM
pub fn auto_fallback_llm() -> Option<(String, LLMConfig)> {
    let s = Settings::load();
    let names: Vec<String> = s.llms.keys().cloned().collect();
    let cur = s.active_llm.clone();
    let pos = names.iter().position(|n| *n == cur).unwrap_or(0);
    if pos + 1 >= names.len() {
        warn!("auto_fallback: '{}' is last LLM", cur);
        return None;
    }
    let next_name = names[pos + 1].clone();
    let config = s.llms.get(&next_name).cloned()?;
    // 切换到下一个
    let mut sw = SETTINGS.write().unwrap();
    sw.active_llm = next_name.clone();
    sw.save();
    info!("auto_fallback: {} → {}", cur, next_name);
    Some((next_name, config))
}
