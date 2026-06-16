# RelayForge

RelayForge is a minimal Rust MCP server and provider proxy for launching isolated Codex subagents with a different model provider than the main agent.

The current target setup is:

- Main Codex agent: DeepSeek through `cc-switch`
- Subagent: GLM through RelayForge's local Responses-to-Chat proxy
- Coordination: MCP `spawn_agent` tool plus a Codex skill that teaches the main agent when to delegate

## Components

- `src/main.rs` registers the MCP `spawn_agent` tool, creates a temporary subagent `CODEX_HOME`, and launches `codex exec`.
- `src/proxy.rs` runs the provider proxy daemon.
- `src/proxy/request.rs` converts Codex Responses API requests into Chat Completions requests.
- `src/proxy/stream.rs` converts Chat Completions SSE chunks back into Responses API events.
- `skills/spawn-agent/SKILL.md` documents the DeepSeek planner + GLM implementation workflow.
- `scripts/deploy-vm.sh` builds, deploys, and restarts the VM-side proxy.

## Build

```bash
cargo build --release
```

## Test

```bash
cargo test
```

## Run Proxy

Create `~/.codex/mcp-spawner.toml` with a provider entry:

```toml
[providers.glm]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
proxy_url = "http://127.0.0.1:15722"
api_key = "..."

[[providers.glm.models]]
slug = "glm-5.2"
context_window = 128000
```

Then start the proxy daemon:

```bash
codex-mcp-spawner proxy --provider glm --listen 127.0.0.1:15722
```

## MCP Config

Add the server to `~/.codex/config.toml`:

```toml
[mcp_servers.subagent-spawner]
command = "/home/arch/.local/bin/codex-mcp-spawner"
startup_timeout_sec = 10
tool_timeout_sec = 900

[mcp_servers.subagent-spawner.tools.spawn_agent]
approval_mode = "approve"
```

