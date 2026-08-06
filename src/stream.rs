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
	                let preview_len = full_xml.len().min(100);
	                let preview_len = full_xml.floor_char_boundary(preview_len);
	                warn!(
	                    "⚠️ [XML Parse] finalize_intercept 拦截到无效工具标签，跳过: {}",
	                    &full_xml[..preview_len]
	                );
	            }
	            self.intercept_active = false;
	            if !remaining.is_empty() {
	                self.text_buffer = remaining;
	                self.check_text_buffer_triggers();
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
	            self.intercept_buffer.push_str(content);
	            let norm_buf = normalize_dsml_tags(&self.intercept_buffer);
	            if norm_buf.contains(&self.active_close_tag) {
	                let orig_end = find_close_tag_end_in_original(
	                    &self.intercept_buffer,
	                    &norm_buf,
	                    &self.active_close_tag,
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
	                    let preview_len = full_xml.len().min(100);
	                    let preview_len = full_xml.floor_char_boundary(preview_len);
	                    warn!(
	                        "⚠️ [XML Parse] 拦截到无效的工具标签格式，跳过: {}",
	                        &full_xml[..preview_len]
	                    );
	                }
	                self.intercept_active = false;
	                self.intercept_buffer.clear();
	                if !remaining.is_empty() {
	                    self.text_buffer = remaining;
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
	            if idx > 0 {
	                let pre_text = normalized[..idx].to_string();
	                self.send_text_delta(&pre_text);
	            }
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
	    fn find_original_tag_pos(
	        &self,
	        _normalized: &str,
	        norm_idx: usize,
	        open_tag_base: &str,
	    ) -> usize {
	        let estimated = norm_idx.min(self.text_buffer.len());
	        let search_start = self.text_buffer.floor_char_boundary(estimated.saturating_sub(30));
	        let search_slice = &self.text_buffer[search_start..];
	        if let Some(pos) = search_slice.find('<') {
	            let orig_pos = search_start + pos;
	            let test = normalize_dsml_tags(&self.text_buffer[orig_pos..]);
	            let expected = format!("<{}", open_tag_base);
	            if test.starts_with(&expected) {
	                return orig_pos;
	            }
	        }
	        self.text_buffer.floor_char_boundary(estimated)
	    }
	    /// 处理原生 tool_calls delta
	    pub fn handle_tool_call(&mut self, tc: &OpenAIToolCallDelta) {
	        self.finalize_intercept();
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
	            self.active_native_tools.insert(idx, self.block_idx);
	        }
	        if let Some(ref func) = tc.function {
	            if let Some(ref args) = func.arguments {
	                if !args.is_empty() {
	                    let target_block =
	                        self.active_native_tools.get(&idx).copied().unwrap_or(self.block_idx);
	                    self.send_event(
	                        "content_block_delta",
	                        &serde_json::json!({
	                            "type": "content_block_delta",
	                            "index": target_block,
	                            "delta": {"type": "input_json_delta", "partial_json": args}
	                        }),
	                    );
	                }
	            }
	        }
	    }
	    /// 处理 reasoning / reasoning_content — 输出为独立 thinking content_block
	    pub fn handle_reasoning(&mut self, text: &str) {
	        if !self.thinking_open {
	            self.finalize_intercept();
	            self.flush_text_buffer();
	            info!("[{}] 💭 reasoning block START", self.msg_id);
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
	    pub async fn finish(&mut self, upstream_stop_reason: &str) {
	        if !self.text_open && !self.thinking_open && !self.has_tool_use {
	            self.send_text_delta(" ");
	        }
	        self.flush_text_buffer();
	        self.finalize_intercept();
	        self.flush_text_buffer();
	        self.close_thinking();
	        self.close_text();
	        self.close_tool();
	        for _ in 0..self.active_native_tools.len() {
	            self.close_tool();
	        }
	        self.active_native_tools.clear();
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
	                Ok(None) => {}
	                Err(_elapsed) => {
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
	        self.send_event("message_stop", &serde_json::json!({"type": "message_stop"}));
	    }
	    /// 发送错误消息并完成 SSE 流。
	    pub async fn send_error(&mut self, msg: &str) {
	        self.flush_text_buffer();
	        self.send_text_delta(msg);
	        self.close_thinking();
	        self.close_text();
	        if self.is_agent_mode && !self.has_tool_use {
	            let tool_refs: HashMap<String, &ToolDef> = self
	                .valid_tools
	                .iter()
	                .map(|(k, v)| (k.clone(), v))
	                .collect();
	            if let Some((target_name, target_args)) = recovery::pick_recovery_tool(&tool_refs) {
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
	fn normalize_dsml_tags(text: &str) -> String {
	    use std::sync::OnceLock;
	    let tmp = text.replace('〈', "<").replace('〉', ">");
	    let tmp = normalize_fullwidth_ascii(&tmp);
	    static RE_DSML_WRAPPER: OnceLock<Regex> = OnceLock::new();
	    let re_wrapper = RE_DSML_WRAPPER.get_or_init(|| {
	        Regex::new(
	            r"(?x)
	            [<]+
	            (?:[|\s！、\u{2581}]*(?:DSML|dsml|DSMARTTOOLCALLS|DSM|dsmarttoolcalls)[|\s！、\u{2581}]*)?
	            (/)?
	            \s*
	            (tool_calls|tool_calls_begin|tool_calls_end|tool_call_begin|tool_call_end|invoke|parameter)
	            ",
	        )
	        .unwrap()
	    });
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
	            ",
	        )
	        .unwrap()
	    });
	    let result = re_wrapper
	        .replace_all(&tmp, |caps: &regex::Captures| {
	            let tag = caps.get(2).map(|m| m.as_str()).unwrap_or("");
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
	        })
	        .to_string();
	    let result = re_internal
	        .replace_all(&result, |caps: &regex::Captures| {
	            let tag = caps.get(2).map(|m| m.as_str()).unwrap_or("");
	            match tag {
	                "tool_calls_begin" => "<tool_calls>".to_string(),
	                "tool_calls_end" => "</tool_calls>".to_string(),
	                "tool_call_begin" => "<invoke".to_string(),
	                "tool_call_end" => "</invoke>".to_string(),
	                "tool_sep" | "tool_call_argument_begin" => String::new(),
	                _ => caps.get(0).unwrap().as_str().to_string(),
	            }
	        })
	        .to_string();
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
	            '＼' => '\\',
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
	fn find_close_tag_end_in_original(
	    original: &str,
	    norm_canonical: &str,
	    close_tag: &str,
	) -> Option<usize> {
	    let norm_pos = norm_canonical.find(close_tag)?;
	    let norm_end_bytes = norm_pos + close_tag.len();
	    let norm_end = norm_canonical[..norm_end_bytes].chars().count();
	    let orig_chars: Vec<(usize, char)> = original.char_indices().collect();
	    let norm_chars: Vec<char> = norm_canonical.chars().collect();
	    let mut oi = 0usize;
	    let mut ni = 0usize;
	    while ni < norm_end && oi < orig_chars.len() {
	        let (_, oc) = orig_chars[oi];
	        if ni < norm_chars.len() {
	            let nc = norm_chars[ni];
	            if normalize_fullwidth_ascii(&oc.to_string()) == nc.to_string() {
	                oi += 1;
	                ni += 1;
	            } else {
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
	    format!(
	        "toolu_{}",
	        &Uuid::new_v4().to_string().replace('-', "")[..24]
	    )
	}
	// ============================================================
	// DSML 专用预处理 + quick-xml 解析（新增）
	// ============================================================
	/// DSML 专用预处理：
	/// 1. 全角字符转半角
	/// 2. 去除标签名中的 DSML 前缀，规范化为标准 XML 标签
	/// 3. 转义 <parameter> 内部的 <、&、> 使其成为合法 XML
	fn preprocess_dsml_xml(input: &str) -> String {
	    use std::sync::OnceLock;
	    // Step 1: 全角 → 半角
	    let tmp = normalize_fullwidth_ascii(input);
	    // Step 2: 规范化标签名 — 去除 DSML 前缀
	    static RE_TAG: OnceLock<Regex> = OnceLock::new();
	    let re_tag = RE_TAG.get_or_init(|| {
	        Regex::new(
	            r"<(/?)\s*\|?(?:DSML\|?)?\s*(tool_calls|invoke|parameter)((?:\s+[^>]*)?)\|?\s*>",
	        )
	        .unwrap()
	    });
	    let tmp = re_tag.replace_all(&tmp, "<$1$2$3>").to_string();
	    // Step 3: 转义 <parameter> 内部文本中的 XML 特殊字符
	    escape_parameter_text(&tmp)
	}
	/// 转义 <parameter>...</parameter> 内部的 <、&、>
	/// 使包含代码片段的文本内容能被标准 XML 解析器正确处理
	fn escape_parameter_text(input: &str) -> String {
	    use std::sync::OnceLock;
	    static RE: OnceLock<Regex> = OnceLock::new();
	    let re = RE.get_or_init(|| {
	        Regex::new(r"(?s)(<parameter\b[^>]*>)(.*?)(</parameter\s*>)").unwrap()
	    });
	    re.replace_all(input, |caps: &regex::Captures| {
	        let open_tag = &caps[1];
	        let body = &caps[2];
	        let close_tag = &caps[3];
	        let escaped = body
	            .replace('&', "&amp;")
	            .replace('<', "&lt;")
	            .replace('>', "&gt;");
	        format!("{open_tag}{escaped}{close_tag}")
	    })
	    .into_owned()
	}
	/// 使用 quick-xml 解析 invoke XML 格式
	/// 输入应已经过 preprocess_dsml_xml 预处理
	fn parse_invoke_with_quick_xml(
	    xml: &str,
	    valid_tools: &HashMap<String, ToolDef>,
	) -> Option<(String, serde_json::Value)> {
	    use quick_xml::events::Event;
	    use quick_xml::Reader;
	    let mut reader = Reader::from_str(xml);
	    let mut tool_name: Option<String> = None;
	    let mut args_map = serde_json::Map::new();
	    let mut current_param_name: Option<String> = None;
	    let mut current_param_value = String::new();
	    let mut in_invoke = false;
	    let mut found_invoke = false;
	    let mut buf = Vec::new();
	    loop {
	        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref());
	                match tag.as_ref() {
	                    "invoke" => {
	                        in_invoke = true;
	                        found_invoke = true;
	                        for attr in e.attributes().flatten() {
	                            if attr.key.as_ref() == b"name" {
                                tool_name = Some(
                                    String::from_utf8_lossy(&attr.value)
                                        .trim()
                                        .to_string(),
                                );
	                            }
	                        }
	                    }
	                    "parameter" if in_invoke => {
	                        current_param_value.clear();
	                        current_param_name = None;
	                        for attr in e.attributes().flatten() {
	                            if attr.key.as_ref() == b"name" {
                                current_param_name = Some(
                                    String::from_utf8_lossy(&attr.value)
                                        .trim()
                                        .to_string(),
                                );
	                            }
	                        }
	                    }
	                    _ => {}
	                }
	            }
	            Ok(Event::Text(t)) => {
	                if current_param_name.is_some() {
	                    match t.unescape() {
	                        Ok(unescaped) => current_param_value.push_str(&unescaped),
	                        Err(_) => {
	                            current_param_value
	                                .push_str(&String::from_utf8_lossy(t.as_ref()));
	                        }
	                    }
	                }
	            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref());
	                match tag.as_ref() {
	                    "parameter" if in_invoke => {
	                        if let Some(pname) = current_param_name.take() {
	                            let val = current_param_value.trim().to_string();
	                            if !pname.is_empty() && !val.is_empty() {
	                                args_map.insert(pname, serde_json::json!(val));
	                            }
	                        }
	                    }
	                    "invoke" => break,
	                    _ => {}
	                }
	            }
	            Ok(Event::Eof) => break,
	            Err(_) => return None,
	            _ => {}
	        }
	        buf.clear();
	    }
	    if found_invoke {
	        if let Some(name) = tool_name {
	            let matched = valid_tools
	                .keys()
	                .find(|k| k.eq_ignore_ascii_case(&name))
	                .cloned()
	                .unwrap_or(name);
	            return Some((matched, serde_json::Value::Object(args_map)));
	        }
	    }
	    None
	}
	// ============================================================
	// parse_fallback_tool（invoke XML 部分改用 quick-xml）
	// ============================================================
	/// 解析 fallback XML/JSON 工具调用（公开，供 OpenAI 透传路径复用）
	fn parse_fallback_tool(
	    text: &str,
	    valid_tools: &HashMap<String, ToolDef>,
	) -> (String, serde_json::Value) {
    // Normalize: DeepSeek ▁(U+2581) → _(U+005F), subsequent parsing uses _ variant
    let normalized = text.replace('▁', "_");
	    let text = normalized.as_str();
	    // Raw DeepSeek DSML format (fullwidth pipes ｜ U+FF5C):
	    // <｜tool_call_begin｜>function<｜tool_sep｜>name<｜tool_call_argument_begin｜>{json}<｜tool_call_end｜>
	    const DSML_SEP: &str = "<|tool_sep|>";
	    const DSML_ARG_BEGIN: &str = "<|tool_call_argument_begin|>";
	    const DSML_CALL_END: &str = "<|tool_call_end|>";
	    // 先做全角转半角以便匹配 DSML 内部格式
	    let text_half = normalize_fullwidth_ascii(text);
	    if let Some(sep_pos) = text_half.find(DSML_SEP) {
	        let after_sep = &text_half[sep_pos + DSML_SEP.len()..];
	        if let Some(arg_pos) = after_sep.find(DSML_ARG_BEGIN) {
	            let name = after_sep[..arg_pos].trim().to_string();
	            let args_start = arg_pos + DSML_ARG_BEGIN.len();
	            let after_args = &after_sep[args_start..];
	            let args_end = after_args.find(DSML_CALL_END).unwrap_or(after_args.len());
	            let args_str = after_args[..args_end].trim();
	            if !name.is_empty() {
	                let matched_name = valid_tools
	                    .keys()
	                    .find(|k| k.eq_ignore_ascii_case(&name))
	                    .cloned()
	                    .unwrap_or(name);
	                if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
	                    return (matched_name, args);
	                }
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
	    // ============================================================
	    // invoke XML: 预处理 + quick-xml 解析（替换原有手动解析）
	    // ============================================================
	    {
	        let preprocessed = preprocess_dsml_xml(text);
	        if let Some(result) = parse_invoke_with_quick_xml(&preprocessed, valid_tools) {
	            return result;
	        }
	    }
	    // DSML 格式: <tool_name>Name</tool_name><tool_arguments>{"arg":"val"}</tool_arguments>
	    let ds_name =
	        Regex::new(r"(?i)<[|]?(?:DSML[|]?)?tool_name[|]?>\s*([\s\S]*?)\s*(?:</[|]|$)").ok();
	    let ds_args = Regex::new(
	        r"(?i)<[|]?(?:DSML[|]?)?(?:tool_arguments|parameter)[|]?>\s*([\s\S]*?)\s*(?:</[|]|$)",
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
	        let input = "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash<｜tool▁call▁argument▁begin｜>{\"command\":\"ls\"}<｜tool▁call▁end｜>";
	        let (name, _args) = parse_fallback_tool(input, &valid_tools);
	        assert_eq!(name, "Bash");
	    }
	    #[test]
	    fn test_gen_tool_id() {
	        let id = gen_tool_id();
	        assert!(id.starts_with("toolu_"));
	        assert_eq!(id.len(), 30);
	    }
	    #[test]
	    fn test_parse_invoke_with_quick_xml_basic() {
	        let valid_tools = std::collections::HashMap::from([(
	            "edit".into(),
	            ToolDef {
	                name: "edit".into(),
	                description: Some("Edit file".into()),
	                input_schema: Some(serde_json::json!({
	                    "properties": {
	                        "filePath": {"type": "string"},
	                        "newString": {"type": "string"}
	                    }
	                })),
	            },
	        )]);
	        let input = r#"<｜DSML｜tool_calls><｜DSML｜invoke name="edit"><｜DSML｜parameter name="filePath" string="true">F:\test.py</｜DSML｜parameter><｜DSML｜parameter name="newString" string="true">if x < 10: print(x)</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"#;
	        let (name, args) = parse_fallback_tool(input, &valid_tools);
	        assert_eq!(name, "edit");
	        assert_eq!(
	            args.get("filePath").unwrap().as_str().unwrap(),
	            "F:\\test.py"
	        );
	        assert_eq!(
	            args.get("newString").unwrap().as_str().unwrap(),
	            "if x < 10: print(x)"
	        );
	    }
	    #[test]
	    fn test_preprocess_dsml_xml() {
	        let input = r#"<｜DSML｜invoke name="edit"><｜DSML｜parameter name="code" string="true">if x < 10 && y > 5: pass</｜DSML｜parameter></｜DSML｜invoke>"#;
	        let result = preprocess_dsml_xml(input);
	        assert!(result.contains("<invoke name=\"edit\">"));
	        assert!(result.contains("if x &lt; 10 &amp;&amp; y &gt; 5: pass"));
	        assert!(result.contains("</parameter>"));
	        assert!(result.contains("</invoke>"));
	    }
	}