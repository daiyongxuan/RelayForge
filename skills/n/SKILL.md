---
name: n
description: Delegate isolated implementation work to a subagent. Use for code writing, test generation, refactoring, or batch edits that can be specified without shared conversation context.
---

# When to Use

Use `spawn_agent` with `agent_type: "worker"` when the task satisfies **all three**:

1. **Self-contained** — the prompt can describe the full job without referencing this conversation.
2. **Code-heavy** — the work is writing/editing code, tests, configs, or docs.
3. **Verifiable** — a clear command exists to check correctness (`cargo test`, `pytest`, `bash -n`, etc.).

Do **not** use for: reading or analyzing code, making decisions, tasks < 10 lines, or anything needing conversation context.

# How to Call

Always include these 6 sections in `message`:

```
1. CONTEXT: repo path, why this task exists, relevant files to READ first.
2. TASK: exactly what to implement/change. Be specific about behavior.
3. DO NOT: explicit boundaries — files/conventions not to touch.
4. CONSTRAINTS: lint rules, test frameworks, code style from AGENTS.md.
5. VERIFY: exact commands to run for validation.
6. ON FAILURE: "Report the exact error. Do not guess. Do not work around it silently."
```

Set `cwd` to the repo root. Default `timeout_sec` is 600; set higher for builds.

# After Subagent Returns

1. Read `final_message` — if it reports an error, trust it. Do not re-run the same prompt.
2. If `exit_code != 0` or `final_message` is empty: the subagent failed. Either fix it yourself or spawn a corrected prompt.
3. If `exit_code == 0`: open the changed files and verify the diff makes sense. Remove dead code or overlap.
4. Run the verification command yourself before reporting success to the user.
5. Report to the user: what changed, whether tests pass, any caveats.

# Retry Pattern

If subagent fails, spawn again with a **corrected prompt** — add the error message, narrow the scope, or clarify instructions. Never retry the identical prompt expecting different results.

# Example

```json
{
  "task_name": "add-auth-tests",
  "provider": "glm",
  "cwd": "/home/arch/project",
  "timeout_sec": 600,
  "message": "CONTEXT: /home/arch/project, auth middleware in src/auth.py.\nTASK: Add 3 pytest tests for token expiry edge cases in tests/test_auth.py. Cover: expired token returns 401, malformed token returns 400, missing token returns 401.\nDO NOT: modify src/auth.py, change existing tests, or add new dependencies.\nCONSTRAINTS: Use pytest.raises for exceptions. Follow existing test naming (test_<scenario>).\nVERIFY: Run `python -m pytest tests/test_auth.py -v` and confirm all pass.\nON FAILURE: Report the exact error and traceback. Do not guess fixes."
}
```
