#!/usr/bin/env python3
"""MCP server: expose spawn_agent as a tool to Codex.

When the parent Codex calls spawn_agent, this server runs `codex exec` as a
subprocess, waits for it to finish, and returns the final message.

The child Codex reads the same ~/.codex/config.toml as the parent, so it
automatically uses the same provider (e.g. DeepSeek via cc-switch). Pass
--model to override the model per-subagent.

Requires: pip install mcp
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path

from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import TextContent, Tool

SERVER = Server("codex-subagent-spawner")

# Tool schema mirrors Codex's native spawn_agent (multi_agents_v2).
SPAWN_AGENT_TOOL = Tool(
    name="spawn_agent",
    description=(
        "Launch a separate Codex agent to handle a task. The subagent runs as "
        "an independent process with its own empty context window. Use for "
        "parallel or isolated work. Returns the subagent's final message as text.\n\n"
        "IMPORTANT: the subagent has NO access to this conversation's history. "
        "You must write a fully self-contained prompt in `message` — include all "
        "relevant file paths, prior decisions, and constraints the subagent needs. "
        "Do not assume it knows anything discussed earlier."
    ),
    inputSchema={
        "type": "object",
        "properties": {
            "task_name": {
                "type": "string",
                "description": "Short identifier for the task (used in logs).",
            },
            "message": {
                "type": "string",
                "description": (
                    "Fully self-contained task instruction. The subagent sees "
                    "ONLY this text — no conversation history. Include all "
                    "context, file paths, and constraints it needs."
                ),
            },
            "model": {
                "type": "string",
                "description": "Model override for this subagent (e.g. deepseek-v4-flash). "
                "Omit to inherit the parent's model.",
            },
            "cwd": {
                "type": "string",
                "description": "Working directory for the subagent. Defaults to parent's cwd.",
            },
            "timeout_sec": {
                "type": "integer",
                "description": "Optional max seconds to wait. Omit to use the server/provider default; an explicit value overrides that default.",
            },
        },
        "required": ["task_name", "message"],
    },
)


@SERVER.list_tools()
async def list_tools() -> list[Tool]:
    return [SPAWN_AGENT_TOOL]


@SERVER.call_tool()
async def call_tool(name: str, arguments: dict) -> list[TextContent]:
    if name != "spawn_agent":
        return [TextContent(type="text", text=f"unknown tool: {name}")]

    task_name: str = arguments["task_name"]
    message: str = arguments["message"]
    model: str | None = arguments.get("model")
    cwd: str | None = arguments.get("cwd") or os.getcwd()
    timeout_sec: int = arguments.get("timeout_sec", 1800)

    # Build the codex exec command. --json makes output machine-readable.
    cmd: list[str] = ["codex", "exec", "--json", "--skip-git-repo-check"]
    if model:
        cmd += ["-m", model]
    cmd.append(message)

    # Use -o to capture the final message to a file (reliable extraction).
    out_file = Path(tempfile.gettempdir()) / f"codex_subagent_{task_name}_{os.getpid()}.txt"
    cmd_with_out = cmd[:-1] + ["-o", str(out_file)] + [cmd[-1]]

    sys.stderr.write(f"[spawner] task={task_name} model={model or 'default'} cwd={cwd}\n")
    sys.stderr.flush()

    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd_with_out,
            cwd=cwd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=os.environ.copy(),
        )
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(), timeout=timeout_sec
        )
    except asyncio.TimeoutError:
        proc.kill()  # type: ignore[possibly-undefined]
        return [TextContent(type="text", text=f"[subagent {task_name} timed out after {timeout_sec}s]")]
    except Exception as exc:
        return [TextContent(type="text", text=f"[subagent {task_name} failed to start: {exc}]")]

    # Prefer the -o output file (the agent's final message), fall back to stdout.
    final_message = ""
    if out_file.exists():
        final_message = out_file.read_text(encoding="utf-8", errors="replace").strip()
        out_file.unlink(missing_ok=True)

    if not final_message and stdout:
        final_message = stdout.decode("utf-8", errors="replace").strip()

    if proc.returncode != 0:
        err_tail = stderr.decode("utf-8", errors="replace").strip()[-500:]
        return [TextContent(
            type="text",
            text=f"[subagent {task_name} exited {proc.returncode}]\nstderr: {err_tail}",
        )]

    result = {
        "task_name": task_name,
        "model": model or "inherited",
        "exit_code": proc.returncode,
        "final_message": final_message or "(no output)",
    }
    return [TextContent(type="text", text=json.dumps(result, ensure_ascii=False, indent=2))]


async def main() -> None:
    async with stdio_server() as (read_stream, write_stream):
        await SERVER.run(read_stream, write_stream, SERVER.create_initialization_options())


if __name__ == "__main__":
    asyncio.run(main())
