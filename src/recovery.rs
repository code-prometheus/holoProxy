/// 判断是否需要在 finish 时注入恢复工具
/// 用关键词权重打分代替硬匹配：score > 0 → 不恢复，score <= 0 → 恢复
pub fn should_recover(generated_text: &str, stop_reason: &str) -> Option<String> {
    // 0. stop_reason == 'length' → 总是需要恢复
    if stop_reason == "length" {
        return Some("stop_reason=length".into());
    }

    if generated_text.is_empty() {
        return Some("empty response".into());
    }

    let mut score: i32 = -1; // 默认倾向恢复

    // 加分项（表明不需要恢复）
    for &(kw, weight) in &[
        // 问用户问题 → 强信号
        ("你想", 5), ("你希望", 5), ("你打算", 5), ("你需要", 4), ("你确认", 4),
        ("请告诉我", 5), ("请选择", 5), ("请确认", 4), ("请问", 4),
        ("Do you want", 5), ("Would you like", 5), ("你更喜欢", 4), ("你决定", 4),
        ("你怎么看", 4), ("你的意见", 4), ("你觉得", 4), ("你倾向于", 4),
        // 任务完成 → 强信号
        ("全部就绪", 5), ("任务完成", 5), ("已完成", 4), ("工作结束", 4),
        ("成功完成", 4), ("执行完毕", 4), ("准备就绪", 4),
        ("All tasks completed", 5), ("✅", 3), ("🎉", 3), ("✨", 2),
        // session 正常结束标记
        ("改动总结", 5), ("总结如下", 5), ("以下是总结", 5), ("变更总结", 4),
        ("本次对话", 3), ("本次会话", 3), ("任务总结", 4), ("工作总结", 4),
        ("主要变化", 3), ("核心变化", 3), ("改动包括", 3),
        ("Summary", 4), ("In summary", 5), ("Here's a summary", 5),
        ("Changes made", 4), ("Key changes", 3),
        ("完成✅", 4), ("搞定了", 3), ("收工", 3),
        // 要求手动测试
        ("手动测试", 4), ("请测试", 4), ("请验证", 3), ("请检查", 3),
        ("Please test", 4), ("Please verify", 4), ("Please check", 4),
        ("试一下", 3), ("试试看", 3),
    ] {
        if generated_text.contains(kw) {
            score += weight;
        }
    }

    // 减分项（说明输出混乱/无意义，更需要恢复）
    for &(kw, weight) in &[
        ("[holoProxy Recovery", -3),   // 已经是恢复标记，减分防止循环
        ("[holoProxy Error", -3),
    ] {
        if generated_text.contains(kw) {
            score += weight;
        }
    }

    if score > 0 {
        return None;
    }

    Some("score <= 0, no completion/question/manual-test/summary flag".into())
}

/// 动态选取恢复工具：优先 Bash/Shell/RunCommand/Execute
pub fn pick_recovery_tool(
    valid_tools: &std::collections::HashMap<String, &crate::types::ToolDef>,
) -> Option<(String, serde_json::Value)> {
    if valid_tools.is_empty() {
        return None;
    }

    let priority_names = [
        "Bash", "Shell", "bash", "shell",
        "Execute", "Run_Command", "RunCommand", "terminal", "Terminal",
    ];
    for name in &priority_names {
        if let Some(tool) = valid_tools.get(*name) {
            return Some((name.to_string(), build_recovery_args(tool)));
        }
    }

    for (name, tool) in valid_tools.iter() {
        let props = tool.input_schema.as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props_map) = props {
            if props_map.contains_key("command") {
                return Some((name.clone(), serde_json::json!({
                    "command": "echo \"Fake tool calling ...\" && pwd && ls -la || cd && dir"
                })));
            }
        }
    }

    let (name, tool) = valid_tools.iter().next().unwrap();
    Some((name.clone(), build_recovery_args(tool)))
}

fn build_recovery_args(tool: &crate::types::ToolDef) -> serde_json::Value {
    let props = tool.input_schema.as_ref()
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
    use crate::types::ToolDef;

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
    fn test_should_recover_on_empty() {
        assert!(should_recover("", "stop").is_some());
    }

    #[test]
    fn test_should_not_recover_on_strong_signal() {
        // 多个强信号叠加 → score 很高 → 不恢复
        assert!(should_recover("任务完成！你需要什么调整吗？总结如下：全部就绪", "stop").is_none());
    }

    #[test]
    fn test_pick_recovery_tool_with_bash() {
        let bash_tool = ToolDef {
            name: "Bash".into(),
            description: Some("Run commands".into()),
            input_schema: Some(serde_json::json!({"properties": {"command": {"type": "string"}}})),
        };
        let tools: std::collections::HashMap<String, &ToolDef> =
            vec![("Bash".into(), &bash_tool)].into_iter().collect();
        let (name, args) = pick_recovery_tool(&tools).unwrap();
        assert_eq!(name, "Bash");
        assert!(args.get("command").unwrap().as_str().unwrap().contains("Fake tool calling"));
    }
}
