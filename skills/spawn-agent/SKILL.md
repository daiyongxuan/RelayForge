---
name: spawn-agent
description: Spawn isolated Codex subagents through MCP. Use when a coding task benefits from DeepSeek as the main planner/reviewer and GLM as the implementation subagent, when work can be delegated with a self-contained prompt, when verifying the subagent-spawner MCP path, or when a different provider/model should perform code writing.
---

# DeepSeek + GLM Workflow

Use the MCP `subagent-spawner` tool with `provider: "glm"` for implementation work that can be specified independently.

Default division of labor:

- Main agent: read broad context, compare designs, plan the implementation, write the self-contained subagent prompt, review the result, and run final verification.
- GLM subagent: edit code, add tests, run the requested verification commands, and report exact files changed and command results.

Do not use the subagent for trivial one-liners, unclear tasks, or work that depends on hidden conversation history.

# Prompt Contract

The subagent starts with an empty context. The `message` must include:

- Absolute repo path and working directory.
- Exact files to inspect or modify.
- The intended behavior and non-goals.
- Code quality constraints from the user or repo.
- Verification commands.
- A requirement to fail loudly and report errors instead of guessing.

Set `cwd` to the target repo. Use `provider: "glm"`. Leave `model` null unless the user requests a specific GLM model.

# Review Contract

After the subagent returns:

- Inspect the changed files yourself.
- Remove redundant code if the subagent introduced overlap.
- Run final verification from the main agent.
- Treat `exit_code: 0` as insufficient unless the expected files changed and tests passed.

# Example

Use this shape for implementation delegation:

```json
{
  "task_name": "protocol-replay-tests",
  "provider": "glm",
  "model": null,
  "cwd": "/root/InnovationLab/code/codex-mcp-spawner",
  "timeout_sec": 900,
  "message": "You are implementing tests in /root/InnovationLab/code/codex-mcp-spawner. Inspect src/proxy/request.rs and src/proxy/stream.rs. Add focused Rust tests for Responses-to-Chat history conversion and Chat-SSE tool-call conversion. Keep changes minimal, delete redundant tests if needed, and follow AGENTS.md reliability rules: fail fast, never hide errors, never silently ignore unexpected states. Run cargo fmt --check and cargo test -- --nocapture. Report exact files changed and command results."
}
```
