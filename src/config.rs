use crate::types::{LLMConfig, Settings};
use indexmap::IndexMap;
use std::path::PathBuf;
use tracing::{error, info};

// 查找配置文件的真实路径
static SETTINGS_PATH: once_cell::sync::Lazy<PathBuf> = once_cell::sync::Lazy::new(|| {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.parent().unwrap_or(std::path::Path::new(".")).join("settings.json");
        if p.exists() { return p; }
    }
    let p = PathBuf::from("assets/settings.json");
    if p.exists() { return p; }
    PathBuf::from("settings.json")
});

// [修改] 移除了容易导致状态过期并错误覆盖本地文件的 static SETTINGS 全局内存缓存

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
        Settings { active_llm: "默认本地大模型".into(), llms }
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

pub fn get_llm_names() -> Vec<String> {
    Settings::load().llms.keys().cloned().collect()
}

pub fn get_active_llm_name() -> String {
    // [修改] 直接从磁盘获取，保证状态永远是最新
    Settings::load().active_llm
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
    // [修改] 先从磁盘 load 最新配置，然后在此基础上修改 active_llm 再保存
    // 这样就不会因为内存数据过期而覆盖掉用户手动修改的参数 (比如 context_max_length)
    let mut s = Settings::load();
    
    if !s.llms.contains_key(name) {
        return Err(format!("LLM '{}' not found", name));
    }
    s.active_llm = name.to_string();
    s.save();
    info!("switch LLM → {}", name);
    Ok(())
}
