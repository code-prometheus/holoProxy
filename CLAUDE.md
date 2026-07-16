# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

holoProxy — 用 Rust 重写的 Claude Code API 代理。核心目标：将 Claude Code 的 Anthropic API 请求 (`/v1/messages`) 转换为任意下游 LLM 的 OpenAI 兼容 API 格式，实现流式响应和工具调用的透明代理。最终编译为 Windows 托盘应用，右键可切换下游模型。

参考项目：`F:\AIQuantTrade\proxy-bridge`（Python 实现，仅复刻其 Claude Code 接口部分）。

## 技术栈

- **语言**: Rust (stable)
- **HTTP 框架**: axum (基于 tokio/tower)
- **HTTP 客户端**: reqwest (异步，支持流式 SSE)
- **序列化**: serde + serde_json
- **Windows 托盘**: tray-icon + winit
- **配置**: serde_json 读写 JSON 配置文件
- **日志**: tracing + tracing-subscriber

## 核心架构

### Anthropic → OpenAI 协议转换流程

```
Claude Code (客户端)
 │ POST /v1/messages (Anthropic Messages API 格式)
 ▼
holoProxy (127.0.0.1:5430)
 │ 1. 解析 Anthropic 请求体 (model, messages, tools, stream, system)
 │ 2. 转换为 OpenAI Chat Completions 格式
 │ 3. 注入 Tools Instruction
 │ 4. 发送到下游 LLM
 ▼
下游 LLM (OpenAI 兼容 API)
 │ SSE 流式响应 或 非流式 JSON
 ▼
holoProxy 解析响应
 │ 流式：SSE 状态机 → Anthropic SSE 事件
 │ 非流式：JSON → Anthropic 响应格式
 ▼
Claude Code (客户端收到标准 Anthropic SSE/JSON)
```

### 关键模块设计

#### 1. 配置模块 (`config.rs`)
- 读取 `settings.json`（与 proxy-bridge 格式兼容）
- 动态加载 LLM 列表和当前激活的 LLM
- 支持运行时切换 `active_llm`

#### 2. 协议转换模块 (`converter.rs`)
- `AnthropicRequest` → `OpenAIRequest` 结构体映射
- system prompt 处理（Anthropic 的 system 是顶层字段，OpenAI 是 messages 中的 role=system）
- messages 内容块转换（user/assistant/tool_result → OpenAI 格式）
- **Tools Instruction 注入**：对不支持原生 function calling 的模型，在 system prompt 末尾注入 XML 格式工具调用指令
- 每次请求的 context 长度和信息条数记入 log
- 上下文超过 LLM 配置 context_max_length 的 75% 时自动裁剪
- 恢复信号清洗：替换 `[holoProxy Recovery ...]` 为压缩文本

#### 3. SSE 流处理模块 (`stream.rs`)
核心 SSE 状态机，将 OpenAI 格式 SSE 流转换为 Anthropic 格式 SSE 流。

状态转换：`message_start` → `content_block_start (text)` → `content_block_delta` → … → `content_block_stop` → `content_block_start (tool_use)` → `content_block_delta (input_json_delta)` → … → `content_block_stop` → `message_delta` + `message_stop`

关键逻辑：
- **原生 tool_calls 处理**：下游返回 `delta.tool_calls[]` 时，按 index 追踪，输出 Anthropic tool_use 事件
- **XML 工具调用拦截**：下游在 content 文本中输出 `<tool_call>...</tool_call>` XML 时，拦截并转换为正式 tool_use 事件
- **Fallback 解析**：DSML 标签、JSON 块、MD 代码块等多种格式的工具调用尝试解析
- **finish_reason 映射**：`tool_calls`/stop/length → `stop_reason: "tool_use"` 或 `"end_turn"`
- **错误恢复**：`send_error()` 在 agent 模式下直接注入 fake tool，防止 Claude Code 因无 tool_use 响应报 API Error

#### 4. 自动恢复机制 (`recovery.rs`)

**核心：硬编码拦截 + LLM 语义判断双保险。** agent 模式下无 tool_use 时，硬编码匹配 API 错误/超时关键词快速拦截，匹配不到再调用 LLM 语义判断是否正常结束。LLM 判断为 COMPLETE（总结、提问、等待用户操作等）不注入，确保正常对话不被误杀。

**两路恢复入口**：
- `finish()`：硬编码拦截 → LLM 语义判断 → 按需注入 fake tool
- `send_error()`：重试耗尽后无条件注入 fake tool（走到这里的必然是异常）

**should_recover() 判断流程**：
1. `stop_reason == "length"` → 直接触发
2. 包含 `[holoProxy Recovery/Error]` → 跳过（防死循环）
3. 硬编码匹配 14 种关键词（`timed out`, `empty or malformed response`, `api error`, `502/503/504`, `gateway`, `connection`, `unreachable` 等）→ 直接触发
4. 空文本/纯空白 → 直接触发
5. 以上都不满足 → LLM 语义判断（独立线程 + tokio runtime）

**LLM 判断 ask_llm_if_incomplete()**：
- 截取文本尾部 ~1500 字节发送给下游 LLM
- Prompt 定义三类场景：
  - NORMAL STOP — 自然结束、总结、提问、等待用户操作 → COMPLETE
  - API/NETWORK ERROR — HTTP 错误、超时、网关异常等系统错误文本 → INCOMPLETE
  - ABNORMAL CUT-OFF — 中途截断、半句话、未闭合代码块 → INCOMPLETE
- 判断结果计入日志 `[Recovery] LLM 判断结果: COMPLETE/INCOMPLETE`
- 仅 INCOMPLETE 触发恢复

**恢复动作 pick_recovery_tool()**：
- 优先选取 Bash/Shell/RunCommand/Execute 等无害工具
- 注入跨平台命令：`echo "Fake tool calling ..." && pwd && ls -la || cd && dir`
- 标记：`[holoProxy Recovery Injected]`

#### 5. HTTP 代理模块 (`server.rs`)
- 监听 `127.0.0.1:5430`
- 路由：`POST /v1/messages`、`GET /v1/models`、`POST /v1/select_model`
- 重连机制：每次重试新建 `reqwest::Client`（`pool_max_idle_per_host=0`）
- 日志格式 `[msg_id] attempt=N/3 remaining=M url=... http=STATUS in=X.Xs`

#### 6. Windows 托盘模块 (`tray.rs`)
- 系统托盘图标 + 右键菜单列出所有 LLM（激活标记 ✓）
- 点击切换：更新 settings.json 中的 `active_llm`

## 构建与运行

```bash
cargo build --release
cargo run
cargo test
cargo test -- --nocapture
cargo check      # 仅类型检查
cargo clippy     # Lint 检查
```

Release 输出：`target/release/holo_proxy.exe`

## 配置文件

`settings.json`（与 proxy-bridge 格式兼容）：
```json
{
 "active_llm": "DeepSeek V4",
 "llms": {
 "DeepSeek V4": {
 "base_url": "http://xxx:8000/v1",
 "model_name": "dsv4",
 "context_max_length": "1m",
 "api_key": "none"
 }
 }
}
```

## 注意事项

- 参考 proxy-bridge 时仅关注 Claude Code 接口逻辑，忽略 Chrome 扩展等无关功能
- 对下游 LLM 的 SSL 证书验证默认忽略（`danger_accept_invalid_certs: true`）
- 下游 API 返回错误时，返回 200 + 包含错误信息的 SSE 流，而非直接返回 5xx
- 恢复机制注入的命令必须跨平台兼容（Windows cmd + Linux/Mac bash）
- 下游 LLM 连接失败时静默重试 3 次（每次新建 Client）
- 每次重试都用 `fresh_client()` 新建 `reqwest::Client`（`pool_max_idle_host=0`）
