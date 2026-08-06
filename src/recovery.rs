use tracing;

/// Fast-path hardcoded checks for recovery trigger.
/// These run synchronously and cover the most common error patterns.
/// Returns Some(reason) if recovery is needed, None if more analysis is required.
fn hardcoded_checks(generated_text: &str, stop_reason: &str) -> Option<String> {
    // 0. stop_reason == 'length' → API explicitly says truncated, always recover
    if stop_reason == "length" {
        tracing::info!("🚨 [Recovery] stop_reason=length triggers recovery");
        return Some("stop_reason=length".into());
    }

    // 1. Prevent infinite loop: skip if recovery/error markers already present
    if generated_text.contains("[holoProxy Recovery") || generated_text.contains("[holoProxy Error") {
        tracing::debug!("[Recovery] recovery/error markers detected, skip to prevent infinite loop");
        return None;
    }

    // 2. Hardcoded API error/timeout/gateway exception keyword interception
    // Prevents LLM from misjudging "complete error messages" as "normal completion COMPLETE"
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
                "🚨 [Recovery] hardcoded intercept triggered | keyword={} | snippet={}",
                keyword,
                &generated_text[..generated_text.len().min(200)]
            );
            return Some(format!("detected API error keyword: {}", keyword));
        }
    }

    // 3. Empty or whitespace-only text: abnormal cutoff, trigger recovery
    if generated_text.trim().is_empty() {
        tracing::warn!("[Recovery] empty or whitespace-only text, trigger recovery");
        return Some("empty or whitespace-only text".into());
    }

    // Need LLM semantic judgment
    None
}

/// Async recovery check: hardcoded interception + LLM semantic judgment dual insurance.
/// Called from StreamContext::finish() with tokio::time::timeout for non-blocking behavior.
pub async fn should_recover_async(generated_text: &str, stop_reason: &str) -> Option<String> {
    // Fast path: hardcoded checks (no I/O, completes instantly)
    if let Some(reason) = hardcoded_checks(generated_text, stop_reason) {
        return Some(reason);
    }

    // Slow path: consult LLM for semantic judgment
    let config = match crate::config::get_active_llm_config() {
        Some(c) => c,
        None => {
            tracing::warn!("[Recovery] cannot get LLM config, conservative: trigger recovery");
            return Some("no active LLM config, conservative recovery".into());
        }
    };

    tracing::info!(
        "🔍 [Recovery] LLM semantic check started | text_len={}B | model={}",
        generated_text.len(),
        config.model_name
    );

    ask_llm_if_incomplete(generated_text, &config).await
}

/// Ask downstream LLM to judge whether text is incomplete.
///
/// Returns Some(reason) if recovery needed, None if normal completion.
async fn ask_llm_if_incomplete(text: &str, config: &crate::types::LLMConfig) -> Option<String> {
    // Trim to last ~2000 bytes — judgment focuses on ending context, saves tokens
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

    let base = config.base_url.trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    };

    // Enhanced prompt: clearly defined scenarios + detailed criteria + anti-false-positive guide
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
- When in doubt, lean towards "COMPLETE" — normal conversation endings are far more common than true cutoffs
- Error messages from proxy/API are ALWAYS "INCOMPLETE" even if they look grammatically complete
- Reply "COMPLETE" for any natural ending: summary given, question asked, task described as done, file written, etc.
- Reply "INCOMPLETE" ONLY when the text is clearly mid-sentence, mid-code-block, or an API error message
- Think step by step: "Is this text clearly interrupted mid-thought?" If not sure → COMPLETE

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

    // Build client with timeout — reuse server's fresh_client for consistency
    let client = crate::server::fresh_client();

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
            if !status.is_success() {
                tracing::warn!(
                    "❌ [Recovery] LLM check returned non-success status: status={} | url={}",
                    status,
                    url
                );
                return Some(format!("LLM API returned non-success status {}, conservative recovery", status));
            }

            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    let content = json["choices"][0]["message"]["content"]
                        .as_str()
                        .or_else(|| json["choices"][0]["message"]["content"].get("text").and_then(|v| v.as_str()))
                        .or_else(|| json["choices"][0].get("text").and_then(|v| v.as_str()))
                        .map(|s| s.trim())
                        .unwrap_or("");

                    if content.is_empty() {
                        tracing::warn!("[Recovery] LLM returned empty content, conservative: trigger recovery | raw_json={}",
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
                        "🧠 [Recovery] LLM verdict | verdict={} | raw_response={} | text_len={}B | model={}",
                        verdict,
                        content,
                        text.len(),
                        config.model_name
                    );

                    match verdict {
                        "INCOMPLETE" => {
                            Some(format!("LLM judged as INCOMPLETE: {}", content))
                        }
                        "COMPLETE" => {
                            tracing::debug!("✅ [Recovery] LLM judged as normal completion, skip recovery");
                            None
                        }
                        _ => {
                            tracing::warn!(
                                "⚠️ [Recovery] LLM returned unrecognized format: raw={}, conservative: trigger recovery",
                                content
                            );
                            Some(format!("LLM response unrecognizable: {}, conservative recovery", content))
                        }
                    }
                }
                Err(parse_err) => {
                    tracing::warn!(
                        "❌ [Recovery] LLM response parse failed: error={}, conservative: trigger recovery | status={}",
                        parse_err,
                        status
                    );
                    Some(format!("LLM response parse failed: {}, conservative recovery", parse_err))
                }
            }
        }
        Err(send_err) => {
            let err_str = send_err.to_string();
            let err_type = if send_err.is_timeout() { "timeout" }
                else if send_err.is_connect() { "connect_error" }
                else if send_err.is_request() { "request_error" }
                else { "other" };
            tracing::error!(
                "❌ [Recovery] LLM check request failed: error={} | type={} | url={}",
                err_str,
                err_type,
                url
            );
            Some(format!("LLM request failed ({}), conservative recovery", err_type))
        }
    }
}

/// Dynamically select recovery tool: prioritize Bash/Shell/RunCommand/Execute.
pub fn pick_recovery_tool(
    valid_tools: &std::collections::HashMap<String, &crate::types::ToolDef>,
) -> Option<(String, serde_json::Value)> {
    if valid_tools.is_empty() {
        tracing::warn!("[Recovery] no tools available, cannot inject recovery tool call");
        return None;
    }
    let priority_names = [
        "Bash", "Shell", "bash", "shell",
        "Execute", "Run_Command", "RunCommand", "terminal", "Terminal",
    ];
    for name in &priority_names {
        if let Some(tool) = valid_tools.get(*name) {
            tracing::info!("[Recovery] selected priority tool: name={}", name);
            return Some((name.to_string(), build_recovery_args(tool)));
        }
    }
    // Find any tool with a command parameter
    for (name, tool) in valid_tools.iter() {
        let props = tool
            .input_schema
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props_map) = props {
            if props_map.contains_key("command") {
                tracing::info!("[Recovery] selected tool with command param: name={}", name);
                return Some((
                    name.clone(),
                    serde_json::json!({
                        "command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"
                    }),
                ));
            }
        }
    }
    // Fallback: first available tool
    let (name, tool) = valid_tools.iter().next().unwrap();
    tracing::info!("[Recovery] using fallback tool: name={}", name);
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
        assert!(hardcoded_checks("some text", "length").is_some());
    }

    #[test]
    fn test_prevent_infinite_loop() {
        assert!(hardcoded_checks("[holoProxy Recovery] some text", "stop").is_none());
    }

    #[test]
    fn test_empty_text_recovery() {
        assert!(hardcoded_checks("", "stop").is_some());
        assert!(hardcoded_checks(" \n\t ", "stop").is_some());
    }

    #[test]
    fn test_api_error_intercept() {
        assert!(hardcoded_checks("API Error: The operation timed out.", "stop").is_some());
        assert!(hardcoded_checks("API Error: API returned an empty or malformed response (HTTP 200)", "stop").is_some());
        assert!(hardcoded_checks("Some normal text. API Error occurred.", "stop").is_some());
        assert!(hardcoded_checks("502 Bad Gateway", "stop").is_some());
        assert!(hardcoded_checks("Connection refused", "stop").is_some());
        assert!(hardcoded_checks("Internal Server Error", "stop").is_some());
    }
}
