#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod context;
mod converter;
mod recovery;
mod server;
mod stream;
mod tray;
mod types;

use tracing::info;
use tracing_subscriber::{fmt::time::LocalTime, EnvFilter};

fn main() {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(LocalTime::rfc_3339())
        .with_target(false)
        .init();

    info!("🚀 holoProxy v{} 启动中...", env!("CARGO_PKG_VERSION"));
    info!("📋 监听端口: 127.0.0.1:5430");

    // 验证配置
    match config::get_active_llm_config() {
        Some(c) => info!(
            "✅ 当前激活 LLM: {} ({})",
            config::get_active_llm_name(),
            c.model_name
        ),
        None => info!("⚠️ 无激活的 LLM 配置，请在 settings.json 中配置"),
    }

    // 在后台线程启动 tokio + axum HTTP 服务器
    let http_thread = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            // 定时清理器
            tokio::spawn(async {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    tracing::debug!("🕐 [Cleanup] 定时清理 tick");
                }
            });

            let app = server::create_router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:5430")
                .await
                .expect("❌ 无法绑定端口 5430");

            info!("🌐 HTTP 服务已启动: http://127.0.0.1:5430");
            info!("   POST /v1/messages    → Claude Code API 代理");
            info!("   GET  /v1/models      → 查看可用模型");
            info!("   POST /v1/select_model → 切换激活模型");

            axum::serve(listener, app).await.unwrap();
        });
    });

    // 主线程：运行系统托盘（winit event loop 必须在主线程）
    tray::run_tray();

    // 托盘退出后，等待 HTTP 线程结束
    let _ = http_thread.join();
}
