# holoProxy

**Claude Code API 代理 — Rust 实现**

将 Claude Code 的 Anthropic Messages API 透明转发到任意 OpenAI 兼容 LLM，同时提供 OpenAI 透传端点。支持流式 SSE、工具调用、reasoning/thinking 处理、智能恢复、Windows 系统托盘。

[![CI](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml/badge.svg)](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml)

## 功能

- **双协议端点**：
  - `POST /v1/messages` — Anthropic Messages API → OpenAI 协议转换
  - `POST /v1/chat/completions` — OpenAI API 直接透传（自动替换 model + reasoning → `<thinking>` 标签 + XML/DSML 工具调用拦截）
- **SSE 流处理**：完整状态机，原生 tool_calls + XML/DSML 工具调用拦截双通道
- **reasoning/thinking 支持**：
  - Anthropic 路径：`delta.reasoning` / `delta.reasoning_content` → 独立 thinking content_block
  - OpenAI 透传路径：reasoning 流式输出 `<thinking>` 标签
  - 日志标记 `💭 reasoning` / `📝 content` / `🔧 XML/DSML tool_call` 区分输出类型
- **配置驱动**：`thinking`、`reasoning_effort`、`stream`、`chat_template_kwargs` 均可在 settings.json 中按模型独立配置
- **标准兼容**：`thinking=false` 时不注入 `chat_template_kwargs`，发送标准 OpenAI body
- **Tools Instruction 注入**：不支持原生 function calling 的模型自动注入 XML 格式指令
- **智能恢复**：硬编码拦截 + LLM 语义判断，精准注入 fake tool 防止 Claude Code 退出
- **断线重连**：下游连接失败静默重试 3 次（含 HTTP 状态码检查）
- **上下文管理**：超 80% 阈值自动裁剪
- **Windows 系统托盘**：右键切换模型

## 快速开始

### 1. 下载

从 [Releases](https://github.com/code-prometheus/holoProxy/releases) 下载最新 `holoProxy-windows-x64.zip`。

解压后包含：`holo_proxy.exe`、`settings.json`、`README.md`、`assets/icon.ico`。

### 2. 配置

编辑 `settings.json`：

```json
{
  "active_llm": "DeepSeek V4 pro",
  "llms": {
    "DeepSeek V4 pro": {
      "base_url": "http://your-llm:8000/v1",
      "model_name": "dsv4",
      "context_max_length": "1m",
      "api_key": "none",
      "thinking": true,
      "reasoning_effort": "max",
      "stream": true
    }
  }
}
```

字段说明：

| 字段 | 类型 | 说明 | 默认值 |
|------|------|------|--------|
| `base_url` | string | OpenAI 兼容 API 地址 | - |
| `model_name` | string | 模型名 | - |
| `context_max_length` | string | 最大上下文（`200k`/`1m`） | `200k` |
| `api_key` | string | API 密钥 | `"none"` |
| `auth_header` | string | 认证头（可选） | `Authorization` |
| `auth_prefix` | string | 认证前缀（可选） | `Bearer ` |
| `supports_native_function_calling` | bool | 是否支持原生 function calling | `true` |
| `thinking` | bool | 是否注入 `chat_template_kwargs`（仅 vLLM/SGLang） | `true` |
| `reasoning_effort` | string | reasoning 强度 `"max"` / `"medium"` / `"low"` | `"max"` |
| `stream` | bool | 是否强制流式输出 | `true` |

### 3. 运行

```bash
./holo_proxy.exe
```

系统托盘出现蓝色 "h" 图标，右键可切换模型。

### 4. 配置客户端

- **Claude Code**：API endpoint → `http://127.0.0.1:5430`（自动使用 `/v1/messages`）
- **OpenAI 客户端**：API endpoint → `http://127.0.0.1:5430/v1/chat/completions`

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/v1/messages` | Anthropic Messages API（自动转 OpenAI） |
| POST | `/v1/chat/completions` | OpenAI Chat Completions API（透传 + reasoning + XML 拦截） |
| GET | `/v1/models` | 获取可用模型列表 |
| POST | `/v1/select_model` | 切换激活模型 |

## 从源码构建

```bash
cargo build --release
# 输出: target/release/holo_proxy.exe
```

## 技术栈

Rust · axum · tokio · reqwest · tray-icon · winit

## License

MIT
