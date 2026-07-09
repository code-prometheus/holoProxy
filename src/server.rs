use axum::{
    body::Body,
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::config::{get_active_llm_name, get_llm_names, is_auto_select};
use crate::context::{calc_timeout_secs, parse_context_length};
use crate::converter::convert_to_openai_req;
use crate::stream::StreamContext;
use crate::types::*;

/// 活跃连接跟踪：msg_id → 创建时间
type ActiveConns = Arc<Mutex<HashMap<String, std::time::Instant>>>;

pub struct AppState {
    pub active_connections: ActiveConns,
}

pub fn create_router() -> Router {
    let state = Arc::new(AppState {
        active_connections: Arc::new(Mutex::new(HashMap::new())),
    });

    // 启动定时清理任务（每 30s 清理超过 300s 无活动的连接）
    let conns = state.active_connections.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let mut guard = conns.lock().await;
            let before = guard.len();
            guard.retain(|_id, ts| ts.elapsed() < std::time::Duration::from_secs(300));
            if guard.len() != before {
                info!("🧹 [Cleanup] 清理 {} 个超时连接（>300s），剩余 {} 个", before - guard.len(), guard.len());
            }
        }
    });

    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/models", get(handle_get_models))
        .route("/v1/select_model", post(handle_select_model))
        .with_state(state)
}

async fn handle_messages(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(body): Json<AnthropicRequest>,
) -> Response {
    let llm_config = match crate::config::get_active_llm_config() {
        Some(c) => c.clone(),
        None => {
            error!("无 LLM 配置");
            return (StatusCode::INTERNAL_SERVER_ERROR, "No LLM").into_response();
        }
    };

    let is_agent_mode = !body.tools.is_empty();
    let valid_tools: HashMap<String, ToolDef> =
        body.tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
    let valid_tools_arc = Arc::new(valid_tools);

    let mut openai_req = convert_to_openai_req(&body, &llm_config);
    let max_context = parse_context_length(&llm_config.context_max_length);
    if crate::context::should_trim(&openai_req.messages, max_context) {
        crate::context::trim_messages(&mut openai_req.messages, max_context);
    }

    let req_body_bytes = serde_json::to_vec(&openai_req).map_or(0, |v| v.len());
    info!(
        "📏 [Context] {:.1}KB | {}条 | ~{}tokens | max={}",
        req_body_bytes as f64 / 1024.0, openai_req.messages.len(),
        crate::context::estimate_token_count(&openai_req.messages), max_context
    );

    let base_url = llm_config.base_url.trim_end_matches('/');
    let api_url: Arc<str> = if base_url.ends_with("/chat/completions") {
        base_url.to_string().into()
    } else {
        format!("{}/chat/completions", base_url).into()
    };
    let msg_id = format!("msg_{}", chrono::Utc::now().timestamp());
    let timeout_secs = calc_timeout_secs(req_body_bytes);
    info!("⏱️ [Timeout] 请求体{:.1}KB → 动态超时{}s", req_body_bytes as f64/1024.0, timeout_secs);

    // 注册活跃连接
    {
        let mut guard = state.active_connections.lock().await;
        guard.insert(msg_id.clone(), std::time::Instant::now());
        info!("📊 [ActiveConns] 注册 {} (当前活跃: {})", msg_id, guard.len());
    }

    // ⚡ 强制 SSE 流式：统一走 mpsc channel，先返回 keepalive 防止 Claude Code 60s 超时
    let (tx, mut rx) = mpsc::channel::<Bytes>(256);
    let _ = tx.send(Bytes::from(": keepalive\n\n")).await;

    let tools_arc = valid_tools_arc.clone();
    let model = llm_config.model_name.clone();
    let mid = msg_id.clone();
    let api = api_url.clone();
    let oa_req = openai_req;
    let llm_cfg = llm_config;
    let conns = state.active_connections.clone();
    let is_auto = is_auto_select();

    tokio::spawn(async move {
        background_request_with_fallback(
            &api, &oa_req, &llm_cfg, &mid, &model, is_agent_mode, &tools_arc,
            timeout_secs, &tx, is_auto,
        ).await;
        // 完成后清理
        conns.lock().await.remove(&mid);
    });

    let body_stream = async_stream::stream! {
        while let Some(data) = rx.recv().await { yield Ok::<_, std::convert::Infallible>(data); }
    };

    Response::builder()
        .status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

const MAX_RETRIES: u32 = 10;

/// 带自动 fallback 的后台请求：非自动模式走单 LLM 重试，自动模式失败后依次切换 LLM
async fn background_request_with_fallback(
    api_url: &str,
    openai_req: &OpenAIRequest,
    llm_config: &crate::types::LLMConfig,
    msg_id: &str,
    model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    timeout_secs: u64,
    tx: &mpsc::Sender<Bytes>,
    is_auto: bool,
) {
    // 尝试当前 LLM
    let success = try_request(
        api_url, openai_req, llm_config, msg_id, model_name,
        is_agent_mode, valid_tools, timeout_secs, tx,
    ).await;

    if success {
        return;
    }

    // 非自动模式：直接返回（已在 try_request 中发过保活）
    if !is_auto {
        send_keepalive_response(msg_id, model_name, is_agent_mode, valid_tools, tx).await;
        return;
    }

    // 自动模式：依次 fallback
    let _current_llm = llm_config.model_name.clone();
    let mut fallback_count = 0;

    loop {
        let next = crate::config::auto_fallback_llm();
        match next {
            Some((fallback_name, config)) => {
                fallback_count += 1;
                let fb_url: Arc<str> = {
                    let base = config.base_url.trim_end_matches('/');
                    if base.ends_with("/chat/completions") {
                        base.to_string().into()
                    } else {
                        format!("{}/chat/completions", base).into()
                    }
                };
                info!(
                    "🔄 [AutoSelect Fallback #{}/{}] 尝试: '{}' ({})",
                    fallback_count, fallback_name, fallback_name, config.model_name
                );

                let success = try_request(
                    &fb_url, openai_req, &config, msg_id, &config.model_name,
                    is_agent_mode, valid_tools, timeout_secs, tx,
                ).await;

                if success {
                    return;
                }
            }
            None => {
                warn!(
                    "🚨 [AutoSelect] 所有 LLM (共{}个) 均已尝试失败",
                    fallback_count + 1
                );
                send_keepalive_response(msg_id, model_name, is_agent_mode, valid_tools, tx).await;
                return;
            }
        }
    }
}

/// 对单个 LLM 发起请求（含 10 次重试），返回是否成功
async fn try_request(
    api_url: &str,
    openai_req: &OpenAIRequest,
    llm_config: &crate::types::LLMConfig,
    msg_id: &str,
    model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    timeout_secs: u64,
    tx: &mpsc::Sender<Bytes>,
) -> bool {
    let client = Client::builder().danger_accept_invalid_certs(true).build().unwrap();

    for attempt in 1..=MAX_RETRIES {
        let mut req = client.post(api_url).json(openai_req)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        if !llm_config.api_key.is_empty() && llm_config.api_key.to_lowercase() != "none" {
            req = req.header(&llm_config.auth_header,
                format!("{}{}", llm_config.auth_prefix, llm_config.api_key));
        }

        let send_future = req.send();
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), send_future).await {
            Ok(Ok(response)) => {
                let has_output = forward_sse(
                    response, msg_id, model_name, is_agent_mode, valid_tools, tx,
                ).await;
                if has_output {
                    return true; // 成功且有效输出
                }
                // 200 但无内容 → 算失败，继续重试
                warn!("⚠️ [holoProxy] LLM 返回空内容，重试...");
                if attempt < MAX_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                return false;
            }
            Ok(Err(e)) => {
                let err_str = e.to_string();
                if should_retry_silent(&err_str) && attempt < MAX_RETRIES {
                    let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(6)));
                    info!("🔌 [holoProxy Retry {}/{}] {} → {}s 后静默重试...", attempt, MAX_RETRIES, err_str, delay.as_secs());
                    tokio::time::sleep(delay).await;
                    continue;
                }
                info!("🔌 [holoProxy Retry {}/{}] 非静默错误，放弃: {}", attempt, MAX_RETRIES, err_str);
                if attempt < MAX_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                warn!("🚨 [holoProxy] {} 次重试全部失败: {}", MAX_RETRIES, err_str);
                return false;
            }
            Err(_timeout) => {
                if attempt < MAX_RETRIES {
                    let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(6)));
                    info!("⏰ [holoProxy Timeout {}/{}] {}s → 静默重试...", attempt, MAX_RETRIES, timeout_secs);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                warn!("🚨 [holoProxy] {} 次超时重试全部失败 ({}s 超时)", MAX_RETRIES, timeout_secs);
                return false;
            }
        }
    }
    false
}

/// 发送一个最小保活的完整 SSE 响应（防止 Claude Code 空响应报错）
async fn send_keepalive_response(
    msg_id: &str,
    model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    tx: &mpsc::Sender<Bytes>,
) {
    let mut sse_ctx = StreamContext::new(
        msg_id.into(), model_name.into(), is_agent_mode, (**valid_tools).clone(),
    );
    sse_ctx.fallback_empty_finish();
    for batch in sse_ctx.take_output() {
        let _ = tx.send(batch).await;
    }
}

fn should_retry_silent(err_str: &str) -> bool {
    err_str.contains("disconnect") || err_str.contains("10054")
        || err_str.contains("connection reset") || err_str.contains("EOF")
        || err_str.contains("connection closed") || err_str.contains("timed out")
        || err_str.contains("timeout")
}

/// 转发下游 SSE 流，返回是否有有效输出（至少一个 SSE 事件）
async fn forward_sse(
    response: reqwest::Response,
    msg_id: &str, model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    tx: &mpsc::Sender<Bytes>,
) -> bool {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut sse_ctx = StreamContext::new(
        msg_id.into(), model_name.into(), is_agent_mode, (**valid_tools).clone(),
    );
    let mut finish_reason = String::from("stop");
    let mut has_any_data = false;

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                let text = String::from_utf8_lossy(&chunk);
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || !line.starts_with("data: ") { continue; }
                    let data_str = &line[6..];
                    if data_str == "[DONE]" { break; }
                    if let Ok(c) = serde_json::from_str::<OpenAISseChunk>(data_str) {
                        has_any_data = true;
                        for choice in &c.choices {
                            if let Some(ref d) = choice.delta {
                                if let Some(ref r) = d.reasoning_content { if !r.is_empty() { sse_ctx.handle_reasoning(r); } }
                                if let Some(ref ct) = d.content { if !ct.is_empty() { sse_ctx.handle_content(ct); } }
                                if let Some(ref tcs) = d.tool_calls { for tc in tcs { sse_ctx.handle_tool_call(tc); } }
                            }
                            if let Some(ref fr) = choice.finish_reason { finish_reason = fr.clone(); }
                        }
                    }
                }
            }
            Err(e) => { warn!("SSE read err: {}", e); break; }
        }
    }

    if has_any_data {
        sse_ctx.finish(&finish_reason);
        for batch in sse_ctx.take_output() { let _ = tx.send(batch).await; }
        true
    } else {
        false
    }
}

async fn handle_get_models() -> impl IntoResponse {
    Json(serde_json::json!({
        "active_llm": get_active_llm_name(),
        "auto_select": is_auto_select(),
        "models": get_llm_names()
    }))
}

async fn handle_select_model(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let name = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    // 处理自动选择开关
    if name == "__auto__" || name == "auto" {
        let enable = body.get("enable").and_then(|v| v.as_bool()).unwrap_or(true);
        crate::config::switch_auto_select(enable);
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "success",
                "auto_select": enable,
                "active_llm": get_active_llm_name()
            })),
        );
    }

    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"status":"error","msg":"model required"})));
    }
    match crate::config::switch_active_llm(name) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status":"success","active_llm":name}))),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"status":"error","msg":e}))),
    }
}
