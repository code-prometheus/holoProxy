use crate::types::*;

const TOOLS_INSTRUCTION: &str = "\n\n[Tools Instruction]\nIf you cannot use Native API function calling, output EXACTLY:\n<tool_call>\n{\"name\": \"tool_name\", \"arguments\": {\"arg\": \"val\"}}\n</tool_call>";

/// 清洗历史消息中的恢复标记
pub fn clean_recovery_garbage(text: &str) -> String {
    if text.contains("[holoProxy Recovery") {
        "(Previous auto-recovery signal omitted to save context)".into()
    } else {
        text.into()
    }
}

/// Anthropic 请求 → OpenAI Chat Completions 请求
pub fn convert_to_openai_req(anthropic_req: &AnthropicRequest, llm_config: &LLMConfig) -> OpenAIRequest {
    let mut openai_req = OpenAIRequest {
        model: llm_config.model_name.clone(),
        messages: Vec::new(),
        stream: true, // 强制流式，防止下游 LLM 非流式导致超时
        tools: None,
        max_tokens: anthropic_req.max_tokens.map(|mt| mt.min(8192)),
        temperature: anthropic_req.temperature,
        top_p: anthropic_req.top_p,
        stop: anthropic_req.stop_sequences.as_ref().map(|s| s.iter().take(4).cloned().collect()),
    };

    // 转换 tools
    if !anthropic_req.tools.is_empty() {
        openai_req.tools = Some(
            anthropic_req.tools.iter().map(|t| {
                let mut params = t.input_schema.clone().unwrap_or(serde_json::json!({}));
                if let Some(obj) = params.as_object_mut() {
                    obj.entry("type").or_insert(serde_json::json!("object"));
                    obj.entry("properties").or_insert(serde_json::json!({}));
                }
                OpenAITool {
                    tool_type: "function".into(),
                    function: OpenAIToolFunction {
                        name: t.name.clone(),
                        description: t.description.clone().unwrap_or_default(),
                        parameters: params,
                    },
                }
            }).collect()
        );
    }

    // 构建 system prompt
    let mut system_content = match &anthropic_req.system {
        Some(SystemPrompt::String(s)) => s.clone(),
        Some(SystemPrompt::Blocks(blocks)) => blocks.iter()
            .filter(|b| b.block_type == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        None => String::new(),
    };

    // 仅当模型不支持原生 function calling 时才注入 Tools Instruction
    if !anthropic_req.tools.is_empty() && !llm_config.supports_native_function_calling {
        system_content.push_str(TOOLS_INSTRUCTION);
    }

    if !system_content.is_empty() {
        openai_req.messages.push(OpenAIMessage {
            role: "system".into(), content: Some(system_content),
            tool_calls: None, tool_call_id: None,
        });
    }

    // 转换 messages
    for msg in &anthropic_req.messages {
        convert_message(msg, &mut openai_req.messages);
    }

    sanitize_messages(&mut openai_req.messages);
    openai_req
}

fn convert_message(msg: &AnthropicMessage, out: &mut Vec<OpenAIMessage>) {
    let role = msg.role.as_str();
    match &msg.content {
        AnthropicContent::String(content) => {
            let cleaned = if role == "assistant" { clean_recovery_garbage(content) } else { content.clone() };
            out.push(OpenAIMessage {
                role: role.into(),
                content: Some(if cleaned.is_empty() { "(empty)".into() } else { cleaned }),
                tool_calls: None, tool_call_id: None,
            });
        }
        AnthropicContent::Blocks(blocks) => match role {
            "user" => {
                let mut text_parts: Vec<String> = Vec::new();
                for block in blocks {
                    match block.block_type.as_str() {
                        "text" => { if let Some(t) = &block.text { text_parts.push(t.clone()); } }
                        "tool_result" => {
                            if !text_parts.is_empty() {
                                out.push(OpenAIMessage {
                                    role: "user".into(), content: Some(text_parts.join(" ")),
                                    tool_calls: None, tool_call_id: None,
                                });
                                text_parts.clear();
                            }
                            let res = match &block.content {
                                Some(serde_json::Value::Array(arr)) => arr.iter()
                                    .filter_map(|v| v.as_object()?.get("text")?.as_str().map(|s| s.to_string()))
                                    .collect::<Vec<_>>().join(" "),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            let final_res = if block.is_error.unwrap_or(false) { format!("Error: {}", res) } else { res };
                            out.push(OpenAIMessage {
                                role: "tool".into(),
                                content: Some(if final_res.is_empty() { "(empty tool result)".into() } else { final_res }),
                                tool_calls: None,
                                tool_call_id: Some(block.tool_use_id.clone().unwrap_or_else(|| "unknown".into())),
                            });
                        }
                        _ => {}
                    }
                }
                if !text_parts.is_empty() {
                    out.push(OpenAIMessage {
                        role: "user".into(), content: Some(text_parts.join(" ")),
                        tool_calls: None, tool_call_id: None,
                    });
                }
            }
            "assistant" => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<OpenAIToolCall> = Vec::new();
                for block in blocks {
                    match block.block_type.as_str() {
                        "text" => { if let Some(t) = &block.text { text_parts.push(clean_recovery_garbage(t)); } }
                        "tool_use" => tool_calls.push(OpenAIToolCall {
                            id: block.id.clone().unwrap_or_default(),
                            call_type: "function".into(),
                            function: OpenAIFunctionCall {
                                name: block.name.clone().unwrap_or_default(),
                                arguments: serde_json::to_string(&block.input.clone().unwrap_or(serde_json::json!({})))
                                    .unwrap_or_else(|_| "{}".into()),
                            },
                        }),
                        _ => {}
                    }
                }
                if !text_parts.is_empty() || !tool_calls.is_empty() {
                    let content = if text_parts.is_empty() { Some(String::new()) } else { Some(text_parts.join(" ")) };
                    let tcs = if tool_calls.is_empty() { None } else { Some(tool_calls) };
                    if content.as_deref() == Some("") && tcs.is_none() {
                        out.push(OpenAIMessage { role: "assistant".into(), content: Some("(empty)".into()), tool_calls: None, tool_call_id: None });
                    } else {
                        out.push(OpenAIMessage { role: "assistant".into(), content, tool_calls: tcs, tool_call_id: None });
                    }
                }
            }
            _ => {
                let content = blocks.iter().filter_map(|b| b.text.as_ref()).cloned().collect::<Vec<_>>().join(" ");
                out.push(OpenAIMessage {
                    role: role.into(),
                    content: Some(if content.is_empty() { "(empty)".into() } else { content }),
                    tool_calls: None, tool_call_id: None,
                });
            }
        },
    }
}

fn sanitize_messages(messages: &mut Vec<OpenAIMessage>) {
    for msg in messages.iter_mut() {
        match msg.role.as_str() {
            "user" | "system" => {
                if msg.content.as_deref().map_or(true, |c| c.trim().is_empty()) {
                    msg.content = Some("(empty)".into());
                }
            }
            "assistant" => {
                if msg.content.is_none() && msg.tool_calls.is_none() {
                    msg.content = Some("(empty)".into());
                }
            }
            "tool" => {
                if msg.content.as_deref().map_or(true, |c| c.trim().is_empty()) {
                    msg.content = Some("(empty tool result)".into());
                }
                if msg.tool_call_id.is_none() {
                    msg.tool_call_id = Some("unknown".into());
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> LLMConfig {
        LLMConfig {
            base_url: "http://localhost:8000/v1".into(), model_name: "test".into(),
            context_max_length: "200k".into(), verify_ssl: false, api_key: "none".into(),
            auth_header: "Authorization".into(), auth_prefix: "Bearer ".into(),
            supports_native_function_calling: false, // 测试默认走 XML tools instruction
            supports_reasoning_content: false,
        }
    }

    #[test]
    fn test_clean_recovery() {
        assert!(!clean_recovery_garbage("[holoProxy Recovery ...]").contains("[holoProxy"));
        assert_eq!(clean_recovery_garbage("normal"), "normal");
    }

    #[test]
    fn test_convert_simple() {
        let req = AnthropicRequest {
            model: "claude".into(),
            messages: vec![AnthropicMessage { role: "user".into(), content: AnthropicContent::String("Hello".into()) }],
            system: Some(SystemPrompt::String("Be helpful.".into())),
            tools: vec![], stream: true, max_tokens: Some(1024),
            temperature: None, top_p: None, stop_sequences: None,
        };
        let oa = convert_to_openai_req(&req, &test_config());
        assert_eq!(oa.messages.len(), 2);
        assert_eq!(oa.messages[0].role, "system");
    }

    #[test]
    fn test_tools_instruction() {
        let req = AnthropicRequest {
            model: "claude".into(),
            messages: vec![AnthropicMessage { role: "user".into(), content: AnthropicContent::String("Hi".into()) }],
            system: None,
            tools: vec![ToolDef { name: "Bash".into(), description: None, input_schema: Some(json!({"type":"object","properties":{"command":{"type":"string"}}})) }],
            stream: true, max_tokens: None, temperature: None, top_p: None, stop_sequences: None,
        };
        let oa = convert_to_openai_req(&req, &test_config());
        assert!(oa.tools.is_some());
        assert!(oa.messages[0].content.as_ref().unwrap().contains("[Tools Instruction]"));
    }
}
