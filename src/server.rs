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

use crate::config::{get_active_llm_name, get_llm_names};
use crate::context::{calc_timeout_secs, parse_context_length};
use crate::converter::convert_to_openai_req;
use crate::stream::{self, StreamContext};
use crate::types::*;

/// 每次重试都新建——确保不用已断开的TCP连接
fn fresh_client() -> Client {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .tcp_nodelay(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("fresh HTTP client")
}

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
        .route("/v1/chat/completions", post(handle_chat_completions))
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
    crate::context::clean_messages(&mut openai_req.messages);

    let max_context = parse_context_length(&llm_config.context_max_length);
    if crate::context::should_trim(&openai_req.messages, max_context) {
        crate::context::trim_messages(&mut openai_req.messages, max_context);
    }

    // 注入 chat_template_kwargs（仅 thinking=true 时注入，保持标准兼容）
    let mut req_body = serde_json::to_value(&openai_req).unwrap_or_default();
    if let Some(obj) = req_body.as_object_mut() {
        if llm_config.thinking {
            obj.insert("chat_template_kwargs".into(), serde_json::json!({"thinking": true, "reasoning_effort": llm_config.reasoning_effort.as_str()}));
        }
        obj.insert("stream".into(), serde_json::json!(llm_config.stream));
    }

    let req_bytes = openai_req.messages.len() * 512;
    let msg_id = format!("msg_{}", chrono::Utc::now().timestamp());
    let timeout_secs = calc_timeout_secs(req_bytes);

    info!(
        "[req {}] agent={} msgs={} tok={} max_ctx={} timeout={}s | {}",
        msg_id,
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

    // 首 token 到达前每 15s 发 keepalive，首 token 后立即停止
    let keepalive_tx = tx.clone();
    let (stop_ka_tx, mut stop_ka_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = &mut stop_ka_rx => break,
                _ = interval.tick() => {
                    if keepalive_tx.send(Bytes::from(": keepalive\n\n")).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let tools_arc = valid_tools_arc.clone();
    let model = llm_config.model_name.clone();
    let mid = msg_id.clone();
    let api = api_url.clone();
    let llm_cfg = llm_config;
    let conns = state.active_connections.clone();

    tokio::spawn(async move {
        background_request(
            &api, &req_body, &llm_cfg, &mid, &model, is_agent_mode, &tools_arc,
            timeout_secs, &tx, Some(stop_ka_tx),
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

const MAX_RETRIES: u32 = 3;

async fn background_request(
    api_url: &str, openai_req: &serde_json::Value,
    llm_config: &crate::types::LLMConfig,
    msg_id: &str, model_name: &str,
    is_agent_mode: bool, valid_tools: &Arc<HashMap<String, ToolDef>>,
    timeout_secs: u64, tx: &mpsc::Sender<Bytes>,
    mut stop_keepalive: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let mut last_err = String::new();
    let req_start = std::time::Instant::now();

    for attempt in 1..=MAX_RETRIES {
        let remaining = MAX_RETRIES - attempt;
        info!(
            "[{}] attempt={}/{} remaining={} url={}",
            msg_id, attempt, MAX_RETRIES, remaining, api_url
        );

        let client = fresh_client();

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
                let status = response.status().as_u16();
                info!("[{}] attempt={} http={} in={:.1}s", msg_id, attempt, status, req_start.elapsed().as_secs_f64());
                let ka = if attempt == 1 { stop_keepalive.take() } else { None };
                let has_output = forward_sse(
                    response, msg_id, model_name, is_agent_mode, valid_tools, tx, ka,
                ).await;
                if has_output {
                    return;
                }
                last_err = "empty response".into();
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                info!("[{}] attempt={} conn_err={}", msg_id, attempt, last_err);
            }
            Err(_) => {
                last_err = format!("timeout {}s", timeout_secs);
                info!("[{}] attempt={} timeout={}s", msg_id, attempt, timeout_secs);
            }
        }
        // drop(client) — 释放本次 attempt 的 TCP 连接

        if attempt < MAX_RETRIES {
            info!("[{}] retry {}/{} remaining={} err={}", msg_id, attempt, MAX_RETRIES, remaining - 1, last_err);
        }
    }

    warn!("[{}] ALL {} retries exhausted after {:.1}s: {}", msg_id, MAX_RETRIES, req_start.elapsed().as_secs_f64(), last_err);
    send_error_response(msg_id, model_name, is_agent_mode, valid_tools, tx, &last_err).await;
}

async fn send_error_response(
    msg_id: &str, model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    tx: &mpsc::Sender<Bytes>,
    last_err: &str,
) {
    let mut sse_ctx = StreamContext::new(
        msg_id.into(), model_name.into(), is_agent_mode, (**valid_tools).clone(),
    );
    sse_ctx.send_error(&format!(
        "[holoProxy Error] 下游 LLM 连接失败 (已重试{}次): {}",
        MAX_RETRIES, last_err
    ));
    for batch in sse_ctx.take_output() { let _ = tx.send(batch).await; }
}

async fn forward_sse(
    response: reqwest::Response,
    msg_id: &str, model_name: &str,
    is_agent_mode: bool,
    valid_tools: &Arc<HashMap<String, ToolDef>>,
    tx: &mpsc::Sender<Bytes>,
    mut stop_keepalive: Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    use futures_util::StreamExt;
    let req_start = std::time::Instant::now();
    let mut stream = response.bytes_stream();
    let mut sse_ctx = StreamContext::new(
        msg_id.into(), model_name.into(), is_agent_mode, (**valid_tools).clone(),
    );
    let mut finish_reason = String::from("stop");
    let mut has_any_data = false;
    let mut first_token = true;
    let mut line_buf_sse = String::new(); // 跨 chunk 行缓冲

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                if first_token {
                    first_token = false;
                    if let Some(s) = stop_keepalive.take() { let _ = s.send(()); }
                    info!("[rsp {}] {} first_token in {:.1}s chunk={}B", msg_id, model_name, req_start.elapsed().as_secs_f64(), chunk.len());
                }
                // 行缓冲：处理完整行，保留不完整尾部
                line_buf_sse.push_str(&String::from_utf8_lossy(&chunk));
                let complete_end = line_buf_sse.rfind('\n').map(|p| p + 1).unwrap_or(0);
                let complete_text = line_buf_sse[..complete_end].to_string();
                let tail = line_buf_sse[complete_end..].to_string();
                line_buf_sse.clear();
                line_buf_sse.push_str(&tail);

                for line in complete_text.lines() {
                    let line = line.trim();
                    if line.is_empty() || !line.starts_with("data: ") { continue; }
                    let data_str = &line[6..];
                    if data_str == "[DONE]" { break; }
                    if let Ok(c) = serde_json::from_str::<OpenAISseChunk>(data_str) {
                        has_any_data = true;
                        for choice in &c.choices {
                            if let Some(ref d) = choice.delta {
                                if let Some(ref r) = d.reasoning { if !r.is_empty() { sse_ctx.handle_reasoning(r); } }
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

/// 直接透传 OpenAI 格式请求到下游 — 不做协议转换
async fn handle_chat_completions(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let llm_config = match crate::config::get_active_llm_config() {
        Some(c) => c,
        None => {
            error!("No LLM config");
            return (StatusCode::INTERNAL_SERVER_ERROR, "No LLM").into_response();
        }
    };

    let base_url = llm_config.base_url.trim_end_matches('/');
    let api_url: Arc<str> = if base_url.ends_with("/chat/completions") {
        base_url.to_string().into()
    } else {
        format!("{}/chat/completions", base_url).into()
    };

    let msg_id = format!("msg_{}", chrono::Utc::now().timestamp());
    info!("[req {}] OpenAI passthrough | {}", msg_id, llm_config.model_name);

    {
        let mut guard = state.active_connections.lock().await;
        guard.insert(msg_id.clone(), std::time::Instant::now());
    }

    let (tx, mut rx) = mpsc::channel::<Bytes>(256);

    // keepalive
    let keepalive_tx = tx.clone();
    let (stop_ka_tx, mut stop_ka_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = &mut stop_ka_rx => break,
                _ = interval.tick() => {
                    if keepalive_tx.send(Bytes::from(": keepalive\n\n")).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mid = msg_id.clone();
    let api = api_url.clone();
    let llm_cfg = llm_config;
    let conns = state.active_connections.clone();
    let timeout_secs: u64 = 120;

    tokio::spawn(async move {
        background_request_raw(&api, &body, &llm_cfg, &mid, timeout_secs, &tx, Some(stop_ka_tx)).await;
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

/// 直接透传 SSE — 不解析不转换，原样转发
async fn background_request_raw(
    api_url: &str,
    body: &serde_json::Value,
    llm_config: &crate::types::LLMConfig,
    msg_id: &str,
    timeout_secs: u64,
    tx: &mpsc::Sender<Bytes>,
    mut stop_keepalive: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let mut last_err = String::new();
    let req_start = std::time::Instant::now();

    for attempt in 1..=MAX_RETRIES {
        let remaining = MAX_RETRIES - attempt;
        info!("[{}] attempt={}/{} remaining={} url={}", msg_id, attempt, MAX_RETRIES, remaining, api_url);

        let client = fresh_client();
        // 注入 model + chat_template_kwargs 确保下游识别模型和 reasoning 启用
        let mut req_body = body.clone();
        if let Some(obj) = req_body.as_object_mut() {
            obj.insert("model".into(), serde_json::json!(llm_config.model_name));
            obj.insert("chat_template_kwargs".into(), serde_json::json!({"thinking": true, "reasoning_effort": llm_config.reasoning_effort.as_str()}));
            obj.insert("stream".into(), serde_json::json!(llm_config.stream));
        }
    let mut req = client.post(api_url).json(&req_body)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        if !llm_config.api_key.is_empty() && llm_config.api_key.to_lowercase() != "none" {
            req = req.header(&llm_config.auth_header,
                format!("{}{}", llm_config.auth_prefix, llm_config.api_key));
        }

        let send_future = req.send();
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), send_future).await {
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                info!("[{}] attempt={} http={} in={:.1}s", msg_id, attempt, status, req_start.elapsed().as_secs_f64());
                if status < 200 || status >= 300 {
                    last_err = format!("HTTP {}", status);
                } else {
                    let ka = if attempt == 1 { stop_keepalive.take() } else { None };
                    if forward_raw_sse(response, msg_id, tx, ka).await {
                        return;
                    }
                    last_err = "empty response".into();
                }
                last_err = "empty response".into();
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                info!("[{}] attempt={} conn_err={}", msg_id, attempt, last_err);
            }
            Err(_) => {
                last_err = format!("timeout {}s", timeout_secs);
                info!("[{}] attempt={} timeout={}s", msg_id, attempt, timeout_secs);
            }
        }

        if attempt < MAX_RETRIES {
            info!("[{}] retry {}/{} remaining={} err={}", msg_id, attempt, MAX_RETRIES, remaining - 1, last_err);
        }
    }

    warn!("[{}] ALL {} retries exhausted after {:.1}s: {}", msg_id, MAX_RETRIES, req_start.elapsed().as_secs_f64(), last_err);
    // 错误时发送简单 SSE 错误事件
    let _ = tx.send(Bytes::from(format!("data: [ERROR] downstream unavailable after {} retries: {}\n\n", MAX_RETRIES, last_err))).await;
}

/// 透传下游 SSE 流 — reasoning 包 <thinking> 标签，其余原样转发
async fn forward_raw_sse(
    response: reqwest::Response,
    msg_id: &str,
    tx: &mpsc::Sender<Bytes>,
    mut stop_keepalive: Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    use futures_util::StreamExt;
    let req_start = std::time::Instant::now();
    let mut stream = response.bytes_stream();
    let mut has_any_data = false;
    let mut first_token = true;
    let mut reasoning_open = false;
    let mut line_buf = String::new(); // 跨 chunk 行缓冲
    let mut content_buf = String::new();
    let mut intercept_active = false;
    let mut intercept_buffer = String::new();
    let mut active_close_tag = String::new();
    // 触发标签集合（与 stream.rs 保持一致，fullwidth pipe ｜ U+FF5C）
    let mut triggers: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    triggers.insert("<tool_call>", "</tool_call>");
    triggers.insert("```json", "```");
    triggers.insert("```tool_call", "```");
    // fullwidth pipe variants
    triggers.insert("<｜tool_calls｜>", "</｜tool_calls｜>");
    triggers.insert("<｜tool_call｜>", "</｜tool_call｜>");
    triggers.insert("<invoke", "</invoke>");
    triggers.insert("<parameter", "</parameter>");
    triggers.insert("<｜invoke｜>", "</｜invoke｜>");
    triggers.insert("<｜parameter｜>", "</｜parameter｜>");
    triggers.insert("<DSML｜tool_name｜>", "</DSML｜tool_name｜>");
    triggers.insert("<DSML｜tool_calls｜>", "</DSML｜tool_calls｜>");
    // katakana variants
    triggers.insert("<ツtool_callsツ>", "</ツtool_callsツ>");
    triggers.insert("<ツtool_callツ>", "</ツtool_callツ>");

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                if first_token {
                    first_token = false;
                    if let Some(s) = stop_keepalive.take() { let _ = s.send(()); }
                    info!("[rsp {}] first_token in {:.1}s chunk={}B", msg_id, req_start.elapsed().as_secs_f64(), chunk.len());
                }
                has_any_data = true;
                // 追加到行缓冲区，处理完整行，保留不完整的尾部
                line_buf.push_str(&String::from_utf8_lossy(&chunk));
                let mut modified = String::with_capacity(line_buf.len() + 128);
                // 找出最后一个换行之后的内容（可能不完整）
                let complete_end = if let Some(last_nl) = line_buf.rfind('\n') {
                    last_nl + 1
                } else {
                    0
                };
                let complete_text = line_buf[..complete_end].to_string();
                let incomplete_tail = line_buf[complete_end..].to_string();
                line_buf.clear();
                line_buf.push_str(&incomplete_tail);

                for line in complete_text.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        modified.push('\n');
                        continue;
                    }
                    if !trimmed.starts_with("data: ") {
                        modified.push_str(trimmed);
                        modified.push('\n');
                        continue;
                    }
                    let data_str = trimmed[6..].trim();
                    if data_str == "[DONE]" {
                        // 流结束：flush content buffer + 关闭 reasoning
                        if !content_buf.is_empty() {
                            let buf_json = serde_json::to_string(&content_buf).unwrap_or_default();
                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                            modified.push_str(&buf_json);
                            modified.push_str("}}]}\n\n");
                            content_buf.clear();
                        }
                        if reasoning_open {
                            let close_json = serde_json::to_string("</thinking>").unwrap_or_default();
                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                            modified.push_str(&close_json);
                            modified.push_str("}}]}\n\n");
                            info!("[rsp {}] 💭 reasoning END at DONE", msg_id);
                            reasoning_open = false;
                        }
                        modified.push_str("data: [DONE]\n\n");
                        continue;
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(data_str) {
                        Ok(v) => v,
                        Err(_) => { modified.push_str(trimmed); modified.push('\n'); continue; }
                    };
                    let delta = &parsed["choices"][0]["delta"];

                    let reasoning_text = delta["reasoning"].as_str().unwrap_or("").to_string()
                        + delta["reasoning_content"].as_str().unwrap_or("");
                    let content_text = delta["content"].as_str().unwrap_or("");
                    let has_tool_calls = !delta["tool_calls"].as_array().unwrap_or(&vec![]).is_empty();

                    // 1. 处理 reasoning（流式输出，<thinking> 包裹）
                    if !reasoning_text.is_empty() {
                        if !reasoning_open {
                            info!("[rsp {}] 💭 reasoning START", msg_id);
                            let tagged = format!("<thinking>{}", reasoning_text);
                            let tagged_json = serde_json::to_string(&tagged).unwrap_or_default();
                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                            modified.push_str(&tagged_json);
                            modified.push_str("}}]}\n\n");
                            reasoning_open = true;
                        } else {
                            info!("[rsp {}] 💭 reasoning delta", msg_id);
                            let r_json = serde_json::to_string(&reasoning_text).unwrap_or_default();
                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                            modified.push_str(&r_json);
                            modified.push_str("}}]}\n\n");
                        }
                    }

                    // 2. 处理 tool_calls（原样转发）
                    if has_tool_calls {
                        modified.push_str(trimmed);
                        modified.push('\n');
                        // 关闭 reasoning 防止干扰
                        if reasoning_open {
                            info!("[rsp {}] 💭 reasoning END → tool_calls", msg_id);
                            reasoning_open = false;
                        }
                        continue;
                    }

                    // 3. 处理 content：缓冲 + XML/DSML 工具调用拦截
                    if !content_text.is_empty() {
                        if intercept_active {
                            // 拦截模式：收集到 close_tag
                            intercept_buffer.push_str(&content_text);
                            if let Some(ci) = intercept_buffer.find(&active_close_tag) {
                                let close_end = ci + active_close_tag.len();
                                let full_xml = intercept_buffer[..close_end].to_string();
                                let remaining = intercept_buffer[close_end..].to_string();

                                // 尝试解析工具调用
                                let (tool_name, tool_args) = stream::parse_fallback_tool(
                                    &full_xml,
                                    &std::collections::HashMap::new(),
                                );
                                if tool_name != "unknown" {
                                    // 发 tool_call delta
                                    let tool_id = stream::gen_tool_id();
                                    let tc_json = serde_json::json!({
                                        "choices": [{"delta": {"tool_calls": [{
                                            "index": 0,
                                            "id": tool_id,
                                            "type": "function",
                                            "function": {"name": tool_name, "arguments": serde_json::to_string(&tool_args).unwrap_or_default()}
                                        }]}}]
                                    });
                                    modified.push_str("data: ");
                                    modified.push_str(&serde_json::to_string(&tc_json).unwrap_or_default());
                                    modified.push_str("\n\n");
                                    info!("[rsp {}] 🔧 XML/DSML tool_call: {}", msg_id, tool_name);
                                } else {
                                    // 不是工具调用，作为普通 content 输出
                                    warn!("[rsp {}] ⚠️ 拦截到无效标签，作为 content 输出", msg_id);
                                    let pre_json = serde_json::to_string(&full_xml).unwrap_or_default();
                                    modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                                    modified.push_str(&pre_json);
                                    modified.push_str("}}]}\n\n");
                                }

                                intercept_active = false;
                                intercept_buffer.clear();
                                content_buf = remaining;
                                if !content_buf.is_empty() {
                                    // 检查剩余文本中是否有触发标签
                                    let mut earliest_idx = None;
                                    let mut matched_tag = None;
                                    for (open_tag, _) in &triggers {
                                        if let Some(idx) = content_buf.find(open_tag) {
                                            if earliest_idx.is_none() || idx < earliest_idx.unwrap() {
                                                earliest_idx = Some(idx);
                                                matched_tag = Some(open_tag);
                                            }
                                        }
                                    }
                                    if let (Some(idx), Some(tag)) = (earliest_idx, matched_tag) {
                                        // 又出现触发标签
                                        let pre = content_buf[..idx].to_string();
                                        if !pre.is_empty() {
                                            // 发 content
                                            let content_str = if reasoning_open {
                                                format!("</thinking>{}", pre)
                                            } else {
                                                pre
                                            };
                                            if reasoning_open {
                                                info!("[rsp {}] 💭 reasoning END → 📝 content", msg_id);
                                                reasoning_open = false;
                                            }
                                            let c_json = serde_json::to_string(&content_str).unwrap_or_default();
                                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                                            modified.push_str(&c_json);
                                            modified.push_str("}}]}\n\n");
                                        }
                                        intercept_active = true;
                                        active_close_tag = triggers.get(tag).unwrap_or(&"").to_string();
                                        intercept_buffer = content_buf[idx..].to_string();
                                        content_buf.clear();
                                    } else {
                                        // 正常 flush
                                        let flush = std::mem::take(&mut content_buf);
                                        if !flush.is_empty() {
                                            let content_str = if reasoning_open {
                                                format!("</thinking>{}", flush)
                                            } else {
                                                flush
                                            };
                                            if reasoning_open {
                                                info!("[rsp {}] 💭 reasoning END → 📝 content", msg_id);
                                                reasoning_open = false;
                                            }
                                            let c_json = serde_json::to_string(&content_str).unwrap_or_default();
                                            modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                                            modified.push_str(&c_json);
                                            modified.push_str("}}]}\n\n");
                                        }
                                    }
                                }
                            }
                        } else {
                            // 非拦截模式：追加到 content buffer
                            content_buf.push_str(&content_text);

                            // 检查是否有触发标签
                            let mut earliest_idx = None;
                            let mut matched_tag = None;
                            for (open_tag, _) in &triggers {
                                if let Some(idx) = content_buf.find(open_tag) {
                                    if earliest_idx.is_none() || idx < earliest_idx.unwrap() {
                                        earliest_idx = Some(idx);
                                        matched_tag = Some(open_tag);
                                    }
                                }
                            }
                            if let (Some(idx), Some(tag)) = (earliest_idx, matched_tag) {
                                // 发 trigger 之前的文本
                                if idx > 0 {
                                    let pre = content_buf[..idx].to_string();
                                    let content_str = if reasoning_open {
                                        format!("</thinking>{}", pre)
                                    } else {
                                        pre
                                    };
                                    if reasoning_open {
                                        info!("[rsp {}] 💭 reasoning END → 📝 content", msg_id);
                                        reasoning_open = false;
                                    }
                                    let c_json = serde_json::to_string(&content_str).unwrap_or_default();
                                    modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                                    modified.push_str(&c_json);
                                    modified.push_str("}}]}\n\n");
                                }
                                intercept_active = true;
                                active_close_tag = triggers.get(tag).unwrap_or(&"").to_string();
                                intercept_buffer = content_buf[idx..].to_string();
                                content_buf.clear();
                            } else if content_buf.len() > 35 {
                                // buffer 够大且无触发标签 → 发送
                                let safe_cut = content_buf.len() - 35;
                                let send_len = content_buf.floor_char_boundary(safe_cut);
                                let send_text = content_buf[..send_len].to_string();
                                if !send_text.is_empty() {
                                    let content_str = if reasoning_open {
                                        format!("</thinking>{}", send_text)
                                    } else {
                                        send_text
                                    };
                                    if reasoning_open {
                                        info!("[rsp {}] 💭 reasoning END → 📝 content", msg_id);
                                        reasoning_open = false;
                                    }
                                    info!("[rsp {}] 📝 content delta", msg_id);
                                    let c_json = serde_json::to_string(&content_str).unwrap_or_default();
                                    modified.push_str("data: {\"choices\":[{\"delta\":{\"content\":");
                                    modified.push_str(&c_json);
                                    modified.push_str("}}]}\n\n");
                                }
                                content_buf = content_buf[send_len..].to_string();
                            }
                        }
                    }
                }
                if tx.send(Bytes::from(modified)).await.is_err() {
                    break;
                }
            }
            Err(_) => { break; }
        }
    }

    has_any_data
}

async fn handle_get_models() -> impl IntoResponse {
    Json(serde_json::json!({
        "active_llm": get_active_llm_name(),
        "models": get_llm_names()
    }))
}

async fn handle_select_model(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let name = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"status":"error","msg":"model required"})));
    }
    match crate::config::switch_active_llm(name) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status":"success","active_llm":name}))),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"status":"error","msg":e}))),
    }
}
