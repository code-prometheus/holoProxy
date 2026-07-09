use crate::types::ToolDef;
use std::collections::HashMap;
use tracing::{info, warn};

/// 判断是否需要在 finish 时注入恢复工具
/// 返回 None 表示不需要恢复，Some(reason) 表示需要恢复
pub fn should_recover(generated_text: &str, stop_reason: &str) -> Option<String> {
    // 0. stop_reason == 'length' → 总是需要恢复（输出被截断）
    if stop_reason == "length" {
        return Some("下游 LLM 触发长度限制 (stop_reason=length)".into());
    }

    // 1. 检测 Claude Code 是否在问用户问题（不要恢复！）
    let question_patterns = [
        "你想", "你希望", "你打算", "你需要", "你确认",
        "请告诉我", "请选择", "请确认", "请问",
        "Do you want", "Would you like", "Which", "Choose",
        "你更喜欢", "你的选择", "你决定", "你想用",
        "你怎么看", "你的意见", "你觉得", "你倾向于",
    ];
    for p in &question_patterns {
        if generated_text.contains(p) {
            info!("✅ [Recovery] 检测到 Claude Code 在问用户问题，跳过恢复");
            return None;
        }
    }

    // 2. 检测任务是否已完成（不要恢复！）
    let completion_keywords = [
        "全部就绪", "任务完成", "已完成", "工作结束",
        "所有任务均已完成", "没有更多需要", "无需进一步",
        "成功完成", "执行完毕", "准备就绪",
        "All tasks completed", "Task finished", "Done",
        "✅", "🎉", "✨",
        "需要做什么调整吗", "还有什么我可以",
        "请告诉我下一步",
    ];
    for kw in &completion_keywords {
        if generated_text.contains(kw) {
            info!("✅ [Recovery] 检测到任务已完成标志，跳过恢复");
            return None;
        }
    }

    // 3. 检测 Claude Code 是否要求用户手动测试（不要恢复！）
    let manual_test_patterns = [
        "手动测试", "请测试", "请验证",
        "请检查", "请运行", "请执行", "测试一下",
        "试一下", "试试看", "你可以测试",
        "Please test", "Please verify", "Please check",
        "manually test", "try it", "test it",
        "运行一下", "执行一下", "检查一下",
        "验证一下", "确认一下",
    ];
    for p in &manual_test_patterns {
        if generated_text.contains(p) {
            info!("✅ [Recovery] 检测到 Claude Code 要求用户手动测试，跳过恢复");
            return None;
        }
    }

    // 4. 检测 Claude Code 是否输出了总结/摘要（session 正常结束标志）
    let summary_patterns = [
        // 中文
        "改动总结", "总结如下", "以下是总结", "变更总结", "修改总结",
        "本次对话", "本次会话", "本次修改", "本次改动", "本次变更",
        "完成总结", "任务总结", "工作总结",
        "主要变化", "核心变化", "改动包括", "做了什么",
        "以下是本次", "如下所示",
        // 英文
        "Summary", "In summary", "Here's a summary",
        "Changes made", "What was changed", "Modified files",
        "Key changes", "Overview of changes",
        // 信号词（表明在做收尾性陈述）
        "完成✅", "搞定了", "收工",
    ];
    for p in &summary_patterns {
        if generated_text.contains(p) {
            info!("✅ [Recovery] 检测到 session 结束标志（总结/摘要），跳过恢复");
            return None;
        }
    }

    // 5. 其他情况 → 需要恢复
    warn!(
        "🚨 [Recovery] 下游 LLM 未生成 Tool Use 且无完成/问问题/手动测试标志，将触发恢复"
    );
    Some("下游 LLM 输出了纯文本/废话，未生成 Tool Use 且无完成标志".into())
}

/// 动态选取恢复工具：优先 Bash/Shell/RunCommand/Execute
pub fn pick_recovery_tool(valid_tools: &HashMap<String, &ToolDef>) -> Option<(String, serde_json::Value)> {
    if valid_tools.is_empty() {
        return None;
    }

    // 优先匹配 Bash/Shell 类工具
    let priority_names = ["Bash", "Shell", "bash", "shell", "Execute", "Run_Command", "RunCommand", "terminal", "Terminal"];
    for name in &priority_names {
        if let Some(tool) = valid_tools.get(*name) {
            let args = build_recovery_args(*tool);
            return Some((name.to_string(), args));
        }
    }

    // 查找有 command 属性的工具
    for (name, tool) in valid_tools.iter() {
        let props = tool
            .input_schema
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props_map) = props {
            if props_map.contains_key("command") {
                return Some((name.clone(), serde_json::json!({"command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"})));
            }
        }
    }

    // 取第一个工具
    let (name, tool) = valid_tools.iter().next().unwrap();
    let args = build_recovery_args(tool);
    Some((name.clone(), args))
}

fn build_recovery_args(tool: &ToolDef) -> serde_json::Value {
    let props = tool
        .input_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object());

    if let Some(props_map) = props {
        if props_map.contains_key("command") {
            return serde_json::json!({
                "command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"
            });
        }
        if props_map.contains_key("path") {
            return serde_json::json!({"path": "./"});
        }
        if props_map.contains_key("query") {
            return serde_json::json!({"query": "*"});
        }
        // 取第一个属性
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
    fn test_should_not_recover_on_question() {
        assert!(should_recover("你想用什么框架？", "stop").is_none());
        assert!(should_recover("Do you want to proceed?", "stop").is_none());
    }

    #[test]
    fn test_should_not_recover_on_completion() {
        assert!(should_recover("任务完成 ✅", "stop").is_none());
        assert!(should_recover("All tasks completed!", "stop").is_none());
    }

    #[test]
    fn test_should_not_recover_on_manual_test() {
        assert!(should_recover("请手动测试一下", "stop").is_none());
        assert!(should_recover("Please test it", "stop").is_none());
    }

    #[test]
    fn test_should_recover_on_bad_output() {
        assert!(should_recover("一些无意义的废话", "stop").is_some());
    }

    #[test]
    fn test_pick_recovery_tool_with_bash() {
        let bash_tool = ToolDef {
            name: "Bash".into(),
            description: Some("Run commands".into()),
            input_schema: Some(serde_json::json!({
                "properties": {"command": {"type": "string"}}
            })),
        };
        let tools: HashMap<String, &ToolDef> = vec![
            ("Bash".into(), &bash_tool),
        ]
        .into_iter()
        .collect();

        let (name, args) = pick_recovery_tool(&tools).unwrap();
        assert_eq!(name, "Bash");
        assert!(args.get("command").unwrap().as_str().unwrap().contains("Fake tool calling"));
    }
}
