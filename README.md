# holoProxy

**Claude Code API 代理 — Rust 实现**

将 Claude Code 的 Anthropic Messages API 透明转发到任意 OpenAI 兼容 LLM，同时提供 OpenAI 透传端点。支持流式 SSE、工具调用、reasoning/thinking 处理、自动恢复、Windows 系统托盘。

[![CI](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml/badge.svg)](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml)

## 功能

- **双协议端点**：
  - `POST /v1/messages` — Anthropic Messages API → OpenAI 协议转换
  - `POST /v1/chat/completions` — OpenAI API 直接透传（reasoning → `<thinking>` 标签）
- **SSE 流处理**：完整状态机，原生 tool_calls + XML 工具调用拦截双通道
- **reasoning/thinking 支持**：
  - Anthropic 路径：`delta.reasoning` / `delta.reasoning_content` → 独立 thinking content_block
  - OpenAI 透传路径：reasoning → `<thinking></thinking>` 标签 + content 合并
- **Tools Instruction 注入**：不支持原生 function calling 的模型自动注入 XML 格式指令
- **智能恢复**：硬编码拦截 API 错误/超时 + LLM 语义判断正常结束/异常截断，精准注入 fake tool 防止 Claude Code 报 API Error 退出
- **请求增强**：发送到下游前自动注入 `chat_template_kwargs`（`thinking: true, reasoning_effort: "max"`）+ `stream: true`
- **断线重连**：下游连接失败后静默重试 3 次，error 路径无条件注入 fake tool
- **上下文管理**：超过 80% 阈值自动裁剪，日志记录压缩比
- **Windows 系统托盘**：右键切换模型

## 快速开始

### 1. 下载

从 [Releases](https://github.com/code-prometheus/holoProxy/releases) 下载最新 `holoProxy-windows-x64.zip`。

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
      "api_key": "none"
    }
  }
}
```

字段说明：

| 字段 | 说明 | 默认值 |
|------|------|--------|
| `base_url` | OpenAI 兼容 API 地址 | - |
| `model_name` | 模型名 | - |
| `context_max_length` | 最大上下文（支持 `200k`/`1m`） | `200k` |
| `api_key` | API 密钥 | `none` |
| `auth_header` | 认证头 | `Authorization` |
| `auth_prefix` | 认证前缀 | `Bearer ` |
| `supports_native_function_calling` | 是否支持原生 function calling | `true` |
| `supports_reasoning_content` | 是否支持推理内容 | `false` |

### 3. 运行

```bash
./holo_proxy.exe
```

系统托盘出现蓝色 "h" 图标，右键可切换模型。

### 4. 配置客户端

- **Claude Code**：将 API endpoint 指向 `http://127.0.0.1:5430`（自动使用 `/v1/messages`）
- **OpenAI 客户端**：使用 `http://127.0.0.1:5430/v1/chat/completions` 作为 API endpoint

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/v1/messages` | Anthropic Messages API（自动转 OpenAI） |
| POST | `/v1/chat/completions` | OpenAI Chat Completions API（直接透传 + reasoning 处理） |
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
