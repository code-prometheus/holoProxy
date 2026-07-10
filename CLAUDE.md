# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

holoProxy — 用 Rust 重写的 Claude Code API 代理。核心目标：将 Claude Code 的 Anthropic API 请求 (`/v1/messages`) 转换为任意下游 LLM 的 OpenAI 兼容 API 格式，实现流式响应和工具调用的透明代理。最终编译为 Windows 托盘应用，右键可切换下游模型。

参考项目：`F:\AIQuantTrade\proxy-bridge`（Python 实现，仅复刻其 Claude Code 接口部分）。

## 技术栈

- **语言**: Rust (stable)
- **HTTP 框架**: axum (基于 tokio/tower，类型安全，与 tower 中间件生态兼容)
- **HTTP 客户端**: reqwest (异步，支持流式 SSE)
- **序列化**: serde + serde_json
- **Windows 托盘**: tray-icon + winit（原生 Windows 系统托盘）
- **配置**: serde_json 读写 JSON 配置文件
- **日志**: tracing + tracing-subscriber

## 核心架构（参考 proxy-bridge）

### Anthropic → OpenAI 协议转换流程

```
Claude Code (客户端)
    │  POST /v1/messages  (Anthropic Messages API 格式)
    ▼
holoProxy (127.0.0.1:5430)
    │  1. 解析 Anthropic 请求体 (model, messages, tools, stream, system)
    │  2. 转换为 OpenAI Chat Completions 格式
    │  3. 注入 Tools Instruction (强制非原生 function calling 的模型输出 XML 格式工具调用)
    │  4. 发送到下游 LLM
    ▼
下游 LLM (OpenAI 兼容 API)
    │  SSE 流式响应 或 非流式 JSON
    ▼
holoProxy 解析响应
    │  流式：SSE 状态机 → Anthropic SSE 事件
    │  非流式：JSON → Anthropic 响应格式
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
- messages 内容块转换：
  - user: text + tool_result → OpenAI user/tool 消息
  - assistant: text + tool_use → OpenAI assistant 消息（含 tool_calls）
- **Tools Instruction 注入**：对不支持原生 function calling 的模型，在 system prompt 末尾注入：
  ```
  [Tools Instruction]
  If you cannot use Native API function calling, output EXACTLY:
  <tool_call>
  {"name": "tool_name", "arguments": {"arg": "val"}}
  </tool_call>
  ```
- 每次请求的context的长度和信息条数都要计入log里面，方便调试
- 上下文长度超过LLM配置里面的context_max_length的75%时，要能自动裁剪（原理参照claude code的自动裁剪机制，因为下游是自定义模型claude code不会触发自动裁剪，只能自己搞！）
- 取消垃圾标签清洗！垃圾标签清洗完以后有时会造成空响应错误！
- 恢复信号清洗：替换 `[Proxy-Bridge: Recovery Injected]` 为压缩文本

#### 3. SSE 流处理模块 (`stream.rs`)
**这是最核心、最复杂的模块**。实现一个 SSE 状态机，将下游 LLM 的 OpenAI 格式 SSE 流转换为 Anthropic 格式 SSE 流。

核心状态机 (对应 `StreamContext`)：
- `message_start` → `content_block_start (text)` → `content_block_delta (text_delta)` → … → `content_block_stop`
- 然后 `content_block_start (tool_use)` → `content_block_delta (input_json_delta)` → … → `content_block_stop`
- 最后 `message_delta` + `message_stop`

关键处理逻辑：
- **原生 tool_calls 处理**：下游返回 `delta.tool_calls[]` 时，按 index 追踪工具调用，输出 Anthropic tool_use 事件
- **XML 工具调用拦截**：如果下游在 content 文本中输出 `<tool_call>...</tool_call>` XML，需要拦截并转换为正式的 tool_use 事件（使用 `valid_triggers` 和 `CLOSING_GARBAGE`/`STRAY_GARBAGE` 列表匹配）
- **Fallback 解析**：对 DSML 标签、JSON 块、MD 代码块等多种格式的工具调用尝试解析
- **finish_reason 映射**：`tool_calls`/stop/length → `stop_reason: "tool_use"` 或 `"end_turn"`

#### 4. 自动恢复机制 (`recovery.rs`)
防止下游 LLM 输出纯文本/废话导致 Claude Code 卡死：
要自动判断是不是claude code要求用户回答一个问题或者是session真的成功执行完，是的话不要再自动回复

- **触发条件**（`StreamContext.finish()`）：
  - Agent 模式（请求中有 tools）且未生成任何 tool_use
  - 下游 stop_reason == 'length'（被截断）
  - 下游输出了纯文本但无 tool_use
- **恢复动作**（`inject_recovery_tool()`）：
  - 动态选取一个无害工具（优先 Bash/Shell/RunCommand）
  - 注入无害命令：`echo "Fake tool calling ..." && pwd && ls -la || cd && dir`
  - 向 Claude Code 发送 `[holoProxy Recovery ...]` 标记文本
- **上下文清洗**（`clean_recovery_garbage()`）：
  - 在下一次请求构建时，将历史消息中的恢复标记替换为简短文本，防止上下文膨胀

#### 5. HTTP 代理模块 (`server.rs`)
- 监听 `127.0.0.1:5430`
- 路由分发：
  - `POST /v1/messages` → LLM 代理处理（异步，不阻塞 accept 循环）
  - `GET /v1/models` → 返回可用模型列表和当前激活模型
  - `POST /v1/select_model` → 切换 active_llm
  - 定时清理长时间未响应的 LLM 线程
- **断线重连**：下游 LLM 连接失败时静默重试 3 次（无 sleep 延迟），全部失败后将错误通过 SSE 流直接反馈给 Claude Code
- **连接池**：全局 `reqwest::Client`（`OnceLock`，`pool_max_idle_per_host=8`），首次请求建立 TCP 连接后复用，重试时新建 Client（`pool_max_idle=0`）避免复用断开的连接
- 日志格式 `[msg_id] attempt=N/3 remaining=M conn=pooled|fresh url=...`，含 HTTP 状态码、连接耗时详细记录

#### 6. Windows 托盘模块 (`tray.rs`)
- 系统托盘图标
- 右键菜单：列出所有已配置的下游 LLM（当前激活的模型标记 ✓）
- 点击切换：更新 settings.json 中的 `active_llm` 字段（无需重启代理）

## 关键数据结构

### Anthropic Request (输入)
```rust
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    system: Option<SystemPrompt>,      // String 或 Vec<TextBlock>
    tools: Vec<ToolDef>,
    stream: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop_sequences: Option<Vec<String>>,
}
```

### OpenAI Request (输出)
```rust
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    stream: bool,
    tools: Option<Vec<OpenAITool>>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<Vec<String>>,
}
```

### Settings 配置结构
```rust
struct Settings {
    active_llm: String,
    llms: IndexMap<String, LLMConfig>,
}
```

## 构建与运行

```bash
cargo build --release          # 编译 release 版本
cargo run                      # 开发运行
cargo test                     # 运行测试
cargo test -- --nocapture      # 运行测试并显示输出
cargo check                    # 仅类型检查，不编译
cargo clippy                   # Lint 检查
```

Release 输出：`target/release/holo_proxy.exe`（Windows 托盘应用）

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

## 依赖库

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
axum = "0.7"
reqwest = { version = "0.12", features = ["stream", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
tray-icon = "0.19"            # Windows 系统托盘
winit = "0.30"                # 窗口事件循环（托盘需要）
```

## 实现优先级

1. **协议转换**（Anthropic → OpenAI，含 tools instruction 注入）
2. **SSE 流处理状态机**（OpenAI SSE → Anthropic SSE，含 XML 工具调用拦截）
3. **自动恢复机制**（防止纯文本输出卡死 Claude Code）
4. **HTTP 代理服务**（监听 5430，路由分发）
5. **配置管理**（settings.json 读写 + 动态切换）
6. **Windows 托盘**（右键菜单切换模型）

## 注意事项

- 参考 proxy-bridge 时仅关注 `anthropic_proxy.py` 的 Claude Code 接口逻辑，忽略 Chrome 扩展、Native Messaging、远程隧道、TLS 中间人等与 Claude Code 代理无关的功能
- 忽略 TLS/MITM 代理、域名分流、Chrome Native Messaging 桥接等功能
- 对下游 LLM 的 SSL 证书验证默认忽略（`danger_accept_invalid_certs: true`）
- 下游 API 返回错误时，返回 200 + 包含错误信息的 SSE 流，而不是直接返回 5xx，避免 Claude Code 崩溃
- 恢复机制注入的命令必须跨平台兼容（同时兼容 Windows cmd 和 Linux/Mac bash）
- 下游 LLM 连接失败时静默重试 3 次（无 sleep），失败后将错误通过 `send_error()` SSE 流反馈给 Claude Code
- 首次请求用全局 `shared_http_client()`（连接池复用），重试时新建 Client（`pool_max_idle=0`）避免复用断开的连接