# RelayForge — Handoff

## 目标

让 Codex 的模型能通过 MCP `spawn_agent` tool 启动独立子 agent，且子 agent 可以使用**与父 agent 不同的模型厂商**（目前目标：父用 DeepSeek，子用 GLM-5.2）。

## 代码位置

```
/root/InnovationLab/code/codex-mcp-spawner/
├── src/main.rs          — MCP server + spawner + proxy daemon CLI
├── src/proxy.rs         — Responses↔Chat 翻译代理
├── Cargo.toml            — rmcp 1.7.0, reqwest, schemars, toml
└── n-skill.md  — 精简版 skill 文件
```

## 架构

```
Codex(父, DeepSeek)                      子 agent(GLM-5.2)
      │                                        │
      ├─ cc-switch(:15721)→ api.deepseek.com   │
      │                                        │
      ├─ MCP spawner ──► codex exec ──► Provider Proxy(:15722) ──► open.bigmodel.cn
      │                  (子进程)       (沙箱外 daemon,          (直连, 不走cc-switch)
      │                                  Responses↔Chat翻译)
      │
      └─ 父 agent 不知道子 agent 的 provider 细节
```

Provider Proxy 是提前在 Codex 沙箱外启动的 daemon。它接收子 agent Codex 的 Responses API 请求，翻译成 Chat Completions 发给上游，把 SSE 响应翻译回 Responses 格式。MCP server 不再需要在 `spawn_agent` 调用期间 bind TCP；它只把子 agent 的临时 `CODEX_HOME/config.toml` 指向 provider 的 `proxy_url`。

子 agent 的临时 `CODEX_HOME` 会包含 `model-catalog.json`。这是 Codex CLI 的本地模型元数据，不属于 cc-switch。Provider Proxy 不再提供 `/v1/models`，只处理 `/v1/responses`。

## 关键设计决策

1. **隔离上下文 (opencode 模式)** — 子 agent 从空上下文开始，仅收 `message` 参数。不 fork 父历史。原因：跨 provider 时 prompt cache 不共享，fork 历史 = 全量 prefill 浪费。
2. **凭证集中管理** — `~/.codex/mcp-spawner.toml` 一个文件管所有 provider 的 base_url + api_key。不再散落 bashrc / config.toml / MCP env。
3. **provider 配置完备性** — 临时 config.toml + model catalog 由 spawner 从头生成，不依赖父 config。
4. **沙箱外协议适配** — Responses↔Chat proxy 作为固定 provider daemon 运行，避免 Codex MCP sandbox 中的 `TcpListener::bind` 限制，也避免 cc-switch 全局 provider 切换。

## 当前状态

| 组件 | 状态 | 备注 |
|---|---|---|
| MCP server 注册 | ✅ | config.toml `[mcp_servers.subagent-spawner]` |
| 工具注册 + schema | ✅ | spawn_agent(task_name, message, provider, model?, cwd?, timeout_sec? 仅覆盖默认时填写) |
| 请求翻译 (R→C) | ✅ | `responses_to_chat()` |
| SSE 翻译 (C→R) | ✅ | `SseState` 状态机，5 种 SSE 事件 |
| proxy daemon | ✅ | `codex-mcp-spawner proxy --provider glm --listen 127.0.0.1:15722` |
| skill 文件 | ✅ | `~/.codex/skills/n/SKILL.md` |
| 单测 | ✅ | 9 个 test 全过（TCP smoke test 在 sandbox deny bind 时显式跳过） |
| cc-switch 配置 | ✅ | DeepSeek + GLM provider 均在 DB 中 |
| GLM API 连通 | ✅ | curl 验证 `completed` |
| TCP bind sandbox 问题 | ✅ | bind 移到沙箱外 daemon |

## proxy daemon 配置

`~/.codex/mcp-spawner.toml` 中 provider 需要保留上游 `base_url` / `api_key`，并必填给子 agent 使用的本地 `proxy_url`：

```toml
[providers.glm]
base_url = "https://open.bigmodel.cn/api/paas/v4"
proxy_url = "http://127.0.0.1:15722"
api_key = "..."
default_timeout_sec = 1800

[[providers.glm.models]]
slug = "glm-5.2"
context_window = 128000
```

启动 daemon：

```bash
codex-mcp-spawner proxy --provider glm --listen 127.0.0.1:15722
```

生产上建议用 systemd user service 或 shell supervisor 在启动 Codex 前先启动该 daemon。

## VM 信息

```
IP: 192.168.122.9
用户: arch
SSH: 已配免密 (ssh arch@192.168.122.9)
Codex: codex-cli 0.139.0
cc-switch: ~/cc-switch.AppImage (xvfb-run)
DeepSeek API key: configured locally, do not commit
GLM API key: configured locally, do not commit
```

## 关键文件 on VM

```
~/.codex/
├── config.toml                    # model_provider=deepseek, mcp_servers
├── cc-switch-model-catalog.json   # 父 Codex/cc-switch 使用，非 subagent catalog
├── mcp-spawner.toml               # [providers.glm]
└── skills/n/SKILL.md

~/.cc-switch/
├── cc-switch.db                   # providers: default(deepseek), glm, codex-official
└── settings.json                  # currentProviderCodex: "default" (deepseek)

~/.local/bin/
└── codex-mcp-spawner              # release binary
```

## 快速命令

```bash
# 构建
cd /root/InnovationLab/code/codex-mcp-spawner && cargo build --release

# 部署到 VM
scp target/release/codex-mcp-spawner arch@192.168.122.9:~/.local/bin/

# 启动 GLM provider proxy daemon（在 Codex 沙箱外执行）
ssh arch@192.168.122.9 '
nohup ~/.local/bin/codex-mcp-spawner proxy --provider glm --listen 127.0.0.1:15722 \
  > /tmp/codex-glm-proxy.log 2>&1 &
'

# 重启 cc-switch
ssh arch@192.168.122.9 '
pkill -f cc-switch; sleep 2
export GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1 \
  WEBKIT_DISABLE_COMPOSITING_MODE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1
nohup xvfb-run -a ~/cc-switch.AppImage > /tmp/ccswitch.log 2>&1 &
'

# 运行测试
cd /root/InnovationLab/code/codex-mcp-spawner && cargo test

# 验证 GLM API 连通（通过 cc-switch）
curl -s http://127.0.0.1:15721/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"glm-5.2","input":[{"role":"user","content":"hi"}],"stream":false}'
```
