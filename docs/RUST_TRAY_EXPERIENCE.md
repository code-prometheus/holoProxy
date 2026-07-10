# Rust Windows 托盘 + 菜单应用：从零到完美交付的完整经验总结

> **目标读者**：用 Rust 构建 Windows 系统托盘应用的开发者。本文覆盖了托盘图标、菜单交互、事件循环、配置热加载、日志、构建脚本的全链路经验。

---

## 1. 最小可行依赖组合

以下是一套经过实战验证的依赖组合（`Cargo.toml`）：

```toml
tray-icon = "0.24"        # 系统托盘 API，底层调用 Win32 Shell_NotifyIcon
winit = "0.30"            # 窗口事件循环（托盘不需要窗口，但需要 EventLoop）
crossbeam-channel = "0.5" # 菜单事件接收（tray-icon 通过 crossbeam 通道发送 MenuEvent）
once_cell = "1"           # 惰性全局变量（如共享 HTTP Client、配置路径）
```

**不需要的依赖：**
- 不需要 `windows` / `windows-sys` crate（tray-icon 内部封装好了）
- 不需要 `tao`（tao 是 winit 的 fork，winit 0.30 原生支持，避免冲突）
- 不需要 `native-windows-gui` / `egui` / `iced`（托盘应用不需要 GUI 框架）

---

## 2. 核心架构：主线程 vs 工作线程

### 2.1 铁律：winit EventLoop 必须在主线程

```
winit 0.30 的 EventLoop::new() 要求在主线程创建。
如果把 tokio + axum 放到主线程，EventLoop 就会 panic。
如果把 winit 放到子线程，EventLoop::new() 直接 panic。
```

**唯一正确的架构：**

```rust
fn main() {
    // ── 日志初始化（在任何 spawn 之前）──
    tracing_subscriber::fmt()
        .with_ansi(false)        // Windows 控制台不支持 ANSI
        .with_target(false)      // 精简日志，不显示模块路径
        .with_writer(file_appender)
        .init();

    // ── 后台线程跑 tokio runtime + axum ──
    let http_thread = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        rt.block_on(async {
            let app = create_router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:5430").await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });

    // ── 主线程跑 winit EventLoop + tray ──
    tray::run_tray(); // 内部调用 event_loop.run_app(&mut app)
    let _ = http_thread.join();
}
```

### 2.2 tokio runtime 选择

| Runtime | 场景 | 推荐 |
|---------|------|------|
| `new_current_thread()` | HTTP 代理（请求量不大，无需多线程） | ✅ |
| `new_multi_thread()` | 高并发场景 | ⚠️ 需要时再用 |

对于代理类应用，`current_thread` 足够且资源开销最小。

---

## 3. Windows 托盘：tray-icon + winit 完整方案

### 3.1 隐藏控制台

```rust
// main.rs 第一行
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```

效果：`cargo run` 有控制台，`cargo build --release` 出的 exe 双击运行无黑窗。

### 3.2 图标加载（两级 fallback）

```rust
fn load_icon() -> tray_icon::Icon {
    // 第 1 级：从文件加载 .ico
    if let Some(path) = find_icon_path() {
        if let Ok(icon) = tray_icon::Icon::from_path(&path, None) {
            return icon;
        }
    }

    // 第 2 级：程序化生成 RGBA 像素（永不失败）
    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    // ... 画圆 + 字母的位图 ...
    tray_icon::Icon::from_rgba(rgba, w, h).expect("RGBA icon always works")
}
```

`find_icon_path()` 的查找顺序：
1. EXE 同目录 `assets/icon.ico`
2. EXE 同目录 `icon.ico`
3. 当前工作目录 `assets/icon.ico`
4. 当前工作目录 `icon.ico`

**关键**：`build.rs` 在编译时自动将项目 `assets/` 目录复制到 `target/{profile}/assets/`，这样 `cargo run` 也能找到图标。

### 3.3 菜单构建与 ID 绑定

菜单项**必须用 `MenuItem::with_id(MenuId::new("你的ID"), ...)` 构造**，不能依赖默认 ID：

```rust
fn build_menu() -> tray_icon::menu::Menu {
    let menu = Menu::new();

    // 动态菜单项：每个模型一行，带 ID
    for model in &models {
        let label = if model == &active {
            format!("✓ {}", model)   // 当前激活的标记勾号
        } else {
            format!("  {}", model)
        };
        menu.append(&MenuItem::with_id(
            MenuId::new(model.clone()),  // ID = 模型名
            label, true, None
        )).ok();
    }

    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&MenuItem::with_id(
        MenuId::new("__quit__".to_string()),
        "退出", true, None
    )).ok();
    menu
}
```

### 3.4 TrayIconBuilder 构建

```rust
let tray = TrayIconBuilder::new()
    .with_menu(Box::new(build_menu()))
    .with_icon(load_icon())
    .with_tooltip(format!("MyApp - {}", state_description))
    .build()?;
```

---

## 4. 菜单事件：血泪教训后的唯一可行方案

这是整个托盘开发中最坑的部分。以下是经过大量试错后确认的**唯一可行方案**：

### 4.1 三步法（缺一不可）

#### 步骤 1：在 `TrayIconBuilder::build()` 之前获取 `MenuEvent::receiver()`

```rust
let menu_event_rx = MenuEvent::receiver().clone();  // ⚡ 必须在 build() 之前！
```

#### 步骤 2：实现 `ApplicationHandler` trait，在 `about_to_wait` 中轮询

```rust
struct TrayApp {
    menu_event_rx: crossbeam_channel::Receiver<tray_icon::menu::MenuEvent>,
    tray: Option<tray_icon::TrayIcon>,
}

impl winit::application::ApplicationHandler for TrayApp {
    // 这两个是 trait 要求的，即使为空也要实现
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        while let Ok(event) = self.menu_event_rx.try_recv() {
            if let Some(ref mut tray) = self.tray {
                handle_menu_click(&event.id.0, tray);
            }
        }
    }
}
```

#### 步骤 3：用 `run_app` 启动事件循环

```rust
let event_loop = EventLoop::new()?;
let mut app = TrayApp { menu_event_rx, tray: Some(tray) };
event_loop.run_app(&mut app)?; // ⚡ 是 run_app，不是 run() 或 run_on_demand()
```

### 4.2 为什么其他方案不行？

| 失败方案 | 现象 | 根因 |
|---------|------|------|
| `EventLoop::run()` 闭包 + `try_recv` | 收不到事件 | `ControlFlow::Wait` 阻塞消息泵，`Poll` 时 winit 不 dispatch tray 消息 |
| `MenuEvent::set_event_handler()` | handler 不触发 | tray-icon 的 `send()` 依赖特定时机，handler 注册方式与此冲突 |
| 裸 `PeekMessageW` / `DispatchMessageW` | 收到 0 条消息 | tray-icon 的 `WM_COMMAND` 通过 `SendMessageW` 同步处理，不经过消息队列 |
| `run_on_demand()` + 手动 `poll` | 收不到事件 | 同上 |
| `std::thread::spawn` + `recv()` 阻塞 | 永不触发 | `MenuEvent::send` 依赖 winit 事件循环的运行 |

### 4.3 核心原理

`about_to_wait` 是 winit 在每个空闲周期都会调用的钩子。在这里调用 `try_recv()` 既不会阻塞 winit 的消息泵，又能保证在 winit 处理完内部消息后及时收到 tray-icon 通过 crossbeam 通道发送的 `MenuEvent`。

---

## 5. 菜单切换后的状态更新

用户点击菜单项后需要重建菜单（更新勾号）：

```rust
fn handle_menu_click(id_str: &str, tray: &mut tray_icon::TrayIcon) {
    if id_str == "__quit__" {
        std::process::exit(0);  // 直接退出是最干净的
    }

    // 执行业务逻辑（如切换配置）
    switch_config(id_str);

    // ⚡ 重建菜单：勾号位置自动更新
    let _ = tray.set_menu(Some(Box::new(build_menu())));
    // ⚡ 更新 tooltip
    let _ = tray.set_tooltip(Some(format!("MyApp - {}", get_current_state())));
}
```

**关键注意**：
- `tray.set_menu()` 传入的是 `Option<Box<dyn Menu>>`，不是 `&Menu`
- 每次切换后调用 `build_menu()` 生成全新菜单，开销极小（菜单最多几十项）

---

## 6. 日志系统：文件日志 + 精简格式

### 6.1 每次启动清空旧日志

```rust
let log_path = exe_dir.join("holoProxy.log");
let _ = std::fs::write(&log_path, "");  // 清空
let file_appender = tracing_appender::rolling::never(
    log_path.parent().unwrap(),
    log_path.file_name().unwrap(),
);
```

### 6.2 自定义时间格式（无颜色）

```rust
struct SimpleTimer;
impl tracing_subscriber::fmt::time::FormatTime for SimpleTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%H:%M:%S"))
    }
}

tracing_subscriber::fmt()
    .with_timer(SimpleTimer)
    .with_ansi(false)    // Windows 不支持 ANSI 转义
    .with_target(false)  // 不显示模块路径，保持日志简洁
    .with_writer(file_appender)
    .init();
```

---

## 7. 构建脚本 `build.rs`：自动复制资源文件

```rust
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir_path = std::path::Path::new(&out_dir);
    // OUT_DIR = target/{profile}/build/holo_proxy-xxx/out
    // ancestors().nth(3) → target/{profile}/
    let target_dir = out_dir_path.ancestors().nth(3).unwrap();

    // 复制 assets/ 目录
    let src_assets = std::path::Path::new("assets");
    let dst_assets = target_dir.join("assets");
    if src_assets.exists() {
        let _ = std::fs::remove_dir_all(&dst_assets);
        copy_dir(src_assets, &dst_assets).ok();
        println!("cargo:warning=Assets → {}", dst_assets.display());
    }

    // 复制 settings.json
    let src_settings = std::path::Path::new("settings.json");
    let dst_settings = target_dir.join("settings.json");
    if src_settings.exists() {
        std::fs::copy(src_settings, &dst_settings).ok();
        println!("cargo:warning=settings.json → {}", dst_settings.display());
    }
}
```

这样 `cargo run` 和 `cargo build --release` 都能在 EXE 同目录找到资源文件。

---

## 8. 配置文件路径查找（三级 fallback）

```rust
static SETTINGS_PATH: Lazy<PathBuf> = Lazy::new(|| {
    // 1. EXE 同目录 settings.json（最优先）
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.parent().unwrap_or(Path::new(".")).join("settings.json");
        if p.exists() { return p; }
    }
    // 2. assets/settings.json
    let p = PathBuf::from("assets/settings.json");
    if p.exists() { return p; }
    // 3. 当前工作目录 settings.json
    PathBuf::from("settings.json")
});
```

---

## 9. 配置热切换：`RwLock` + 磁盘读写

```rust
// 全局配置缓存
static SETTINGS: Lazy<RwLock<Settings>> =
    Lazy::new(|| RwLock::new(Settings::load()));

// 读取（每次请求都会重新从磁盘加载）
pub fn get_active_llm_config() -> Option<LLMConfig> {
    let s = Settings::load(); // 重新读磁盘，不是读缓存
    s.llms.get(&s.active_llm).cloned()
}

// 写入（立即持久化）
pub fn switch_active_llm(name: &str) -> Result<(), String> {
    let mut s = SETTINGS.write().unwrap();
    s.active_llm = name.to_string();
    s.save(); // 立即写磁盘
    Ok(())
}
```

托盘菜单点击切换 → `switch_active_llm()` → 写磁盘 → 下一个请求自动加载新配置。无需重启。

---

## 10. 数据文件结构（IndexMap 保持顺序）

```json
{
    "active_llm": "第一个模型",
    "llms": {
        "第一个模型": {
            "base_url": "http://xxx:8000/v1",
            "model_name": "model-name",
            "context_max_length": "1m",
            "api_key": "none"
        }
    }
}
```

使用 `IndexMap<String, LLMConfig>` 代替 `HashMap`，保持配置文件中 LLM 的插入顺序——托盘菜单的显示顺序与配置文件中完全一致。

---

## 11. 完整项目文件结构

```
project/
├── assets/
│   └── icon.ico                # 托盘图标（.ico 格式最可靠）
├── src/
│   ├── main.rs                 # 入口：日志 → HTTP 子线程 → tray 主线程
│   ├── types.rs                # 所有数据结构定义
│   ├── config.rs               # 配置读写 + 热切换
│   ├── server.rs               # axum HTTP 路由
│   ├── tray.rs                 # 托盘 + 菜单（ApplicationHandler）
│   └── ...                     # 业务模块
├── build.rs                    # 编译时复制 assets + settings.json
├── Cargo.toml
├── settings.json               # 配置文件
├── .cargo/
│   └── config.toml             # 可选：linker 配置等
└── .github/
    └── workflows/
        └── ci.yml              # CI: build → test → package → release
```

---

## 12. 完整可运行的托盘模板

以下是一个去掉业务逻辑后的**纯托盘应用模板**，可以直接复制作为新项目起点：

### Cargo.toml (托盘核心依赖)

```toml
[package]
name = "my_tray_app"
version = "0.1.0"
edition = "2021"

[dependencies]
tray-icon = "0.24"
winit = "0.30"
crossbeam-channel = "0.5"
tracing = "0.1"
tracing-subscriber = "0.3"
tracing-appender = "0.2"
chrono = "0.4"
once_cell = "1"
```

### main.rs

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod tray;

fn main() {
    // 日志初始化
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .init();

    // 如果有后台服务，spawn 到子线程
    // std::thread::spawn(|| { ... });

    // 主线程跑托盘
    tray::run_tray();
}
```

### tray.rs

```rust
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::application::ApplicationHandler;
use tracing::{info, warn};

fn build_menu() -> Menu {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(MenuId::new("item1".into()), "选项 1", true, None)).ok();
    menu.append(&MenuItem::with_id(MenuId::new("item2".into()), "选项 2", true, None)).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&MenuItem::with_id(MenuId::new("__quit__".into()), "退出", true, None)).ok();
    menu
}

fn handle_menu_click(id_str: &str, tray: &mut TrayIcon) {
    match id_str {
        "__quit__" => {
            info!("user quit");
            std::process::exit(0);
        }
        "item1" => { /* 业务逻辑 */ }
        "item2" => { /* 业务逻辑 */ }
        _ => {}
    }
}

struct TrayApp {
    menu_event_rx: crossbeam_channel::Receiver<MenuEvent>,
    tray: Option<TrayIcon>,
}

impl ApplicationHandler for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: winit::window::WindowId, _event: winit::event::WindowEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        while let Ok(event) = self.menu_event_rx.try_recv() {
            if let Some(ref mut tray) = self.tray {
                handle_menu_click(&event.id.0, tray);
            }
        }
    }
}

pub fn run_tray() {
    // ⚡ 步骤 1：在 build 之前获取 receiver
    let menu_event_rx = MenuEvent::receiver().clone();

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            warn!("EventLoop::new failed: {:?}", e);
            return;
        }
    };

    // 加载图标（优先 .ico 文件，fallback 到 RGBA）
    let icon = load_icon();

    // ⚡ 步骤 2：构建 tray
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(build_menu()))
        .with_icon(icon)
        .with_tooltip("My Tray App")
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            warn!("TrayIconBuilder failed: {:?}", e);
            return;
        }
    };

    info!("tray ready");

    // ⚡ 步骤 3：run_app
    let mut app = TrayApp { menu_event_rx, tray: Some(tray) };
    let _ = event_loop.run_app(&mut app);
}

fn load_icon() -> Icon {
    // Try .ico files first
    for candidate in &["assets/icon.ico", "icon.ico"] {
        if let Ok(icon) = Icon::from_path(std::path::Path::new(candidate), None) {
            return icon;
        }
    }
    // Fallback: programmatic RGBA
    let mut rgba = vec![0u8; 32 * 32 * 4];
    // ... draw something ...
    Icon::from_rgba(rgba, 32, 32).expect("RGBA icon")
}
```

---

## 13. 踩坑清单（快速排查）

| 症状 | 可能原因 | 修复 |
|------|---------|------|
| `Initializing the event loop outside of the main thread` | winit 在子线程 | 主线程跑 winit，后台线程跑 tokio |
| 托盘菜单点击无反应 | 没用 `ApplicationHandler::about_to_wait` | 参考 4.1 |
| 托盘菜单点击无反应 | `receiver()` 在 `build()` 之后获取 | receiver 必须在 build 前获取 |
| 菜单项 ID 是数字不是字符串 | 用了 `MenuItem::new()` 而非 `with_id()` | 改用 `MenuItem::with_id(MenuId::new("str"), ...)` |
| 双击 exe 弹出黑窗 | debug build 或没加 `windows_subsystem` | 加 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` |
| `Icon::from_path` 失败 | .ico 文件不存在或格式不支持 | 用 .ico 格式 + build.rs 复制 |
| 切换菜单项后勾号不变 | 没重建菜单 | 调用 `tray.set_menu(Some(Box::new(build_menu())))` |
| `cargo run` 找不到图标 | 没把 assets 复制到 target 目录 | 用 build.rs 自动复制 |
| `Menu` / `MenuId` 类型不匹配 | tray-icon 版本问题 | 用 `0.24` 版本 |
| Windows 11 托盘图标被折叠 | 系统行为 | 用户拖到任务栏即可 |
