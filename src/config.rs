use crate::types::{LLMConfig, Settings};
use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::RwLock;
use tracing::{error, info, warn};

static SETTINGS_PATH: once_cell::sync::Lazy<PathBuf> = once_cell::sync::Lazy::new(|| {
    // 1. exe 目录下的 settings.json
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.parent().unwrap_or(std::path::Path::new(".")).join("settings.json");
        if p.exists() { return p; }
    }
    // 2. assets/settings.json (用户放进来的真实配置)
    let p = PathBuf::from("assets/settings.json");
    if p.exists() { return p; }
    // 3. 当前目录下的 settings.json
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
        Settings {
            auto_select: false,
            active_llm: "默认本地大模型".into(),
            llms,
        }
    }

    pub fn load() -> Self {
        let path = SETTINGS_PATH.as_path();
        if !path.exists() {
            info!("✨ 首次运行，生成默认配置: {}", path.display());
            let default_cfg = Self::default_config();
            if let Err(e) = default_cfg.save_to(path) {
                error!("❌ 无法生成默认配置文件: {}", e);
            }
            return default_cfg;
        }
        match std::fs::read_to_string(path) {
            Ok(data) => match serde_json::from_str::<Settings>(&data) {
                Ok(cfg) => cfg,
                Err(e) => {
                    error!("❌ 配置文件损坏: {}，使用默认配置", e);
                    Self::default_config()
                }
            },
            Err(e) => {
                error!("❌ 无法读取配置文件: {}，使用默认配置", e);
                Self::default_config()
            }
        }
    }

    pub fn save(&self) {
        if let Err(e) = self.save_to(SETTINGS_PATH.as_path()) {
            error!("❌ 保存配置失败: {}", e);
        }
    }

    fn save_to(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// 是否开启了自动选择模式
pub fn is_auto_select() -> bool {
    let settings = SETTINGS.read().unwrap();
    settings.auto_select
}

/// 切换自动选择开关
pub fn switch_auto_select(enable: bool) {
    let mut settings = SETTINGS.write().unwrap();
    settings.auto_select = enable;
    settings.save();
    info!("🔄 自动选择模式: {}", if enable { "开启" } else { "关闭" });
}

/// 获取 LLM 名称列表（按 settings.json 中的顺序）
pub fn get_llm_names() -> Vec<String> {
    // 每次重新读取以保证顺序正确
    let settings = Settings::load();
    settings.llms.keys().cloned().collect()
}

/// 获取所有 LLM 名称（从内存缓存读取，用于托盘菜单快速响应）
pub fn get_llm_names_cached() -> Vec<String> {
    let settings = SETTINGS.read().unwrap();
    settings.llms.keys().cloned().collect()
}

/// 获取当前激活的 LLM 名称
pub fn get_active_llm_name() -> String {
    let settings = SETTINGS.read().unwrap();
    settings.active_llm.clone()
}

/// 动态获取当前激活的 LLM 配置（每次重新读取文件，实现热加载）
pub fn get_active_llm_config() -> Option<LLMConfig> {
    let settings = Settings::load();
    let key = settings.active_llm.clone();
    if key.is_empty() {
        return None;
    }
    let config = settings.llms.get(&key).cloned();
    if config.is_none() {
        error!("❌ 配置中未找到 LLM: {}", key);
    }
    config
}

/// 切换激活的 LLM
pub fn switch_active_llm(name: &str) -> Result<(), String> {
    let mut settings = SETTINGS.write().unwrap();
    if !settings.llms.contains_key(name) {
        return Err(format!("LLM '{}' 不存在", name));
    }
    settings.active_llm = name.to_string();
    settings.save();
    info!("🔄 切换激活 LLM → {}", name);
    Ok(())
}

/// 自动模式 fallback：返回当前 LLM 的下一个（按顺序）的 LLM 名称和配置
/// 如果已到最后一个则返回 None（全部试过了）
pub fn auto_fallback_llm() -> Option<(String, LLMConfig)> {
    let settings = Settings::load();
    let names: Vec<String> = settings.llms.keys().cloned().collect();
    let current = settings.active_llm.clone();

    // 找到当前 LLM 的位置
    let current_pos = names.iter().position(|n| *n == current).unwrap_or(0);
    if current_pos + 1 >= names.len() {
        warn!(
            "🚨 [AutoSelect] 已到达最后一个 LLM '{}'，没有更多 fallback",
            names.last().map_or("unknown", |n| n.as_str())
        );
        return None;
    }

    let next_name = names[current_pos + 1].clone();
    if let Some(config) = settings.llms.get(&next_name).cloned() {
        // 自动切换到下一个
        drop(settings);
        let mut settings = SETTINGS.write().unwrap();
        settings.active_llm = next_name.clone();
        settings.save();
        info!("🔄 [AutoSelect] 自动 fallback: '{}' → '{}'", current, next_name);
        Some((next_name, config))
    } else {
        None
    }
}
