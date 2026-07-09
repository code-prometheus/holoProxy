# Rust Windows 托盘 EXE 编译及菜单交互经验总结

## 项目背景

用 Rust (axum + tray-icon + winit) 构建一个 Windows 系统托盘应用，同时运行 HTTP 服务器。托盘右键菜单支持切换模型和退出。

---

## 问题 1: `#![windows_subsystem = "windows"]` 隐藏控制台

### 解决方案
```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```
- `cargo build` (debug) 保留控制台，`cargo build --release` 隐藏控制台

---

## 问题 2: winit EventLoop 必须在主线程

### 现象
```
thread '<unnamed>' panicked: Initializing the event loop outside of the main thread
```

### 根因
winit 0.30 的 `EventLoop::new()` 默认拒绝在非主线程创建。

### 解决方案
**主线程** 运行 winit event loop + tray，**后台线程** 跑 tokio runtime + axum HTTP 服务：
```rust
fn main() {
    // HTTP 在后台线程跑 tokio
    std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        rt.block_on(async { axum::serve(...).await.unwrap() });
    });

    // 主线程跑 winit event loop + tray
    tray::run_tray();
}
```

---

## 问题 3: tray-icon 图标加载

### 两种加载方式
- **`Icon::from_path(path, size)`** — Windows 原生 `LoadImageW`，最可靠，需 `assets/icon.ico`
- **`Icon::from_rgba(rgba, width, height)`** — 程序化生成，fallback 方案
- `build.rs` 自动复制 `assets/` 到 `target/{profile}/assets/`

---

## 问题 4: 托盘菜单事件接收（核心难点）

### 4.1 菜单 ID 设置

**必须使用 `MenuItem::with_id()` 构造菜单项**，不能依赖 `MenuItem::new()` 的自动 ID：

```rust
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};

let menu = Menu::new();
for model in &models {
    let label = format!("✓ {}", model);
    // ⚡ with_id 绑定模型名作为菜单 ID，事件中可精确匹配
    menu.append(&MenuItem::with_id(MenuId::new(model.clone()), label, true, None)).ok();
}
menu.append(&PredefinedMenuItem::separator()).ok();
menu.append(&MenuItem::with_id(MenuId::new("__quit__".to_string()), "退出", true, None)).ok();
```

### 4.2 菜单事件接收（经过大量试错后的唯一可行方案）

**关键三步**，缺一不可：

#### 步骤 1: 在构建菜单之前获取 MenuEvent receiver
```rust
let menu_event_rx = MenuEvent::receiver().clone();  // ⚡ 必须在 build 之前！
```

#### 步骤 2: 使用 `ApplicationHandler::about_to_wait` 轮询
**不能**用 `EventLoop::run()` 的同步闭包，**不能**用 `set_event_handler`。

必须实现 `winit::application::ApplicationHandler` trait，在 `about_to_wait` 中 `try_recv`：

```rust
struct TrayApp {
    menu_event_rx: crossbeam_channel::Receiver<tray_icon::menu::MenuEvent>,
    tray: Option<tray_icon::TrayIcon>,
}

impl winit::application::ApplicationHandler for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // ⚡ 在这里 try_recv 是唯一能收到菜单事件的方式
        while let Ok(event) = self.menu_event_rx.try_recv() {
            handle_menu_id(&event.id.0, self.tray.as_mut().unwrap());
        }
    }
}
```

#### 步骤 3: 用 `run_app` 代替 `run` 启动事件循环
```rust
let event_loop = EventLoop::new()?;
let mut app = TrayApp { menu_event_rx, tray: Some(tray) };
event_loop.run_app(&mut app)?;  // ⚡ run_app，不是 run
```

### 4.3 模型切换后重建菜单（更新勾号）

```rust
fn handle_menu_id(id_str: &str, tray: &mut tray_icon::TrayIcon) {
    if id_str == "__quit__" {
        std::process::exit(0);
    }
    // 切换模型
    crate::config::switch_active_llm(id_str).ok();
    // ⚡ 重建菜单：用 build_menu() 重新生成（勾号位置自动更新）
    tray.set_menu(Some(Box::new(build_menu()))).ok();
    tray.set_tooltip(Some(format!("holoProxy - {}", get_active_llm_name()))).ok();
}
```

### 4.4 试错历程记录（失败方案汇总）

| 方案 | 结果 | 原因 |
|------|------|------|
| `EventLoop::run` + `ControlFlow::Wait` + `try_recv` | ❌ | Wait 阻塞消息泵 |
| `EventLoop::run` + `ControlFlow::Poll` + `try_recv` | ❌ | winit 不 dispatch tray-icon 窗口消息 |
| `EventLoop::run_on_demand` + `Poll` + `try_recv` | ❌ | 同上 |
| `MenuEvent::set_event_handler` + `EventLoopProxy` | ❌ | handler 被调用但 proxy 消息被 winit 吞掉 |
| 原生 `PeekMessageW` + `DispatchMessageW` 消息泵 | ❌ | 收到 0 条消息 — tray-icon 的 `WM_COMMAND` 通过 `SendMessageW` 同步处理，不经过消息队列 |
| `std::thread::spawn` 阻塞 `recv()` | ❌ | `MenuEvent::send` 根本未被触发 |
| **`ApplicationHandler::about_to_wait` + `try_recv`** | ✅ | **唯一可行方案** |

### 4.5 核心教训

- **`ApplicationHandler::about_to_wait`** 是 winit 0.30 中每个空闲周期都调用的钩子，在这里 `try_recv` 不会被消息泵阻塞
- **`MenuEvent::receiver()` 必须先于 `TrayIconBuilder::build()` 获取**
- **`MenuItem::with_id` 必须用，不能依赖自动 ID**
- **放弃 `set_event_handler`**，它在这个组合中不工作
- **放弃裸 Windows 消息泵**，tray-icon 的 `TrackPopupMenu`→`WM_COMMAND`→`MenuEvent::send` 链不依赖 `PeekMessage`

---

## 问题 5: 配置兼容 + 热切换

### serde 忽略未知字段
```rust
#[derive(Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub active_llm: String,
    #[serde(default)]
    pub llms: HashMap<String, LLMConfig>,
}
// serde 默认忽略未知字段（如 common, client, routing），无需额外处理
```

### 动态加载 + 热切换
```rust
// 每次请求重新读磁盘
pub fn get_active_llm_config() -> Option<LLMConfig> {
    let settings = Settings::load();  // 重新从磁盘读
    settings.llms.get(&settings.active_llm).cloned()
}

// 写入立即生效
pub fn switch_active_llm(name: &str) -> Result<(), String> {
    let mut settings = SETTINGS.write().unwrap();
    settings.active_llm = name.to_string();
    settings.save();
}
```

### 配置路径查找优先级
1. EXE 同目录 `settings.json`
2. `assets/settings.json`
3. 当前工作目录 `settings.json`

---

## 问题 6: 上下文缓冲区 UTF-8 字符边界 panic

### 现象
```
panicked at src\stream.rs:288:45:
end byte index 1 is not a char boundary; it is inside '—' (bytes 0..3 of string)
```

### 修复
用 `floor_char_boundary` 确保切片在合法字符边界：
```rust
let safe_cut = self.text_buffer.len() - 35;
let send_len = self.text_buffer.floor_char_boundary(safe_cut);
let send_text = self.text_buffer[..send_len].to_string();
```

---

## 项目最终结构

```
src/
├── main.rs        # 入口：日志初始化 → spawn HTTP 线程 → 主线程跑 tray
├── types.rs       # 所有数据结构 (Anthropic/OpenAI/Settings)
├── config.rs      # settings.json 读写 + 热切换
├── converter.rs   # Anthropic → OpenAI 协议转换 + Tools Instruction 注入
├── context.rs     # token 估算 + 75% 裁剪 + 动态超时计算
├── stream.rs      # SSE 流处理状态机（核心）+ XML 工具调用拦截
├── recovery.rs    # 自动恢复判断与注入
├── server.rs      # axum HTTP 路由（3 个 endpoint）
└── tray.rs        # Windows 托盘（ApplicationHandler + about_to_wait）
```

## 总结

| 问题 | 关键方案 |
|------|---------|
| 隐藏控制台 | `cfg_attr(not(debug_assertions), windows_subsystem = "windows")` |
| EventLoop 线程 | winit 必须在主线程，tokio 在子线程 |
| 图标加载 | `from_path` 优先，`from_rgba` fallback |
| **菜单事件** | `ApplicationHandler::about_to_wait` + `try_recv` + `with_id`（唯一方案） |
| 模型切换 | 托盘菜单 click → `switch_active_llm` → `set_menu` 重建菜单 |
| 配置兼容 | serde 默认忽略未知字段，无需额外处理 |
| 字符边界 | `floor_char_boundary` 防止 UTF-8 panic |
