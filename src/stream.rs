use crate::recovery;
use crate::types::*;
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;
use tracing::warn;
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

    fn ensure_text_open(&mut self) {
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
        if self.intercept_active {
            // 拦截模式：收集到 active_close_tag 为止
            self.intercept_buffer.push_str(content);
            if self.intercept_buffer.contains(&self.active_close_tag) {
                let close_idx =
                    self.intercept_buffer.find(&self.active_close_tag).unwrap()
                        + self.active_close_tag.len();
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

            // 发送 open_tag 之前的文本
            if idx > 0 {
                let pre_text = self.text_buffer[..idx].to_string();
                self.send_text_delta(&pre_text);
            }

            self.intercept_active = true;
            self.active_close_tag = close_tag;
            self.intercept_buffer = self.text_buffer[idx..].to_string();
            self.text_buffer.clear();
        } else if self.text_buffer.len() > 35 {
            // 缓冲够大且没有触发标签 → 发送文本（保留尾部 35 字符防止截断）
            // 用 floor_char_boundary 确保不在多字节字符中间切分
            let safe_cut = self.text_buffer.len() - 35;
            let send_len = self.text_buffer.floor_char_boundary(safe_cut);
            let send_text = self.text_buffer[..send_len].to_string();
            self.send_text_delta(&send_text);
            self.text_buffer = self.text_buffer[send_len..].to_string();
        }
    }

    /// 处理原生 tool_calls delta
    pub fn handle_tool_call(&mut self, tc: &OpenAIToolCallDelta) {
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

    /// 处理 reasoning_content
    pub fn handle_reasoning(&mut self, text: &str) {
        self.send_text_delta(text);
    }

    /// 结束流：关闭所有开放块 + 自动恢复判断 + 发送 message_delta/message_stop
    pub fn finish(&mut self, upstream_stop_reason: &str) {
        // 保底：防止空响应
        if !self.text_open && !self.has_tool_use {
            self.send_text_delta(" ");
        }

        // 刷新 text_buffer 中剩余的内容
        if !self.text_buffer.is_empty() && !self.intercept_active {
            let remaining = std::mem::take(&mut self.text_buffer);
            self.send_text_delta(&remaining);
        }

        // 如果拦截模式未关闭
        if self.intercept_active {
            let remaining = std::mem::take(&mut self.intercept_buffer);
            let (tool_name, tool_args) = parse_fallback_tool(&remaining, &self.valid_tools);
            if tool_name != "unknown" && self.valid_tools.contains_key(&tool_name) {
                let tool_id = gen_tool_id();
                self.open_tool(&tool_id, &tool_name);
                let args_str = serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
                self.send_tool_delta(&args_str);
                self.close_tool();
            }
            self.intercept_active = false;
        }

        self.close_text();
        self.close_tool();

        // 关闭所有原生 tool_calls
        for _ in 0..self.active_native_tools.len() {
            self.close_tool();
        }
        self.active_native_tools.clear();

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
        self.send_text_delta(msg);
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

fn gen_tool_id() -> String {
    format!("toolu_{}", Uuid::new_v4().to_string().replace('-', "")[..24].to_string())
}

/// 解析 fallback XML/JSON 工具调用
fn parse_fallback_tool(
    text: &str,
    valid_tools: &HashMap<String, ToolDef>,
) -> (String, serde_json::Value) {
    // DSML 格式: <tool_name>Name</tool_name><tool_arguments>{"arg":"val"}</tool_arguments>
    let ds_name = Regex::new(r"(?i)<[｜|]?(?:DSML[｜|]?)?tool_name[｜|]?>\s*(.*?)\s*(?:</|[｜|]|$)")
        .ok();
    let ds_args = Regex::new(
        r"(?i)<[｜|]?(?:DSML[｜|]?)?(?:tool_arguments|parameter)[｜|]?>\s*(.*?)\s*(?:</[｜|]|$)",
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
    fn test_gen_tool_id() {
        let id = gen_tool_id();
        assert!(id.starts_with("toolu_"));
        assert_eq!(id.len(), 30); // toolu_ + 24 hex chars
    }
}
