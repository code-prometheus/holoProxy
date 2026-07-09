use crate::types::OpenAIMessage;
use tracing::{info, warn};
use std::sync::OnceLock;
use regex::Regex;

// ================= 1. 纯文本清洗 =================
static RE_BLANK: OnceLock<Regex> = OnceLock::new();
static RE_NEWLINES: OnceLock<Regex> = OnceLock::new();
static RE_CTRL: OnceLock<Regex> = OnceLock::new();

fn clean_text(text: &str) -> String {
    let re_ctrl = RE_CTRL.get_or_init(|| Regex::new(r"[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]").unwrap());
    let re_blank = RE_BLANK.get_or_init(|| Regex::new(r"[ \t]+").unwrap());
    let re_newlines = RE_NEWLINES.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    
    // 1. 控制字符替换为单空格（防止单词粘连）
    let t = re_ctrl.replace_all(text, " ");
    // 2. 【修复】连续空格/制表符压缩为 1 个空格（极致省 Token，避免 Markdown 硬换行）
    let t = re_blank.replace_all(&t, " ");
    // 3. 3个及以上换行压缩为 2 个（保留段落间距）
    re_newlines.replace_all(&t, "\n\n").trim().to_string()
}

/// 清洗消息列表，输出压缩比：chars + tokens 变化
pub fn clean_messages(messages: &mut Vec<OpenAIMessage>) {
    let chars_before: usize = messages.iter()
        .map(|m| m.content.as_deref().unwrap_or("").len()).sum();
    let before = estimate_token_count(messages);

    for msg in messages.iter_mut() {
        // ✅ 安全：只清洗纯文本的 content
        if let Some(ref content) = msg.content {
            msg.content = Some(clean_text(content));
        }
        
        // ❌ 移除对 tool_calls.arguments 的清洗，因为它是 JSON，正则清洗会破坏其内部值的语义（如代码换行、特定空格）
        // 如果 arguments 里有脏数据，应该由下游工具在解析 JSON 后自行处理，而不是在 Prompt 层面破坏 JSON 结构。
    }

    let chars_after: usize = messages.iter()
        .map(|m| m.content.as_deref().unwrap_or("").len()).sum();
    let after = estimate_token_count(messages);

    if before > after {
        let ratio = (1.0 - after as f64 / before.max(1) as f64) * 100.0;
        info!("[Clean] chars:{}->{} tok:{}->{} ratio:{:.0}%", chars_before, chars_after, before, after, ratio);
    }
}

// ================= 2. 解析与估算 =================
// ... (保持你原来的 parse_context_length 和 estimate_tokens 不变，写得很好) ...
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

fn estimate_single_msg_tokens(msg: &OpenAIMessage) -> usize {
    let mut count = 0;
    if let Some(c) = &msg.content { count += estimate_tokens(c); }
    if let Some(tc) = &msg.tool_calls {
        for t in tc {
            count += estimate_tokens(&t.function.name) + estimate_tokens(&t.function.arguments);
        }
    }
    count + 4
}

pub fn estimate_token_count(messages: &[OpenAIMessage]) -> usize {
    messages.iter().map(estimate_single_msg_tokens).sum()
}

// ================= 3. 超时与裁剪 =================
// ... (保持你原来的 calc_timeout_secs, should_trim, trim_messages 不变) ...
pub fn calc_timeout_secs(body_bytes: usize) -> u64 {
    ((body_bytes as f64 / 200_000.0 * 60.0) as u64).max(60)
}

pub fn should_trim(messages: &[OpenAIMessage], max_context: usize) -> bool {
    let token_threshold = (max_context as f64 * 0.80_f64) as usize;
    estimate_token_count(messages) > token_threshold
}

pub fn trim_messages(messages: &mut Vec<OpenAIMessage>, max_context: usize) {
    let token_threshold = (max_context as f64 * 0.80_f64) as usize;
    let before_count = messages.len();
    let before_tokens = estimate_token_count(messages);
    
    let sys_idx = messages.iter().position(|m| m.role == "system");
    let start = sys_idx.map(|i| i + 1).unwrap_or(0);
    let max_removable = messages.len().saturating_sub(start + 2);
    if max_removable == 0 { return; }

    let mut current_tokens = before_tokens;
    let mut remove_count = 0;

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
        let ratio = (1.0 - after_tokens as f64 / before_tokens.max(1) as f64) * 100.0;
        info!("[Trim] {}->{}msgs del{} tok:{}->{} ratio:{:.0}%", before_count, messages.len(), remove_count, before_tokens, after_tokens, ratio);
    } else {
        warn!("[Trim] triggered but cannot trim (<2 msgs after system)");
    }
}

// ================= 4. 单元测试 =================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_text_safety() {
        // 测试控制字符和多余空格
        let dirty = "Hello\x00\x01   \t\tworld.\n\n\n\n\nTest.";
        let clean = clean_text(dirty);
        assert_eq!(clean, "Hello world.\n\nTest.");
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world!"), 3); // 12 * 0.25 = 3
        assert_eq!(estimate_tokens("你好世界啊"), 6);   // 5 * 1.2 = 6.0 -> 6
    }
}