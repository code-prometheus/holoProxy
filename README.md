# holoProxy

**Claude Code API 代理 — Rust 实现**

将 Claude Code 的 Anthropic Messages API 透明转发到任意 OpenAI 兼容 LLM，支持流式 SSE、工具调用、自动恢复、Windows 系统托盘。

[![CI](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml/badge.svg)](https://github.com/code-prometheus/holoProxy/actions/workflows/ci.yml)

---

## 功能

- **协议转换**：Anthropic Messages API → OpenAI Chat Completions
- **SSE 流处理**：完整的状态机，原生 tool_calls + XML 工具调用拦截双通道
- **Tools Instruction 注入**：不支持原生 function calling 的模型自动注入 XML 格式指令
- **自动恢复**：关键词权重打分，防止 Claude Code 因下游输出纯文本卡死
- **上下文裁剪**：超过 80% 最大 token 时自动裁剪历史消息
- **纯文本清洗**：去控制字符 + 压缩空白，每请求输出压缩比
- **断线重连**：下游 LLM 连接失败后静默重试 3 次（无 sleep 延迟），失败将错误直接反馈给 Claude Code
- **连接池复用**：全局 reqwest::Client 连接池，首次请求后 TCP 连接复用，大幅降低首 token 延迟
- **Windows 系统托盘**：右键切换模型
- **连接管理**：300s 超时自动清理

## 快速开始

### 1. 下载

从 [Releases](https://github.com/code-prometheus/holoProxy/releases) 下载最新 `holoProxy-windows-x64.tar.gz`。

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

系统托盘出现蓝色 "h" 图标。右键可切换模型。

### 4. 配置 Claude Code

将 Claude Code 的 API endpoint 指向 `http://127.0.0.1:5430`。

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/v1/messages` | Anthropic Messages API（自动转 OpenAI） |
| GET | `/v1/models` | 获取可用模型列表 |
| POST | `/v1/select_model` | 切换激活模型 |

## 从源码构建

```bash
# 需要 Rust 1.70+
cargo build --release
# 输出: target/release/holo_proxy.exe
```

## 技术栈

Rust · axum · tokio · reqwest · tray-icon · winit · indexmap

## License

MIT
