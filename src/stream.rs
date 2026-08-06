use crate::recovery;
use crate::types::*;
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use uuid::Uuid;

/// SSE 流处理状态机 — 将 OpenAI SSE 流转换为 Anthropic SSE 流
pub struct StreamContext {
    pub msg_id: String,
    pub model_name: String,
    pub is_agent_mode: bool,
    pub valid_tools: HashMap<String, ToolDef>,
    // 内部状态
    block_idx: u32,
    text_open: bool,
    tool_open: bool,
    thinking_open: bool,
    has_tool_use: bool,
    pub generated_text: String,
    // XML 拦截相关
    text_buffer: String,
    intercept_active: bool,
    intercept_buffer: String,
    active_close_tag: String,
    valid_triggers: HashMap<String, String>,
    // 原生 tool_calls 追踪
    active_native_tools: HashMap<u32, u32>,
    // 输出通道（流式发送，不缓冲）
    tx: UnboundedSender<Bytes>,
}

impl StreamContext {
    pub fn new(
        msg_id: String,
        model_name: String,
        is_agent_mode: bool,
        valid_tools: HashMap<String, ToolDef>,
        tx: UnboundedSender<Bytes>,
    ) -> Self {
        // TWO detection paths for tool calls:
        //
        // Path A: ds2api-style DSML wrappers (normalized via normalize_dsml_tags())
        //   <|DSML|tool_calls> → <tool_calls>, <DSML|invoke → <invoke
        //
        // Path B: Raw DeepSeek internal DSML format (also normalized)
        //   <｜tool_calls_begin｜> → wrapper open
        //   <｜tool_call_begin｜>  → individual call begin
        //   <｜tool_sep｜>         → separator (ignored)
        //   <｜tool_call_argument_begin｜> → args begin
        //   <｜tool_call_end｜>    → individual call end
        //   <｜tool_calls_end｜>   → wrapper close
        //
        // After normalization, only these canonical forms are checked:
        //   <tool_calls → close="tool_calls" (wrapper)
        //   <invoke    → close="invoke"    (tool call)
        //   <parameter → close="parameter" (parameter)
        let mut valid_triggers: HashMap<String, String> = HashMap::new();
        valid_triggers.insert("<tool_calls".into(), "tool_calls".into());
        valid_triggers.insert("<invoke".into(), "invoke".into());
        valid_triggers.insert("<parameter".into(), "parameter".into());

        let mut ctx = Self {
            msg_id,
            model_name,
            is_agent_mode,
            valid_tools,
            block_idx: 0,
            text_open: false,
            tool_open: false,
            thinking_open: false,
            has_tool_use: false,
            generated_text: String::new(),
            text_buffer: String::new(),
            intercept_active: false,
            intercept_buffer: String::new(),
            active_close_tag: String::new(),
            valid_triggers,
            active_native_tools: HashMap::new(),
            tx,
        };

        // 发送 message_start
        ctx.send_event(
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": ctx.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": ctx.model_name,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        );

        ctx
    }

    /// Send SSE event directly through the output channel (no buffering).
    fn send_event(&mut self, event_type: &str, data: &serde_json::Value) {
        let payload = format!(
            "event: {}\ndata: {}\n\n",
            event_type,
            serde_json::to_string(data).unwrap_or_default()
        );
        // UnboundedSender::send is sync and infallible — only fails if receiver dropped
        let _ = self.tx.send(Bytes::from(payload));
    }

    /// 刷新 text_buffer 中的缓冲文本（仅在非拦截模式下执行）
    fn flush_text_buffer(&mut self) {
        if !self.text_buffer.is_empty() && !self.intercept_active {
            let remaining = std::mem::take(&mut self.text_buffer);
            self.send_text_delta(&remaining);
        }
    }

    /// 在阶段切换（tool_call/reasoning）前处理未完成的拦截缓冲
    fn finalize_intercept(&mut self) {
        while self.intercept_active {
            let buffer = std::mem::take(&mut self.intercept_buffer);

            // 尝试找到闭合标签（DSML 规范化后匹配）；找不到则处理整个缓冲
            let norm_buf = normalize_dsml_tags(&buffer);
            let (full_xml, remaining) = if let Some(orig_end) =
                find_close_tag_end_in_original(&buffer, &norm_buf, &self.active_close_tag)
            {
                (buffer[..orig_end].to_string(), buffer[orig_end..].to_string())
            } else {
                (buffer, String::new())
            };

            let (tool_name, tool_args) = parse_fallback_tool(&full_xml, &self.valid_tools);
            if tool_name != "unknown" && self.valid_tools.contains_key(&tool_name) {
                let tool_id = gen_tool_id();
                self.open_tool(&tool_id, &tool_name);
                let args_str =
                    serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
                self.send_tool_delta(&args_str);
                self.close_tool();
            } else {
                warn!(
                    "⚠️ [XML Parse] finalize_intercept 拦截到无效工具标签，跳过: {}",
                    &full_xml[..full_xml.len().min(100)]
                );
            }

            self.intercept_active = false;

            if !remaining.is_empty() {
                self.text_buffer = remaining;
                self.check_text_buffer_triggers();
                // 如果 check_text_buffer_triggers 发现了新触发标签，
                // intercept_active 会再次变为 true，循环继续
            } else {
                self.text_buffer.clear();
                break;
            }
        }
    }

    fn ensure_text_open(&mut self) {
        if self.thinking_open {
            self.close_thinking();
        }
        if self.tool_open {
            self.close_tool();
        }
        if !self.text_open {
            self.send_event(
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": self.block_idx,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            self.text_open = true;
        }
    }

    fn send_text_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.generated_text.push_str(text);
        self.ensure_text_open();
        self.send_event(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": self.block_idx,
                "delta": {"type": "text_delta", "text": text}
            }),
        );
    }

    fn close_text(&mut self) {
        if self.text_open {
            self.send_event(
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": self.block_idx
                }),
            );
            self.text_open = false;
            self.block_idx += 1;
        }
    }

    fn open_tool(&mut self, tool_id: &str, name: &str) {
        // 刷新缓冲文本，防止内容丢失
        self.flush_text_buffer();

        if self.thinking_open {
            self.close_thinking();
        }
        if self.text_open {
            self.close_text();
        }
        if self.tool_open {
            self.close_tool();
        }
        self.send_event(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": self.block_idx,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_id,
                    "name": name,
                    "input": {}
                }
            }),
        );
        self.tool_open = true;
        self.has_tool_use = true;
    }

    fn send_tool_delta(&mut self, args_json: &str) {
        if !self.tool_open || args_json.is_empty() {
            return;
        }
        self.send_event(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": self.block_idx,
                "delta": {"type": "input_json_delta", "partial_json": args_json}
            }),
        );
    }

    fn close_tool(&mut self) {
        if self.tool_open {
            self.send_event(
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": self.block_idx
                }),
            );
            self.tool_open = false;
            self.block_idx += 1;
        }
    }

    /// 处理 OpenAI SSE delta content
    pub fn handle_content(&mut self, content: &str) {
        if !self.text_open && !self.intercept_active {
            info!("[{}] 📝 content block START", self.msg_id);
        }
        if self.intercept_active {
            // 拦截模式：收集到 active_close_tag（DSML 规范化后匹配）
            self.intercept_buffer.push_str(content);
            let norm_buf = normalize_dsml_tags(&self.intercept_buffer);
            if norm_buf.contains(&self.active_close_tag) {
                let orig_end = find_close_tag_end_in_original(
                    &self.intercept_buffer, &norm_buf, &self.active_close_tag,
                );
                let close_idx = orig_end.unwrap_or(self.intercept_buffer.len());
                let full_xml = self.intercept_buffer[..close_idx].to_string();
                let remaining = self.intercept_buffer[close_idx..].to_string();

                let (tool_name, tool_args) = parse_fallback_tool(&full_xml, &self.valid_tools);
                if tool_name != "unknown" && self.valid_tools.contains_key(&tool_name) {
                    let tool_id = gen_tool_id();
                    self.open_tool(&tool_id, &tool_name);
                    let args_str =
                        serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
                    self.send_tool_delta(&args_str);
                    self.close_tool();
                } else {
                    warn!(
                        "⚠️ [XML Parse] 拦截到无效的工具标签格式，跳过: {}",
                        &full_xml[..full_xml.len().min(100)]
                    );
                }

                self.intercept_active = false;
                self.intercept_buffer.clear();

                // 处理 close tag 之后的剩余内容
                if !remaining.is_empty() {
                    self.text_buffer = remaining;
                    // 重新检查是否有新的触发标签
                    self.check_text_buffer_triggers();
                } else {
                    self.text_buffer.clear();
                }
            }
        } else {
            self.text_buffer.push_str(content);
            self.check_text_buffer_triggers();
        }
    }

    fn check_text_buffer_triggers(&mut self) {
        if self.intercept_active {
            return;
        }

        // DSML normalization: convert <|DSML|tag>, <|tool_calls_begin|>, CJK brackets, etc.
        // to canonical <tag> before trigger matching (ds2api-compatible approach).
        let normalized = normalize_dsml_tags(&self.text_buffer);

        // Step 1: find earliest trigger (immutable scan)
        let trigger_info: Option<(usize, String)> = {
            let mut earliest_idx: Option<usize> = None;
            let mut matched_tag_base: Option<String> = None;

            for (open_tag, close_tag_base) in &self.valid_triggers {
                if let Some(idx) = normalized.find(open_tag.as_str()) {
                    if earliest_idx.is_none() || idx < earliest_idx.unwrap() {
                        earliest_idx = Some(idx);
                        matched_tag_base = Some(close_tag_base.clone());
                    }
                }
            }

            earliest_idx.map(|idx| (idx, matched_tag_base.unwrap()))
        };

        // Step 2: process trigger (mutable operations)
        if let Some((idx, tag_base)) = trigger_info {
            let close_tag = format!("</{}", tag_base);
            if tag_base.is_empty() {
                return;
            }

            // Send text BEFORE the trigger (from normalized positions)
            if idx > 0 {
                let pre_text = normalized[..idx].to_string();
                self.send_text_delta(&pre_text);
            }

            // Map normalized position back to original text for intercept buffer
            let orig_idx = self.find_original_tag_pos(&normalized, idx, &tag_base);
            self.intercept_active = true;
            self.active_close_tag = close_tag;
            self.intercept_buffer = self.text_buffer[orig_idx..].to_string();
            self.text_buffer.clear();
        } else if self.text_buffer.len() > 50 {
            let safe_cut = self.text_buffer.len() - 50;
            let send_len = self.text_buffer.floor_char_boundary(safe_cut);
            let send_text = self.text_buffer[..send_len].to_string();
            self.send_text_delta(&send_text);
            self.text_buffer = self.text_buffer[send_len..].to_string();
        }
    }

    /// Map normalized trigger position back to original text position.
    /// Scans for the first occurrence of the tag pattern in original text
    /// starting from the mapped position.
    fn find_original_tag_pos(&self, _normalized: &str, norm_idx: usize, open_tag_base: &str) -> usize {
        // Simple heuristic: normalized and original differ only in DSML prefix chars,
        // so original position ≈ norm_idx + (prefix chars already consumed).
        // For robustness, scan the original buffer near the expected position.
        let estimated = norm_idx.min(self.text_buffer.len());
        // Search backward from estimate for the '<' that starts the tag
        let search_start = estimated.saturating_sub(30);
        let search_slice = &self.text_buffer[search_start..];
        if let Some(pos) = search_slice.find('<') {
            let orig_pos = search_start + pos;
            // Verify: after normalizing, this position should produce the canonical tag
            let test = normalize_dsml_tags(&self.text_buffer[orig_pos..]);
            let expected = format!("<{}", open_tag_base);
            if test.starts_with(&expected) {
                return orig_pos;
            }
        }
        // Fallback: use estimate
        estimated
    }

    /// 处理原生 tool_calls delta
    pub fn handle_tool_call(&mut self, tc: &OpenAIToolCallDelta) {
        // 阶段切换：先处理未完成的拦截缓冲
        self.finalize_intercept();
        // 刷新文本缓冲，防止内容丢失
        self.flush_text_buffer();

        let idx = tc.index.unwrap_or(0);
        if !self.active_native_tools.contains_key(&idx) {
            let name = tc
                .function
                .as_ref()
                .and_then(|f| f.name.as_ref())
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            self.open_tool(tc.id.as_deref().unwrap_or(&gen_tool_id()), &name);
            self.active_native_tools.insert(idx, self.block_idx.saturating_sub(1));
        }
        if let Some(ref func) = tc.function {
            if let Some(ref args) = func.arguments {
                self.send_tool_delta(args);
            }
        }
    }

    /// 处理 reasoning / reasoning_content — 输出为独立 thinking content_block
    pub fn handle_reasoning(&mut self, text: &str) {
        if !self.thinking_open {
            // 阶段切换：先处理未完成的拦截缓冲
            self.finalize_intercept();
            // 刷新文本缓冲，防止内容丢失
            self.flush_text_buffer();

            info!("[{}] 💭 reasoning block START", self.msg_id);
            if self.text_open { self.close_text(); }
            if self.tool_open { self.close_tool(); }
            self.send_event(
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": self.block_idx,
                    "content_block": {"type": "thinking", "thinking": ""}
                }),
            );
            self.thinking_open = true;
        }
        self.generated_text.push_str(text);
        self.send_event(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": self.block_idx,
                "delta": {"type": "thinking_delta", "thinking": text}
            }),
        );
    }

    fn close_thinking(&mut self) {
        if self.thinking_open {
            info!("[{}] 💭 reasoning block END", self.msg_id);
            self.send_event(
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": self.block_idx
                }),
            );
            self.thinking_open = false;
            self.block_idx += 1;
        }
    }

    /// 结束流：关闭所有开放块 + 自动恢复判断 + 发送 message_delta/message_stop。
    /// 现在是异步的 — recovery LLM 检查使用 tokio::time::timeout 避免阻塞。
    pub async fn finish(&mut self, upstream_stop_reason: &str) {
        // 保底：防止空响应（只考虑 text/tool/thinking）
        if !self.text_open && !self.thinking_open && !self.has_tool_use {
            self.send_text_delta(" ");
        }

        // 刷新 text_buffer 中剩余的内容
        self.flush_text_buffer();

        // 如果拦截模式未关闭，处理剩余拦截缓冲
        self.finalize_intercept();
        // finalize_intercept 可能留下少量文本在 text_buffer 中
        self.flush_text_buffer();

        self.close_thinking();
        self.close_text();
        self.close_tool();

        // 关闭所有原生 tool_calls
        for _ in 0..self.active_native_tools.len() {
            self.close_tool();
        }
        self.active_native_tools.clear();

        // Agent 模式下的自动恢复判断（硬编码拦截 + LLM 语义判断双保险）
        // 使用 tokio::time::timeout 防止 recovery LLM 调用阻塞 SSE 流
        if self.is_agent_mode && !self.has_tool_use {
            let recovery_result = tokio::time::timeout(
                Duration::from_secs(5),
                recovery::should_recover_async(&self.generated_text, upstream_stop_reason),
            )
            .await;

            match recovery_result {
                Ok(Some(reason)) => {
                    info!("[{}] 🚨 Recovery triggered: {}", self.msg_id, reason);
                    let tool_refs: HashMap<String, &ToolDef> = self
                        .valid_tools
                        .iter()
                        .map(|(k, v)| (k.clone(), v))
                        .collect();
                    if let Some((target_name, target_args)) =
                        recovery::pick_recovery_tool(&tool_refs)
                    {
                        self.send_text_delta("[holoProxy Recovery Injected]");
                        self.close_text();
                        let tool_id = gen_tool_id();
                        self.open_tool(&tool_id, &target_name);
                        let args_str =
                            serde_json::to_string(&target_args).unwrap_or_else(|_| "{}".into());
                        self.send_tool_delta(&args_str);
                        self.close_tool();
                    }
                }
                Ok(None) => {
                    // 正常结束 — 无需恢复
                }
                Err(_elapsed) => {
                    // Recovery 检查超时 — 假设正常结束
                    info!(
                        "[{}] ⏰ Recovery check timed out after 5s, assuming normal completion",
                        self.msg_id
                    );
                }
            }
        }

        let stop_reason = if self.has_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };

        self.send_event(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"output_tokens": 0}
            }),
        );

        self.send_event(
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
    }

    /// 发送错误消息并完成 SSE 流。
    /// Agent 模式下无条件注入 fake tool，防止 Claude Code 报 API Error。
    pub async fn send_error(&mut self, msg: &str) {
        self.flush_text_buffer();
        self.send_text_delta(msg);
        self.close_thinking();
        self.close_text();
        // Agent 模式下必须注入 fake tool，防止 Claude Code 报 API Error
        if self.is_agent_mode && !self.has_tool_use {
            let tool_refs: HashMap<String, &ToolDef> = self
                .valid_tools
                .iter()
                .map(|(k, v)| (k.clone(), v))
                .collect();
            if let Some((target_name, target_args)) =
                recovery::pick_recovery_tool(&tool_refs)
            {
                self.send_text_delta("[holoProxy Recovery Injected]");
                self.close_text();
                let tool_id = gen_tool_id();
                self.open_tool(&tool_id, &target_name);
                let args_str =
                    serde_json::to_string(&target_args).unwrap_or_else(|_| "{}".into());
                self.send_tool_delta(&args_str);
                self.close_tool();
            }
        }
        self.finish("end_turn").await;
    }
}

/// Normalize DSML tags to canonical XML form for trigger detection.
///
/// Handles TWO formats:
///
///   A) ds2api-style DSML wrappers:
///      `<|DSML|tool_calls>` → `<tool_calls>`
///      `<DSML|invoke`       → `<invoke`
///      `〈DSML|parameter〉`   → `<parameter>`
///
///   B) Raw DeepSeek internal DSML format:
///      `<｜tool_calls_begin｜>`            → `<tool_calls>`
///      `<｜tool_call_begin｜>`             → `<invoke`
///      `<｜tool_sep｜>`                    → stripped
///      `<｜tool_call_argument_begin｜>`    → stripped
///      `<｜tool_call_end｜>`               → `</invoke>`
///      `<｜tool_calls_end｜>`              → `</tool_calls>`
///
/// CJK brackets (〈〉), fullwidth punctuation (！｜ etc.) are also normalized.
fn normalize_dsml_tags(text: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;

    // Step 0: Normalize CJK brackets and fullwidth ASCII
    let tmp = text.replace('〈', "<").replace('〉', ">");
    let tmp = normalize_fullwidth_ascii(&tmp);

    // Step 1: Handle ds2api-style wrappers: strip DSML prefix from <|DSML|tag → <tag
    static RE_DSML_WRAPPER: OnceLock<Regex> = OnceLock::new();
    let re_wrapper = RE_DSML_WRAPPER.get_or_init(|| {
        Regex::new(
            r"(?x)
            [<]+
            (?:[|\s！、\u{2581}]*(?:DSML|dsml|DSMARTTOOLCALLS|DSM|dsmarttoolcalls)[|\s！、\u{2581}]*)?
            (/)?
            \s*
            (tool_calls|tool_calls_begin|tool_calls_end|tool_call_begin|tool_call_end|invoke|parameter)
            "
        )
        .unwrap()
    });

    // Step 2: Map raw DeepSeek internal tag names to canonical form
    static RE_DEEPSEEK_INTERNAL: OnceLock<Regex> = OnceLock::new();
    let re_internal = RE_DEEPSEEK_INTERNAL.get_or_init(|| {
        Regex::new(
            r"(?x)
            [<]+
            [|]*
            (/?)
            \s*
            (tool_calls_begin|tool_calls_end|tool_call_begin|tool_call_end|tool_sep|tool_call_argument_begin)
            [|]*
            [>]*
            "
        )
        .unwrap()
    });

    // Step 1: strip DSML prefix
    let result = re_wrapper.replace_all(&tmp, |caps: &regex::Captures| {
        let _slash = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let tag = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        // Map internal names to canonical
        let canonical = match tag {
            "tool_calls_begin" => "tool_calls",
            "tool_calls_end" => "/tool_calls",
            "tool_call_begin" => "invoke",
            "tool_call_end" => "/invoke",
            "tool_sep" => "",
            "tool_call_argument_begin" => "",
            other => other,
        };
        if canonical.is_empty() {
            String::new()
        } else if canonical.starts_with('/') {
            format!("</{}>", &canonical[1..])
        } else {
            format!("<{}", canonical)
        }
    }).to_string();

    // Step 2: handle raw DeepSeek internal tags that survived Step 1
    let result = re_internal.replace_all(&result, |caps: &regex::Captures| {
        let _slash = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let tag = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        match tag {
            "tool_calls_begin" => "<tool_calls>".to_string(),
            "tool_calls_end" => "</tool_calls>".to_string(),
            "tool_call_begin" => "<invoke".to_string(),
            "tool_call_end" => "</invoke>".to_string(),
            "tool_sep" | "tool_call_argument_begin" => String::new(),
            _ => caps.get(0).unwrap().as_str().to_string(),
        }
    }).to_string();

    result
}

/// Normalize fullwidth ASCII characters to their basic ASCII equivalents.
fn normalize_fullwidth_ascii(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '！' => '!',
            '＂' => '"',
            '＃' => '#',
            '＄' => '$',
            '％' => '%',
            '＆' => '&',
            '＼' => '\'',
            '（' => '(',
            '）' => ')',
            '＊' => '*',
            '＋' => '+',
            '，' => ',',
            '－' => '-',
            '．' => '.',
            '／' => '/',
            '：' => ':',
            '；' => ';',
            '＜' => '<',
            '＝' => '=',
            '＞' => '>',
            '？' => '?',
            '＠' => '@',
            '［' => '[',
            '］' => ']',
            '＾' => '^',
            '＿' => '_',
            '｀' => '`',
            '｛' => '{',
            '｜' => '|',
            '｝' => '}',
            '～' => '~',
            _ => c,
        })
        .collect()
}

/// Parse close tag from normalized intercept buffer, map position back to original text.
/// Returns byte index AFTER the close tag in the ORIGINAL (raw) text.
fn find_close_tag_end_in_original(original: &str, norm_canonical: &str, close_tag: &str) -> Option<usize> {
    let norm_pos = norm_canonical.find(close_tag)?;
    let norm_end = norm_pos + close_tag.len();

    // Walk through original and normalized in parallel to map the position
    let orig_chars: Vec<(usize, char)> = original.char_indices().collect();
    let norm_chars: Vec<char> = norm_canonical.chars().collect();

    let mut oi = 0usize; // index into orig_chars
    let mut ni = 0usize; // index into norm_chars

    while ni < norm_end && oi < orig_chars.len() {
        let (_, oc) = orig_chars[oi];
        if ni < norm_chars.len() {
            let nc = norm_chars[ni];
            if normalize_fullwidth_ascii(&oc.to_string()) == nc.to_string() {
                oi += 1;
                ni += 1;
            } else {
                // Original char is part of DSML prefix/noise that gets normalized away
                oi += 1;
            }
        } else {
            break;
        }
    }

    if oi <= orig_chars.len() {
        Some(orig_chars[..oi].iter().map(|(_, c)| c.len_utf8()).sum())
    } else {
        None
    }
}

pub fn gen_tool_id() -> String {
    format!("toolu_{}", Uuid::new_v4().to_string().replace('-', "")[..24].to_string())
}

/// 解析 fallback XML/JSON 工具调用（公开，供 OpenAI 透传路径复用）
pub fn parse_fallback_tool(
    text: &str,
    valid_tools: &HashMap<String, ToolDef>,
) -> (String, serde_json::Value) {
    // Normalize: DeepSeek ▁(U+2581) → _(U+005F), subsequent parsing uses _ variant
    let normalized = text.replace('▁', "_");
    let text = normalized.as_str();

    // Raw DeepSeek DSML format (fullwidth pipes ｜ U+FF5C):
    // <｜tool_call_begin｜>function<｜tool_sep｜>name<｜tool_call_argument_begin｜>{json}<｜tool_call_end｜>
    const DSML_SEP: &str = "<｜tool_sep｜>";
    const DSML_ARG_BEGIN: &str = "<｜tool_call_argument_begin｜>";
    const DSML_CALL_END: &str = "<｜tool_call_end｜>";
    if let Some(sep_pos) = text.find(DSML_SEP) {
        let after_sep = &text[sep_pos + DSML_SEP.len()..];
        if let Some(arg_pos) = after_sep.find(DSML_ARG_BEGIN) {
            let name = after_sep[..arg_pos].trim().to_string();
            let args_start = arg_pos + DSML_ARG_BEGIN.len();
            let after_args = &after_sep[args_start..];
            let args_end = after_args.find(DSML_CALL_END).unwrap_or(after_args.len());
            let args_str = after_args[..args_end].trim();
            if !name.is_empty() {
                // 大小写不敏感匹配工具名，返回精确 key
                let matched_name = valid_tools
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(&name))
                    .cloned()
                    .unwrap_or(name);
                // 优先尝试直接解析 JSON
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
                    return (matched_name, args);
                }
                // 兜底：提取平衡括号内的 JSON（处理 markdown 包裹等情况）
                if let Some(start) = args_str.find('{') {
                    let mut depth = 0i32;
                    let mut end = start;
                    for (i, c) in args_str[start..].char_indices() {
                        match c {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = start + i + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if end > start {
                        if let Ok(args) =
                            serde_json::from_str::<serde_json::Value>(&args_str[start..end])
                        {
                            return (matched_name, args);
                        }
                    }
                }
                return (matched_name, serde_json::json!({}));
            }
        }
    }

    // invoke XML attribute format: <invoke name="Bash"><parameter name="command">dir</parameter></invoke>
    let lower_text = text.to_lowercase();
    if let Some(invoke_start) = lower_text.find("<invoke") {
        // 取 invoke 的 name="..." 属性
        if let Some(name_start) = text[invoke_start..].find("name=\"") {
            let name_val_start = invoke_start + name_start + 6;
            if let Some(name_end) = text[name_val_start..].find('"') {
                let t_name = text[name_val_start..name_val_start + name_end].trim().to_string();
                // 查找 invoke 结束标签，限定搜索范围
                let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {
                    invoke_start + end_pos
                } else if let Some(end_pos) = text[invoke_start..].find("</｜invoke｜>") {
                    invoke_start + end_pos
                } else {
                    text.len()
                };
                let invoke_body = &text[invoke_start..invoke_end];

                // 提取所有 <parameter name="X">V</parameter> 或 <parameter name="X" ...>V</parameter>
                let mut args_map = serde_json::Map::new();
                let mut search_from = 0usize;
                while let Some(p_start) = invoke_body[search_from..].to_lowercase().find("<parameter") {
                    let abs_p = search_from + p_start;
                    if let Some(p_name_begin) = invoke_body[abs_p..].find("name=\"") {
                        let p_name_s = abs_p + p_name_begin + 6;
                        if let Some(p_name_len) = invoke_body[p_name_s..].find('"') {
                            let p_name = invoke_body[p_name_s..p_name_s + p_name_len].trim().to_string();
                            // 找到 > 闭合 opening tag
                            if let Some(gt_pos) = invoke_body[p_name_s + p_name_len..].find('>') {
                                let content_start = p_name_s + p_name_len + gt_pos + 1;
                                let p_val = if let Some(close_p) = invoke_body[content_start..].to_lowercase().find("</parameter>") {
                                    invoke_body[content_start..content_start + close_p].trim().to_string()
                                } else {
                                    invoke_body[content_start..].trim().to_string()
                                };
                                let p_val_clean = p_val.trim().to_string();
                                if !p_name.is_empty() && !p_val_clean.is_empty() {
                                    args_map.insert(p_name, serde_json::json!(p_val_clean));
                                }
                                search_from = content_start + p_val.len();
                            } else {
                                search_from = abs_p + 10; // skip broken tag
                            }
                        } else {
                            search_from = abs_p + 10;
                        }
                    } else {
                        search_from = abs_p + 10;
                    }
                    if search_from >= invoke_body.len() { break; }
                }

                if !t_name.is_empty() && !args_map.is_empty() {
                    return (t_name, serde_json::Value::Object(args_map));
                }
            }
        }
    }

    // DSML 格式: <tool_name>Name</tool_name><tool_arguments>{"arg":"val"}</tool_arguments>
    let ds_name = Regex::new(r"(?i)<[｜|]?(?:DSML[｜|]?)?tool_name[｜|]?>\s*([\s\S]*?)\s*(?:</[｜|]|$)")
        .ok();
    let ds_args = Regex::new(
        r"(?i)<[｜|]?(?:DSML[｜|]?)?(?:tool_arguments|parameter)[｜|]?>\s*([\s\S]*?)\s*(?:</[｜|]|$)",
    )
    .ok();

    if let (Some(name_re), Some(args_re)) = (&ds_name, &ds_args) {
        if let (Some(n), Some(a)) = (name_re.captures(text), args_re.captures(text)) {
            let name = n.get(1).unwrap().as_str().trim().to_string();
            let args_str = a.get(1).unwrap().as_str().trim().to_string();
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_str) {
                return (name, args);
            }
        }
    }

    // JSON 块: {"name": "...", "arguments": {...}}
    // 使用平衡括号匹配提取完整 JSON（处理嵌套 {}）
    if let Some(start) = text.find('{') {
        let mut depth = 0;
        let mut end = start;
        for (i, c) in text[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > start {
            let json_str = &text[start..end];
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let (Some(name), Some(args)) = (
                    data.get("name").and_then(|v| v.as_str()),
                    data.get("arguments"),
                ) {
                    return (name.to_string(), args.clone());
                }
            }
        }
    }

    // 按工具名匹配: <tool_name>content</tool_name>
    for (t_name, t_info) in valid_tools.iter() {
        let lower = t_name.to_lowercase();
        // 尝试 XML 标签
        if let Some(re) = Regex::new(&format!(
            r"(?i)<{}[^>]*>(.*?)(?:</{}>|$)",
            regex::escape(&lower),
            regex::escape(&lower)
        ))
        .ok()
        {
            if let Some(caps) = re.captures(text) {
                let inner = caps.get(1).unwrap().as_str().trim().to_string();
                let props = t_info
                    .input_schema
                    .as_ref()
                    .and_then(|s| s.get("properties"))
                    .and_then(|p| p.as_object());
                if let Some(props_map) = props {
                    if props_map.len() == 1 {
                        let key = props_map.keys().next().unwrap().clone();
                        return (t_name.clone(), serde_json::json!({key: inner}));
                    }
                    if props_map.contains_key("command") {
                        return (t_name.clone(), serde_json::json!({"command": inner}));
                    }
                }
            }
        }
    }

    ("unknown".into(), serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fallback_tool_json_block() {
        let valid_tools = std::collections::HashMap::new();
        let input = r#"{"name": "Bash", "arguments": {"command": "ls"}}"#;
        let (name, args) = parse_fallback_tool(input, &valid_tools);
        assert_eq!(name, "Bash");
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "ls");
    }

    #[test]
    fn test_parse_fallback_tool_xml() {
        let valid_tools = std::collections::HashMap::from([(
            "Bash".into(),
            ToolDef {
                name: "Bash".into(),
                description: Some("Run commands".into()),
                input_schema: Some(serde_json::json!({
                    "properties": {"command": {"type": "string"}}
                })),
            },
        )]);
        let input = "<tool_call>\n{\"name\": \"Bash\", \"arguments\": {\"command\": \"ls -la\"}}\n</tool_call>";
        let (name, _args) = parse_fallback_tool(input, &valid_tools);
        // JSON block inside XML should match
        assert_eq!(name, "Bash");
    }

    #[test]
    fn test_parse_fallback_tool_dsml() {
        let valid_tools = std::collections::HashMap::from([(
            "Bash".into(),
            ToolDef {
                name: "Bash".into(),
                description: Some("Run commands".into()),
                input_schema: Some(serde_json::json!({
                    "properties": {"command": {"type": "string"}}
                })),
            },
        )]);
        // 使用 ▁(U+2581) 变体 — 模型实际输出格式
        let input = "<｜tool▁call▁begin｜>function<｜tool▁sep｜>Bash<｜tool▁call▁argument▁begin｜>{\"command\":\"ls -la\"}<｜tool▁call▁end｜>";
        let (name, args) = parse_fallback_tool(input, &valid_tools);
        assert_eq!(name, "Bash");
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "ls -la");
    }

    #[test]
    fn test_parse_fallback_tool_dsml_case_insensitive() {
        let valid_tools = std::collections::HashMap::from([(
            "Bash".into(),
            ToolDef {
                name: "Bash".into(),
                description: Some("Run commands".into()),
                input_schema: Some(serde_json::json!({
                    "properties": {"command": {"type": "string"}}
                })),
            },
        )]);
        // 工具名小写
        let input = "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash<｜tool▁call▁argument▁begin｜>{\"command\":\"ls\"}<｜tool▁call▁end｜>";
        let (name, _args) = parse_fallback_tool(input, &valid_tools);
        assert_eq!(name, "Bash"); // 应返回 valid_tools 中的精确 key
    }

    #[test]
    fn test_gen_tool_id() {
        let id = gen_tool_id();
        assert!(id.starts_with("toolu_"));
        assert_eq!(id.len(), 30); // toolu_ + 24 hex chars
    }
}
