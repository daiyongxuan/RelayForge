# Issue: MCP Subagent Observability Enhancement

**Status:** Open
**Created:** 2026-06-16
**Labels:** enhancement, observability, proxy, mcp

## Problem

The MCP subagent spawner + provider proxy lacks structured observability, making debugging and operational monitoring difficult. Current logging is ad-hoc `eprintln!` with no correlation IDs, no timing instrumentation, and no health-check endpoints.

## Sub-issues

### 1. Request tracing with correlation IDs

Every `spawn_agent` call should generate a unique `trace_id` that flows through the entire pipeline:

```
MCP spawn_agent  →  codex exec subprocess  →  proxy daemon request  →  upstream GLM API
    [trace_id]          [trace_id]                [trace_id]              [trace_id]
```

- Inject `trace_id` into the subagent's temp `config.toml` as a custom header or env var
- Proxy daemon logs it per-request
- All error messages and eprintln output include it

### 2. Structured logging

Replace `eprintln!` with structured log records (JSON lines to stderr, or env-controlled log level):

```json
{"ts":"2026-06-16T12:00:00Z","level":"info","trace_id":"abc123","msg":"proxy request","method":"POST","path":"/v1/responses","latency_ms":1234}
```

- Use `tracing` or `log` crate instead of raw `eprintln!`
- Support log levels: `error`, `warn`, `info`, `debug`, `trace`
- In `debug` mode, log raw request/response bodies (truncated)

### 3. Timing breakdown per spawn

Report stage-level timing in the spawn result:

| Stage | What |
|---|---|
| `setup_home_ms` | Creating temp CODEX_HOME, writing config + model catalog |
| `subagent_run_ms` | Wall-clock time of `codex exec` |
| `proxy_translate_ms` | Proxy daemon's request translation + upstream latency |

Add to the `finish()` JSON output so the caller can see where time is spent.

### 4. Proxy daemon health endpoint

Add a `GET /health` endpoint to the proxy daemon:

```json
{"status":"ok","uptime_sec":3600,"requests_total":42,"requests_failed":3,"upstream":"https://open.bigmodel.cn/api/coding/paas/v4"}
```

Enable basic alerting: if upstream becomes unreachable, `/health` returns degraded status.

### 5. Proxy request metrics

Per-request logging with key dimensions:

- `method`, `path`, `status_code`
- `latency_ms` (total), `upstream_latency_ms`
- `stream` vs non-stream
- `input_tokens`, `output_tokens`
- `error` message on failure

### 6. Error enrichment

Every error path should include:

- `trace_id`
- The upstream request context (URL, model, truncated request body)
- The raw upstream error response (truncated)
- Timestamp

Example: instead of `"upstream stream completed without assistant output"`, produce:

```
[proxy] trace_id=abc123 upstream=GLM error="stream completed without assistant output" model=glm-5.2 request_body_snippet="..." 
```

## Acceptance Criteria

1. Every MCP `spawn_agent` call produces a unique `trace_id` visible in all log output
2. Proxy daemon logs structured per-request records with latency and token counts
3. `spawn_agent` return JSON includes timing breakdown
4. `GET /health` returns daemon status
5. All error messages include `trace_id` and upstream context
6. Running with `RUST_LOG=debug` shows raw request/response bodies
