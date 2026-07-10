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

fn log_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        exe.parent().unwrap_or(std::path::Path::new(".")).join("holoProxy.log")
    } else {
        std::path::PathBuf::from("holoProxy.log")
    }
}

fn main() {
    // 每次启动清空日志
    let lp = log_path();
    let _ = std::fs::write(&lp, "");

    let file_appender = tracing_appender::rolling::never(
        lp.parent().unwrap_or(std::path::Path::new(".")),
        lp.file_name().unwrap_or(std::ffi::OsStr::new("holoProxy.log")),
    );

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_timer(LocalTime::rfc_3339())
        .with_target(false)
        .with_writer(file_appender)
        .init();

    info!("🚀 holoProxy v{} starting...", env!("CARGO_PKG_VERSION"));
    info!("📋 port: 127.0.0.1:5430");

    match config::get_active_llm_config() {
        Some(c) => info!("✅ active LLM: {} ({}) auto_select:{}", config::get_active_llm_name(), c.model_name, config::is_auto_select()),
        None => info!("⚠️  no LLM configured"),
    }

    // 后台线程跑 tokio + axum
    let http_thread = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let app = server::create_router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:5430")
                .await
                .expect("port 5430");
            info!("HTTP server ready: http://127.0.0.1:5430");
            axum::serve(listener, app).await.unwrap();
        });
    });

    // 主线程跑 tray
    tray::run_tray();
    let _ = http_thread.join();
}
