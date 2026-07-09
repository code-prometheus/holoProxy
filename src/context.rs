use crate::types::OpenAIMessage;
use tracing::{info, warn};

// ================= 1. 解析与估算 =================

pub fn parse_context_length(s: &str) -> usize {
    let s = s.trim().to_lowercase();
    if let Some(num) = s.strip_suffix('m') {
        (num.parse::<f64>().unwrap_or(0.2) * 1_000_000.0) as usize
    } else if let Some(num) = s.strip_suffix('k') {
        (num.parse::<f64>().unwrap_or(200.0) * 1_000.0) as usize
    } else {
        s.parse::<usize>().unwrap_or(200_000)
    }
}

/// 启发式 Token 估算
/// 英文/数字/半角标点：约 4 字符 = 1 token (0.25)
/// 中文/全角字符：约 1 字符 = 1~1.5 token (取 1.2 保守估算)
fn estimate_tokens(text: &str) -> usize {
    let mut tokens: f64 = 0.0;
    for c in text.chars() {
        if c.is_ascii() {
            tokens += 0.25;
        } else {
            tokens += 1.2;
        }
    }
    (tokens.ceil() as usize).max(1)
}

/// 估算单条消息的 token (辅助函数，用于优化裁剪性能)
fn estimate_single_msg_tokens(msg: &OpenAIMessage) -> usize {
    let mut count = 0;
    if let Some(c) = &msg.content { count += estimate_tokens(c); }
    if let Some(tc) = &msg.tool_calls {
        for t in tc {
            count += estimate_tokens(&t.function.name) + estimate_tokens(&t.function.arguments);
        }
    }
    count + 4 // message overhead
}

pub fn estimate_token_count(messages: &[OpenAIMessage]) -> usize {
    messages.iter().map(estimate_single_msg_tokens).sum()
}

// ================= 2. 超时与裁剪逻辑 =================

pub fn calc_timeout_secs(body_bytes: usize) -> u64 {
    ((body_bytes as f64 / 200_000.0 * 60.0) as u64).max(60)
}

pub fn should_trim(messages: &[OpenAIMessage], max_context: usize) -> bool {
    let token_threshold = (max_context as f64 * 0.80_f64) as usize;
    estimate_token_count(messages) > token_threshold
}

/// 强剪：保留 system + 最终消息，O(N) 性能
pub fn trim_messages(messages: &mut Vec<OpenAIMessage>, max_context: usize) {
    let token_threshold = (max_context as f64 * 0.80_f64) as usize;
    let before_count = messages.len();
    let before_tokens = estimate_token_count(messages);

    let sys_idx = messages.iter().position(|m| m.role == "system");
    let start = sys_idx.map(|i| i + 1).unwrap_or(0);

    // 计算最多可以删除多少条 (至少保留 start 之后的最后 2 条)
    let max_removable = messages.len().saturating_sub(start + 2);
    if max_removable == 0 { return; }

    let mut current_tokens = before_tokens;
    let mut remove_count = 0;

    // 从前往后累加要删除的消息 token，直到剩下的 token 满足阈值
    for i in 0..max_removable {
        let msg_tokens = estimate_single_msg_tokens(&messages[start + i]);
        if current_tokens - msg_tokens <= token_threshold {
            break;
        }
        current_tokens -= msg_tokens;
        remove_count += 1;
    }

    if remove_count > 0 {
        messages.drain(start..start + remove_count);
        let after_tokens = estimate_token_count(messages);
        info!(
            "✂️ [Context Trim Done] {}→{}条 (删{}条) | {}→{}tokens",
            before_count, messages.len(), remove_count, before_tokens, after_tokens
        );
    } else {
        warn!("🚨 [Context TRIM] 触发，但无法裁剪 (可能历史消息太少，需保留最后2条)");
    }
}

// ================= 3. 单元测试 =================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world!"), 3);
        assert_eq!(estimate_tokens("你好世界啊"), 6);
    }

    #[test]
    fn test_trim_optimization() {
        let mut msgs = vec![OpenAIMessage {
            role: "system".into(), content: Some("sys".into()),
            tool_calls: None, tool_call_id: None,
        }];
        for _ in 0..100 {
            msgs.push(OpenAIMessage {
                role: "user".into(), content: Some("a".repeat(100)),
                tool_calls: None, tool_call_id: None,
            });
        }
        trim_messages(&mut msgs, 600);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs.len() < 100);
    }
}
