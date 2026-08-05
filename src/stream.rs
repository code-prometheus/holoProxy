	use crate::recovery;
	use crate::types::*;
	use bytes::Bytes;
	use regex::Regex;
	use std::collections::HashMap;
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
	    // 输出缓冲区
	    output: Vec<Bytes>,
	}
	fn floor_char_boundary(s: &str, mut index: usize) -> usize {
	    if index >= s.len() {
	        s.len()
	    } else {
	        while !s.is_char_boundary(index) {
	            index -= 1;
	        }
	        index
	    }
	}
	impl StreamContext {
	    pub fn new(
	        msg_id: String,
	        model_name: String,
	        is_agent_mode: bool,
	        valid_tools: HashMap<String, ToolDef>,
	    ) -> Self {
	        let mut valid_triggers: HashMap<String, String> = HashMap::new();
	        valid_triggers.insert("<tool_call>".into(), "</tool_call>".into());
	        valid_triggers.insert("```json".into(), "```".into());
	        valid_triggers.insert("```tool_call".into(), "```".into());
	        valid_triggers.insert("<｜tool_calls｜>".into(), "</｜tool_calls｜>".into());
	        valid_triggers.insert("<｜tool_call｜>".into(), "</｜tool_call｜>".into());
	        valid_triggers.insert("<ツtool_callsツ>".into(), "</ツtool_callsツ>".into());
	        valid_triggers.insert("<ツtool_callツ>".into(), "</ツtool_callツ>".into());
	        // invoke/parameter XML 格式
	        valid_triggers.insert("<invoke".into(), "</invoke>".into());
	        valid_triggers.insert("<｜invoke｜".into(), "</｜invoke｜>".into());
	        // DeepSeek DSML 格式 ▁(U+2581) 变体 — 模型实际输出格式
	        valid_triggers.insert("<｜tool▁calls▁begin｜>".into(), "<｜tool▁calls▁end｜>".into());
	        valid_triggers.insert("<｜tool▁call▁begin｜>".into(), "<｜tool▁call▁end｜>".into());
	        // _(U+005F) 变体 — 部分上游可能做归一化
	        valid_triggers.insert("<｜tool_calls_begin｜>".into(), "<｜tool_calls_end｜>".into());
	        valid_triggers.insert("<｜tool_call_begin｜>".into(), "<｜tool_call_end｜>".into());
	        // DSML 包装闭合标签 — 注册为自闭合触发，解析返回空自动跳过
	        valid_triggers.insert("<｜tool▁calls▁end｜>".into(), "<｜tool▁calls▁end｜>".into());
	        valid_triggers.insert("<｜tool_calls_end｜>".into(), "<｜tool_calls_end｜>".into());
	        // 为每个有效工具名添加触发标签
	        for name in valid_tools.keys() {
	            let lower = name.to_lowercase();
	            valid_triggers.insert(format!("<{}>", lower), format!("</{}>", lower));
	            valid_triggers.insert(format!("<{}>", name), format!("</{}>", name));
	            valid_triggers.insert(format!("```{}", lower), "```".into());
	        }
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
	            output: Vec::new(),
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
	    fn send_event(&mut self, event_type: &str, data: &serde_json::Value) {
	        let payload = format!(
	            "event: {}\ndata: {}\n\n",
	            event_type,
	            serde_json::to_string(data).unwrap_or_default()
	        );
	        self.output.push(Bytes::from(payload));
	    }
	    pub fn take_output(&mut self) -> Vec<Bytes> {
	        std::mem::take(&mut self.output)
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
	            // 尝试找到闭合标签；找不到则处理整个缓冲
	            let (full_xml, remaining) = if let Some(close_pos) =
	                buffer.find(&self.active_close_tag)
	            {
	                let end = close_pos + self.active_close_tag.len();
	                (buffer[..end].to_string(), buffer[end..].to_string())
	            } else {
	                (buffer, String::new())
	            };
	            let tools = parse_fallback_tools(&full_xml, &self.valid_tools);
	            let mut has_valid_tool = false;
	            for (tool_name, tool_args) in tools {
	                if self.valid_tools.contains_key(&tool_name) {
	                    has_valid_tool = true;
	                    let tool_id = gen_tool_id();
	                    self.open_tool(&tool_id, &tool_name);
	                    let args_str =
	                        serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
	                    self.send_tool_delta(&args_str);
	                    self.close_tool();
	                } else {
	                    warn!(
	                        "⚠️ [XML Parse] finalize_intercept 拦截到无效工具标签，跳过: {}",
	                        &tool_name
	                    );
	                }
	            }
	            if !has_valid_tool {
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
	            // 拦截模式：收集到 active_close_tag 为止
	            self.intercept_buffer.push_str(content);
	            if self.intercept_buffer.contains(&self.active_close_tag) {
	                let close_idx =
	                    self.intercept_buffer.find(&self.active_close_tag).unwrap()
	                        + self.active_close_tag.len();
	                let full_xml = self.intercept_buffer[..close_idx].to_string();
	                let remaining = self.intercept_buffer[close_idx..].to_string();
	                let tools = parse_fallback_tools(&full_xml, &self.valid_tools);
	                let mut has_valid_tool = false;
	                for (tool_name, tool_args) in tools {
	                    if self.valid_tools.contains_key(&tool_name) {
	                        has_valid_tool = true;
	                        let tool_id = gen_tool_id();
	                        self.open_tool(&tool_id, &tool_name);
	                        let args_str =
	                            serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
	                        self.send_tool_delta(&args_str);
	                        self.close_tool();
	                    } else {
	                        warn!(
	                            "⚠️ [XML Parse] 拦截到无效的工具标签格式，跳过: {}",
	                            &tool_name
	                        );
	                    }
	                }
	                if !has_valid_tool {
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
	    fn find_max_partial_tag_len(&self, text: &str) -> usize {
	        let mut max_match_len = 0;
	        for open_tag in self.valid_triggers.keys() {
	            let tag_bytes = open_tag.as_bytes();
	            let text_bytes = text.as_bytes();
	            let max_possible = std::cmp::min(text_bytes.len(), tag_bytes.len());
	            if max_possible == 0 { continue; }
	            for len in (1..=max_possible).rev() {
	                if &text_bytes[text_bytes.len() - len..] == &tag_bytes[..len] {
	                    if len > max_match_len {
	                        max_match_len = len;
	                    }
	                    break;
	                }
	            }
	        }
	        max_match_len
	    }
	    fn check_text_buffer_triggers(&mut self) {
	        if self.intercept_active {
	            return;
	        }
	        // 找到最早出现的触发标签
	        let mut earliest_idx: Option<usize> = None;
	        let mut matched_open_tag: Option<&str> = None;
	        for (open_tag, _close_tag) in &self.valid_triggers {
	            if let Some(idx) = self.text_buffer.find(open_tag.as_str()) {
	                if earliest_idx.is_none() || idx < earliest_idx.unwrap() {
	                    earliest_idx = Some(idx);
	                    matched_open_tag = Some(open_tag);
	                }
	            }
	        }
	        if let (Some(idx), Some(open_tag)) = (earliest_idx, matched_open_tag) {
	            let close_tag = self.valid_triggers.get(open_tag).cloned().unwrap_or_default();
	            // 空闭合标签守卫：防止 contains("") 恒为 true
	            if close_tag.is_empty() {
	                return;
	            }
	            // 发送 open_tag 之前的文本
	            if idx > 0 {
	                let pre_text = self.text_buffer[..idx].to_string();
	                self.send_text_delta(&pre_text);
	            }
	            self.intercept_active = true;
	            self.active_close_tag = close_tag;
	            self.intercept_buffer = self.text_buffer[idx..].to_string();
	            self.text_buffer.clear();
	        } else {
	            // 没有找到完整的触发标签，检查尾部是否是某个触发标签的前缀
	            let max_match_len = self.find_max_partial_tag_len(&self.text_buffer);
	            if self.text_buffer.len() > max_match_len {
	                let safe_cut = self.text_buffer.len() - max_match_len;
	                let send_len = floor_char_boundary(&self.text_buffer, safe_cut);
	                if send_len > 0 {
	                    let send_text = self.text_buffer[..send_len].to_string();
	                    self.send_text_delta(&send_text);
	                    self.text_buffer = self.text_buffer[send_len..].to_string();
	                }
	            }
	        }
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
	    /// 结束流：关闭所有开放块 + 自动恢复判断 + 发送 message_delta/message_stop
	    pub fn finish(&mut self, upstream_stop_reason: &str) {
	        // 如果拦截模式未关闭，处理剩余拦截缓冲
	        self.finalize_intercept();
	        // 处理剩余的 text_buffer，以防有未闭合的残留 DSML 被当作普通文本输出
	        if !self.text_buffer.is_empty() {
	            let buffer = std::mem::take(&mut self.text_buffer);
	            if buffer.contains("<｜tool") || buffer.contains("<tool") || buffer.contains("```json") || buffer.contains("<invoke") {
	                let tools = parse_fallback_tools(&buffer, &self.valid_tools);
	                let mut has_valid_tool = false;
	                for (tool_name, tool_args) in tools {
	                    if self.valid_tools.contains_key(&tool_name) {
	                        has_valid_tool = true;
	                        let tool_id = gen_tool_id();
	                        self.open_tool(&tool_id, &tool_name);
	                        let args_str = serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
	                        self.send_tool_delta(&args_str);
	                        self.close_tool();
	                    }
	                }
	                if !has_valid_tool {
	                    self.send_text_delta(&buffer);
	                }
	            } else {
	                self.send_text_delta(&buffer);
	            }
	        }
	        self.close_thinking();
	        self.close_text();
	        self.close_tool();
	        // 关闭所有原生 tool_calls
	        for _ in 0..self.active_native_tools.len() {
	            self.close_tool();
	        }
	        self.active_native_tools.clear();
	        // 保底：防止空响应（只考虑 text/tool/thinking）
	        if !self.text_open && !self.thinking_open && !self.has_tool_use {
	            self.send_text_delta(" ");
	        }
	        // Agent 模式下的自动恢复判断（硬编码拦截 + LLM 语义判断双保险）
	        if self.is_agent_mode && !self.has_tool_use {
	            if let Some(_reason) = recovery::should_recover(&self.generated_text, upstream_stop_reason)
	            {
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
	    /// 发送错误消息并完成 SSE 流
	    pub fn send_error(&mut self, msg: &str) {
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
	        self.finish("end_turn");
	    }
	}
	/// 兼容包装：parse_fallback_tools → 取首个结果（server.rs 调用兼容）
pub fn parse_fallback_tool(
    text: &str,
    valid_tools: &std::collections::HashMap<String, crate::types::ToolDef>,
) -> (String, serde_json::Value) {
    let tools = parse_fallback_tools(text, valid_tools);
    tools.into_iter().next().unwrap_or(("unknown".into(), serde_json::json!({})))
}

pub fn gen_tool_id() -> String {
	    format!("toolu_{}", Uuid::new_v4().to_string().replace('-', "")[..24].to_string())
	}
	/// 解析 fallback XML/JSON 工具调用（支持多工具）
	pub fn parse_fallback_tools(
	    text: &str,
	    valid_tools: &HashMap<String, ToolDef>,
	) -> Vec<(String, serde_json::Value)> {
	    let mut results = Vec::new();
	    let normalized = text.replace('▁', "_");
	    let text = normalized.as_str();
	    // DeepSeek DSML 格式多工具解析:
	    // <｜tool_call_begin｜>function<｜tool_sep｜>name<｜tool_call_argument_begin｜>{json}<｜tool_call_end｜>
	    const DSML_CALL_BEGIN: &str = "<｜tool_call_begin｜>";
	    const DSML_SEP: &str = "<｜tool_sep｜>";
	    const DSML_ARG_BEGIN: &str = "<｜tool_call_argument_begin｜>";
	    const DSML_CALL_END: &str = "<｜tool_call_end｜>";
	    let mut search_pos = 0;
	    while let Some(begin_pos) = text[search_pos..].find(DSML_CALL_BEGIN) {
	        let abs_begin = search_pos + begin_pos;
	        let after_begin = &text[abs_begin + DSML_CALL_BEGIN.len()..];
	        if let Some(sep_pos) = after_begin.find(DSML_SEP) {
	            let after_sep = &after_begin[sep_pos + DSML_SEP.len()..];
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
	                        .unwrap_or(name.clone());
	                    let args = if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
	                        args
	                    } else if let Some(start) = args_str.find('{') {
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
	                            serde_json::from_str::<serde_json::Value>(&args_str[start..end]).unwrap_or(serde_json::json!({}))
	                        } else {
	                            serde_json::json!({})
	                        }
	                    } else {
	                        serde_json::json!({})
	                    };
	                    results.push((matched_name, args));
	                }
	                // 更新搜索位置
	                let consumed = after_args.as_ptr() as usize - text.as_ptr() as usize + args_end + DSML_CALL_END.len();
	                search_pos = consumed;
	                continue;
	            }
	        }
	        search_pos = abs_begin + DSML_CALL_BEGIN.len();
	    }
	    if !results.is_empty() {
	        return results;
	    }
	    // 如果没有多工具，尝试单工具解析
	    if let Some((name, args)) = parse_single_fallback_tool(text, valid_tools) {
	        results.push((name, args));
	    }
	    results
	}
	/// 解析单个 fallback XML/JSON 工具调用
	fn parse_single_fallback_tool(
	    text: &str,
	    valid_tools: &HashMap<String, ToolDef>,
	) -> Option<(String, serde_json::Value)> {
	    // invoke XML 属性格式: <invoke name="Bash"><parameter name="command">dir</parameter></invoke>
	    let lower_text = text.to_lowercase();
	    if let Some(invoke_start) = lower_text.find("<invoke").or_else(|| text.find("<｜invoke｜")) {
	        if let Some(name_start) = text[invoke_start..].find("name=\"") {
	            let name_val_start = invoke_start + name_start + 6;
	            if let Some(name_end) = text[name_val_start..].find('"') {
	                let t_name = text[name_val_start..name_val_start + name_end].trim().to_string();
	                let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {
	                    invoke_start + end_pos
	                } else if let Some(end_pos) = text[invoke_start..].find("</｜invoke｜>") {
	                    invoke_start + end_pos
	                } else {
	                    text.len()
	                };
	                let invoke_body = &text[invoke_start..invoke_end];
	                let mut args_map = serde_json::Map::new();
	                let mut search_from = 0usize;
	                while let Some(p_start) = invoke_body[search_from..].to_lowercase().find("<parameter") {
	                    let abs_p = search_from + p_start;
	                    if let Some(p_name_begin) = invoke_body[abs_p..].find("name=\"") {
	                        let p_name_s = abs_p + p_name_begin + 6;
	                        if let Some(p_name_len) = invoke_body[p_name_s..].find('"') {
	                            let p_name = invoke_body[p_name_s..p_name_s + p_name_len].trim().to_string();
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
	                            } else { search_from = abs_p + 10; }
	                        } else { search_from = abs_p + 10; }
	                    } else { search_from = abs_p + 10; }
	                    if search_from >= invoke_body.len() { break; }
	                }
	                if !t_name.is_empty() && !args_map.is_empty() {
	                    return Some((t_name, serde_json::Value::Object(args_map)));
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
	                return Some((name, args));
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
	                    return Some((name.to_string(), args.clone()));
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
	                        return Some((t_name.clone(), serde_json::json!({key: inner})));
	                    }
	                    if props_map.contains_key("command") {
	                        return Some((t_name.clone(), serde_json::json!({"command": inner})));
	                    }
	                }
	            }
	        }
	    }
	    None
	}
	#[cfg(test)]
	mod tests {
	    use super::*;
	    #[test]
	    fn test_parse_fallback_tool_json_block() {
	        let valid_tools = std::collections::HashMap::new();
	        let input = r#"{"name": "Bash", "arguments": {"command": "ls"}}"#;
	        let tools = parse_fallback_tools(input, &valid_tools);
	        assert_eq!(tools.len(), 1);
	        assert_eq!(tools[0].0, "Bash");
	        assert_eq!(tools[0].1.get("command").unwrap().as_str().unwrap(), "ls");
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
	        let tools = parse_fallback_tools(input, &valid_tools);
	        assert_eq!(tools.len(), 1);
	        assert_eq!(tools[0].0, "Bash");
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
	        let tools = parse_fallback_tools(input, &valid_tools);
	        assert_eq!(tools.len(), 1);
	        assert_eq!(tools[0].0, "Bash");
	        assert_eq!(tools[0].1.get("command").unwrap().as_str().unwrap(), "ls -la");
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
	        let tools = parse_fallback_tools(input, &valid_tools);
	        assert_eq!(tools.len(), 1);
	        assert_eq!(tools[0].0, "Bash"); // 应返回 valid_tools 中的精确 key
	    }
	    #[test]
	    fn test_parse_fallback_tool_dsml_multi() {
	        let valid_tools = std::collections::HashMap::from([
	            ("Bash".into(), ToolDef {
	                name: "Bash".into(),
	                description: Some("Run commands".into()),
	                input_schema: Some(serde_json::json!({
	                    "properties": {"command": {"type": "string"}}
	                })),
	            }),
	            ("Edit".into(), ToolDef {
	                name: "Edit".into(),
	                description: Some("Edit file".into()),
	                input_schema: Some(serde_json::json!({
	                    "properties": {"filePath": {"type": "string"}, "newString": {"type": "string"}}
	                })),
	            })
	        ]);
	        let input = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>Bash<｜tool▁call▁argument▁begin｜>{\"command\":\"ls\"}<｜tool▁call▁end｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>Edit<｜tool▁call▁argument▁begin｜>{\"filePath\":\"a.txt\",\"newString\":\"hello\"}<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
	        let tools = parse_fallback_tools(input, &valid_tools);
	        assert_eq!(tools.len(), 2);
	        assert_eq!(tools[0].0, "Bash");
	        assert_eq!(tools[0].1.get("command").unwrap().as_str().unwrap(), "ls");
	        assert_eq!(tools[1].0, "Edit");
	        assert_eq!(tools[1].1.get("filePath").unwrap().as_str().unwrap(), "a.txt");
	    }
	    #[test]
	    fn test_gen_tool_id() {
	        let id = gen_tool_id();
	        assert!(id.starts_with("toolu_"));
	        assert_eq!(id.len(), 30); // toolu_ + 24 hex chars
	    }
	}