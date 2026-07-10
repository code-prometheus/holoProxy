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

type ActiveConns = Arc<Mutex<HashMap<String, std::time::Instant>>>;

pub struct AppState {
    pub active_connections: ActiveConns,
}

pub fn create_router() -> Router {
    let state = Arc::new(AppState {
        active_connections: Arc::new(Mutex::new(HashMap::new())),
    });

    let conns = state.active_connections.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let mut guard = conns.lock().await;
            let before = guard.len();
            guard.retain(|_id, ts| ts.elapsed() < std::time::Duration::from_secs(300));
            if before != guard.len() {
                info!("[cleanup] {} stale conns removed (active: {})", before - guard.len(), guard.len());
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
        Some(c) => c,
        None => {
            error!("No LLM config");
            return (StatusCode::INTERNAL_SERVER_ERROR, "No LLM").into_response();
        }
    };

    let is_agent_mode = !body.tools.is_empty();
    let valid_tools: HashMap<String, ToolDef> =
        body.tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
    let valid_tools_arc = Arc::new(valid_tools);

    let mut openai_req = convert_to_openai_req(&body, &llm_config);

    // 纯文本清洗（去控制字符 + 压缩空白），后跟 trim 检查
    crate::context::clean_messages(&mut openai_req.messages);

    let max_context = parse_context_length(&llm_config.context_max_length);
    if crate::context::should_trim(&openai_req.messages, max_context) {
        crate::context::trim_messages(&mut openai_req.messages, max_context);
    }

    let req_bytes = serde_json::to_vec(&openai_req).map_or(0, |v| v.len());
    let msg_id = format!("msg_{}", chrono::Utc::now().timestamp());
    let timeout_secs = calc_timeout_secs(req_bytes);

    // 单行摘要：模型 | 消息条数 | tokens | 上下文大小 | 超时
    info!(
        "[req {}] {}agent={} msgs={} tok={} max_ctx={} timeout={}s | {}",
        msg_id, if is_auto_select() { "[AUTO]" } else { "" },
        is_agent_mode, openai_req.messages.len(),
        crate::context::estimate_token_count(&openai_req.messages),
        max_context, timeout_secs, llm_config.model_name
    );

    let base_url = llm_config.base_url.trim_end_matches('/');
    let api_url: Arc<str> = if base_url.ends_with("/chat/completions") {
        base_url.to_string().into()
    } else {
        format!("{}/chat/completions", base_url).into()
    };

    {
        let mut guard = state.active_connections.lock().await;
        guard.insert(msg_id.clone(), std::time::Instant::now());
    }

    let (tx, mut rx) = mpsc::channel::<Bytes>(256);
    let _ = tx.send(Bytes::from(": keepalive\n\n")).await;

    // 定期保活：每 15 秒发 keepalive，防止 Claude Code 超时断开
    let keepalive_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            if keepalive_tx.send(Bytes::from(": keepalive\n\n")).await.is_err() {
                break; // rx 端已关闭，停止保活
            }
        }
    });

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
    let (success, retries, last_err) = try_request(
        api_url, openai_req, llm_config, msg_id, model_name,
        is_agent_mode, valid_tools, timeout_secs, tx,
    ).await;

    if success {
        return;
    }

    if !is_auto {
        info!("[{}] fail after {} retries: {}", msg_id, retries, last_err);
        send_keepalive_response(msg_id, model_name, is_agent_mode, valid_tools, tx).await;
        return;
    }

    // 自动 fallback
    let mut fb_count = 0;
    loop {
        match crate::config::auto_fallback_llm() {
            Some((name, config)) => {
                fb_count += 1;
                let fb_url: Arc<str> = {
                    let base = config.base_url.trim_end_matches('/');
                    if base.ends_with("/chat/completions") {
                        base.to_string().into()
                    } else {
                        format!("{}/chat/completions", base).into()
                    }
                };
                info!("[{}] auto-fallback #{}/{} → {} ({})", msg_id, fb_count, name, name, config.model_name);

                let (ok, _, _) = try_request(
                    &fb_url, openai_req, &config, msg_id, &config.model_name,
                    is_agent_mode, valid_tools, timeout_secs, tx,
                ).await;

                if ok {
                    return;
                }
            }
            None => {
                warn!("[{}] all LLMs exhausted after {} fallbacks", msg_id, fb_count);
                send_keepalive_response(msg_id, model_name, is_agent_mode, valid_tools, tx).await;
                return;
            }
        }
    }
}

/// 对单个 LLM 请求，含静默重试。返回 (成功, 重试次数, 最后错误)
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
) -> (bool, u32, String) {
    let mut last_err = String::new();

    for attempt in 1..=MAX_RETRIES {
        // ⚡ 每次重试创建新的 Client，确保旧 TCP 连接被释放
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .pool_max_idle_per_host(0)  // 禁用连接池，每次新建 TCP
            .build()
            .unwrap();

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
                    return (true, 0, String::new());
                }
                last_err = "empty response".into();
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
            }
            Err(_) => {
                last_err = format!("timeout {}s", timeout_secs);
            }
        }
        // drop(client) — Client 销毁时关闭所有连接池中的 TCP 连接

        if attempt < MAX_RETRIES {
            if attempt == 1 {
                info!("[{}] retry {}/{}: {}", msg_id, attempt, MAX_RETRIES, last_err);
            }
            let delay = std::time::Duration::from_secs(attempt.min(6) as u64);
            tokio::time::sleep(delay).await;
            continue;
        }
    }
    warn!("[{}] {} retries exhausted: {}", msg_id, MAX_RETRIES, last_err);
    (false, MAX_RETRIES, last_err)
}

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
            Err(_) => { break; }
        }
    }

    sse_ctx.finish(&finish_reason);
    for batch in sse_ctx.take_output() { let _ = tx.send(batch).await; }
    has_any_data
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
