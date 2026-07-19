use tracing;
use std::time::Duration;

/// 判断是否需要在 finish 时注入恢复工具
/// 硬编码拦截 + LLM 智能判断双保险
pub fn should_recover(generated_text: &str, stop_reason: &str) -> Option<String> {
    // 0. stop_reason == 'length' → API 明确告知被截断，总是需要恢复
    if stop_reason == "length" {
        tracing::info!("🚨 [Recovery] stop_reason=length 触发恢复");
        return Some("stop_reason=length".into());
    }

    // 1. 防止死循环：如果已经包含恢复/错误标记，直接不恢复
    if generated_text.contains("[holoProxy Recovery") || generated_text.contains("[holoProxy Error") {
        tracing::debug!("[Recovery] 检测到恢复/错误标记，跳过恢复防止死循环");
        return None;
    }

    // 2. 硬编码拦截 API 错误/超时/网关异常关键词
    // 防止 LLM 将"完整的错误提示"误判为"正常结束 COMPLETE"
    let lower_text = generated_text.to_lowercase();
    let error_keywords = [
        "timed out", "empty or malformed response", "api error",
        "operation timed out", "malformed", "connection",
        "unreachable", "502", "503", "504", "gateway",
        "proxy error", "internal server error", "connection refused",
        "connection reset", "network error", "request failed",
    ];
    
    for keyword in &error_keywords {
        if lower_text.contains(keyword) {
            tracing::warn!(
                "🚨 [Recovery] 硬编码拦截触发 | keyword={} | snippet={}",
                keyword,
                &generated_text[..generated_text.len().min(200)]
            );
            return Some(format!("detected API error keyword: {}", keyword));
        }
    }

    // 3. 空文本或纯空白文本：视为异常中断，直接触发恢复
    if generated_text.trim().is_empty() {
        tracing::warn!("[Recovery] 空文本或纯空白文本，触发恢复");
        return Some("empty or whitespace-only text".into());
    }

    // 4. 获取当前激活的 LLM 配置
    let config = match crate::config::get_active_llm_config() {
        Some(c) => c,
        None => {
            tracing::warn!("[Recovery] 无法获取 LLM 配置，保守策略：触发恢复");
            return Some("no active LLM config, conservative recovery".into());
        }
    };

    // 5. 开一个独立线程专门去咨询 LLM，避免阻塞主异步运行时
    let text = generated_text.to_string();
    let handle = std::thread::Builder::new()
        .name("recovery-llm-check".into())
        .spawn(move || {
            tracing::info!(
                "🔍 [Recovery] LLM 语义判断启动 | text_len={}B | model={}",
                text.len(),
                config.model_name
            );

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime for recovery check");

            rt.block_on(async { ask_llm_if_incomplete(&text, &config).await })
        })
        .expect("failed to spawn recovery check thread");

    // 6. 等待线程结果
    match handle.join() {
        Ok(Some(reason)) => {
            tracing::info!("✅ [Recovery] LLM 判断结论：{}", reason);
            Some(reason)
        }
        Ok(None) => {
            tracing::debug!("✅ [Recovery] LLM 判断为正常结束，跳过恢复");
            None
        }
        Err(_) => {
            tracing::error!("❌ [Recovery] LLM 判断线程崩溃，保守策略：触发恢复");
            Some("LLM check thread panicked, conservative recovery".into())
        }
    }
}

/// 异步咨询 LLM 判断文本是否不完整
/// 
/// 返回 Some(reason) 表示需要恢复，None 表示正常结束
async fn ask_llm_if_incomplete(text: &str, config: &crate::types::LLMConfig) -> Option<String> {
    // 安全截取最后约 2000 字节，判断截断主要看结尾上下文，节省 Token
    let text_to_check = if text.len() > 2000 {
        let start = text.len().saturating_sub(2000);
        let mut safe_start = start;
        while safe_start < text.len() && !text.is_char_boundary(safe_start) {
            safe_start += 1;
        }
        format!("... [truncated, showing last 2000 bytes] ...\n{}", &text[safe_start..])
    } else {
        text.to_string()
    };

    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    // 增强版 prompt：明确定义场景 + 详细判断标准 + 防误判指南
    let system_prompt = r#"You are an expert AI Agent response quality analyzer for Claude Code proxy recovery system.
Your CRITICAL task: Decide if a response needs emergency recovery (fake tool call injection) to prevent Claude Code from stopping.

CONTEXT:
- Claude Code expects either: (a) tool calls to continue working, OR (b) a natural conversation ending
- If response is cut off mid-stream WITHOUT tool call AND WITHOUT natural ending → Claude Code will crash
- Your judgment determines whether to inject a fake tool call to keep Claude Code running

THREE SCENARIOS - READ CAREFULLY:

1. NORMAL STOP → Reply "COMPLETE" (NO recovery needed):
   - Clear task completion statement: "任务完成了", "Done", "Work finished"
   - Natural question to user: "有什么问题吗？", "Need clarification?", "Shall I continue?"
   - Summary/conclusion: "总结：...", "In summary...", "The solution is..."
   - Polite closing: "以上就是分析", "That's all for today", "Hope this helps"
   - Waiting for user input explicitly: "等你确认后再继续", "Waiting for your confirmation"
   KEY SIGNAL: The assistant clearly finished its turn and is waiting for user or done with task.

2. API/NETWORK ERROR MESSAGE → Reply "INCOMPLETE" (RECOVERY NEEDED):
   - HTTP errors: "502 Bad Gateway", "503 Service Unavailable", "504 Gateway Timeout"
   - Connection issues: "Connection refused", "Connection reset", "Network error"
   - Timeout messages: "timed out", "operation timed out", "request timeout"
   - Empty/malformed responses: "empty response", "malformed response", "API Error"
   - Proxy/gateway errors: "proxy error", "gateway error", "upstream error"
   KEY SIGNAL: This is a SYSTEM/PROXY error message, NOT a normal assistant reply.
   IMPORTANT: Even if the error message looks "complete", the ACTUAL assistant response was cut off.

3. ABNORMAL CUT-OFF / INCOMPLETE OUTPUT → Reply "INCOMPLETE" (RECOVERY NEEDED):
   - Mid-sentence: "The solution is to imp", "我们需要先检查", "Let me explain"
   - Mid-code-block: "```python\ndef main(", "```rust\npub fn"
   - Mid-thought: "First, I need to", "第一步是", "Looking at the"
   - Trailing without conclusion: Ends with "...", "etc.", unfinished list
   - Partial JSON/XML/tool tags: incomplete structures
   KEY SIGNAL: The response was clearly interrupted before completion.

CRITICAL RULES:
- When in doubt, lean towards "INCOMPLETE" to prevent Claude Code crash (conservative approach)
- Error messages from proxy/API are ALWAYS "INCOMPLETE" even if they look grammatically complete
- Only reply "COMPLETE" when you see clear task completion or natural conversation pause
- False positive (injecting when not needed) wastes tokens but keeps Claude Code running
- False negative (not injecting when needed) causes Claude Code to stop → WORSE outcome

OUTPUT FORMAT:
Reply EXACTLY one word: "COMPLETE" or "INCOMPLETE"
No explanation, no punctuation, just the word."#;

    let body = serde_json::json!({
        "model": config.model_name,
        "messages": [
            {
                "role": "system",
                "content": system_prompt
            },
            {
                "role": "user",
                "content": format!("Analyze this assistant output:\n\n{}", text_to_check)
            }
        ],
        "temperature": 0.0,
        "max_tokens": 10,
        "stream": false
    });

    // 构建带超时的客户端：增加连接池禁用和更严格的超时设置，防止连接复用导致的问题
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(!config.verify_ssl)
        .timeout(Duration::from_secs(8))  // 8 秒总超时，防止 LLM 判断卡住
        .connect_timeout(Duration::from_secs(3))  // 3 秒连接超时
        .pool_max_idle_per_host(0)  // 禁用连接池，每次新建连接
        .tcp_nodelay(true)  // 禁用 Nagle 算法，降低延迟
        .build()
        .unwrap_or_default();

    let req = client
        .post(&url)
        .header(
            config.auth_header.as_str(),
            format!("{}{}", config.auth_prefix, config.api_key),
        )
        .json(&body);

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            // 检查 HTTP 状态码
            if !status.is_success() {
                tracing::warn!(
                    "❌ [Recovery] LLM 判断请求返回非成功状态码：status={} | url={}",
                    status,
                    url
                );
                // 网络错误时保守策略：触发恢复（宁可误判不要漏判）
                return Some(format!("LLM API returned non-success status {}, conservative recovery", status));
            }
            
            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    // 尝试从多种可能的位置提取回复内容，支持不同的 API 响应格式
                    let content = json["choices"][0]["message"]["content"]
                        .as_str()
                        .or_else(|| json["choices"][0]["message"]["content"].get("text").and_then(|v| v.as_str()))
                        .or_else(|| json["choices"][0].get("text").and_then(|v| v.as_str()))
                        .map(|s| s.trim())
                        .unwrap_or("");
                    
                    if content.is_empty() {
                        tracing::warn!("[Recovery] LLM 返回空内容，保守策略：触发恢复 | raw_json={}", 
                            serde_json::to_string(&json).unwrap_or_default());
                        return Some("LLM returned empty content, conservative recovery".into());
                    }
                    
                    let lower = content.to_lowercase();
                    let verdict = if lower.contains("incomplete") {
                        "INCOMPLETE"
                    } else if lower.contains("complete") && !lower.contains("incomplete") {
                        "COMPLETE"
                    } else {
                        "UNKNOWN"
                    };
                    
                    tracing::info!(
                        "🧠 [Recovery] LLM 判断结论 | verdict={} | raw_response={} | text_len={}B | model={}",
                        verdict,
                        content,
                        text.len(),
                        config.model_name
                    );
                    
                    match verdict {
                        "INCOMPLETE" => {
                            return Some(format!("LLM judged as INCOMPLETE: {}", content));
                        }
                        "COMPLETE" => {
                            tracing::debug!("✅ [Recovery] LLM 判断为正常结束，跳过恢复");
                            return None;
                        }
                        _ => {
                            // 无法识别 LLM 回复，保守策略：触发恢复
                            tracing::warn!(
                                "⚠️ [Recovery] LLM 返回无法识别的格式：raw={}, 保守策略：触发恢复",
                                content
                            );
                            return Some(format!("LLM response unrecognizable: {}, conservative recovery", content));
                        }
                    }
                }
                Err(parse_err) => {
                    tracing::warn!(
                        "❌ [Recovery] LLM 响应解析失败：error={}, 保守策略：触发恢复 | status={}",
                        parse_err,
                        status
                    );
                    // 解析失败时保守策略：触发恢复
                    Some(format!("LLM response parse failed: {}, conservative recovery", parse_err))
                }
            }
        }
        Err(send_err) => {
            // 发送失败（网络错误、超时等）
            let err_str = send_err.to_string();
            let err_type = if send_err.is_timeout() { "timeout" } 
                else if send_err.is_connect() { "connect_error" }
                else if send_err.is_request() { "request_error" }
                else { "other" };
            tracing::error!(
                "❌ [Recovery] LLM 判断请求失败：error={} | type={} | url={}",
                err_str,
                err_type,
                url
            );
            // 网络错误时保守策略：宁可误判也要确保 Claude Code 不停工
            Some(format!("LLM request failed ({}), conservative recovery", err_type))
        }
    }
}

/// 动态选取恢复工具：优先 Bash/Shell/RunCommand/Execute
pub fn pick_recovery_tool(
    valid_tools: &std::collections::HashMap<String, &crate::types::ToolDef>,
) -> Option<(String, serde_json::Value)> {
    if valid_tools.is_empty() {
        tracing::warn!("[Recovery] 没有可用工具，无法注入恢复工具调用");
        return None;
    }
    let priority_names = [
        "Bash", "Shell", "bash", "shell",
        "Execute", "Run_Command", "RunCommand", "terminal", "Terminal",
    ];
    for name in &priority_names {
        if let Some(tool) = valid_tools.get(*name) {
            tracing::info!("[Recovery] 选择优先级工具：name={}", name);
            return Some((name.to_string(), build_recovery_args(tool)));
        }
    }
    // 查找任何带有 command 参数的工具
    for (name, tool) in valid_tools.iter() {
        let props = tool
            .input_schema
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props_map) = props {
            if props_map.contains_key("command") {
                tracing::info!("[Recovery] 选择带 command 参数的工具：name={}", name);
                return Some((
                    name.clone(),
                    serde_json::json!({
                        "command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"
                    }),
                ));
            }
        }
    }
    //  fallback: 选择第一个可用工具
    let (name, tool) = valid_tools.iter().next().unwrap();
    tracing::info!("[Recovery] 使用 fallback 工具：name={}", name);
    Some((name.clone(), build_recovery_args(tool)))
}

fn build_recovery_args(tool: &crate::types::ToolDef) -> serde_json::Value {
    let props = tool
        .input_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object());
    if let Some(props_map) = props {
        if props_map.contains_key("command") {
            return serde_json::json!({"command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"});
        }
        if props_map.contains_key("path") {
            return serde_json::json!({"path": "./"});
        }
        if props_map.contains_key("query") {
            return serde_json::json!({"query": "*"});
        }
        if let Some((first_key, _)) = props_map.iter().next() {
            return serde_json::json!({first_key: "echo recovery"});
        }
    }
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_recover_length() {
        assert!(should_recover("some text", "length").is_some());
    }

    #[test]
    fn test_prevent_infinite_loop() {
        assert!(should_recover("[holoProxy Recovery] some text", "stop").is_none());
    }

    #[test]
    fn test_empty_text_recovery() {
        assert!(should_recover("", "stop").is_some());
        assert!(should_recover(" \n\t ", "stop").is_some());
    }

    #[test]
    fn test_api_error_intercept() {
        // 验证硬编码拦截 API 错误/超时关键词
        assert!(should_recover("API Error: The operation timed out.", "stop").is_some());
        assert!(should_recover("API Error: API returned an empty or malformed response (HTTP 200)", "stop").is_some());
        assert!(should_recover("Some normal text. API Error occurred.", "stop").is_some());
        assert!(should_recover("502 Bad Gateway", "stop").is_some());
        assert!(should_recover("Connection refused", "stop").is_some());
        assert!(should_recover("Internal Server Error", "stop").is_some());
    }
}
