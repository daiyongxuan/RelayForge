use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;

mod proxy;
use proxy::ProxyConfig;

// ── spawner config (mcp-spawner.toml) ────────────────────────────────

#[derive(Debug, Deserialize)]
struct SpawnerConfig {
    providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderConfig {
    base_url: String,
    proxy_url: String,
    api_key: String,
    #[serde(default = "default_wire_api")]
    wire_api: String,
    #[serde(default)]
    default_timeout_sec: Option<u64>,
    #[serde(default)]
    models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelConfig {
    slug: String,
    context_window: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct ProxyArgs {
    provider: String,
    listen: String,
    model: Option<String>,
}

fn default_wire_api() -> String {
    "responses".into()
}

fn effective_timeout_sec(args: &SpawnArgs, provider: &ProviderConfig) -> u64 {
    args.timeout_sec
        .or(provider.default_timeout_sec)
        .unwrap_or(600)
}

static NEXT_TRACE_SEQ: AtomicU64 = AtomicU64::new(0);
fn next_trace_id() -> String {
    format!(
        "{:x}-{:x}",
        std::process::id(),
        NEXT_TRACE_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}
fn log_spawner(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {msg}\n");
    eprint!("{line}");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/codex-spawner.log")
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn load_config() -> Result<SpawnerConfig, String> {
    let path = default_codex_home().join("mcp-spawner.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("invalid config: {e}"))
}

// ── MCP server ────────────────────────────────────────────────────────

#[derive(Clone)]
struct Spawner {
    tool: Arc<Tool>,
    config: Arc<SpawnerConfig>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SpawnArgs {
    /// Short identifier for the task (used in logs).
    task_name: String,
    /// Fully self-contained task instruction. The subagent sees ONLY this text.
    message: String,
    /// Provider key as declared in mcp-spawner.toml (e.g. "deepseek", "glm").
    provider: String,
    /// Model slug (e.g. "deepseek-v4-flash", "glm-5.1"). Must be listed in
    /// the provider's [models] section.
    model: Option<String>,
    /// Working directory for the subagent. Defaults to the parent's cwd.
    #[serde(default)]
    cwd: Option<String>,
    /// Optional max seconds to wait. Omit to use the provider's default_timeout_sec;
    /// an explicit value overrides the provider default.
    #[serde(default)]
    timeout_sec: Option<u64>,
}

const TOOL_DESCRIPTION: &str = "\
Launch a separate Codex agent to handle a task. The subagent runs as an \
independent process with its own empty context window. Use for parallel or \
isolated work. Returns the subagent's final message with a progress summary.\n\n\
IMPORTANT: the subagent has NO access to this conversation's history. Write a \
fully self-contained prompt in `message` — include file paths, constraints, and \
verification steps. Do not assume it knows anything discussed earlier.";

#[derive(Debug, Serialize)]
struct AgentEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
}

impl Spawner {
    fn new(config: SpawnerConfig) -> Self {
        let schema = serde_json::to_value(schemars::schema_for!(SpawnArgs))
            .expect("SpawnArgs schema must serialize");
        let schema: JsonObject =
            serde_json::from_value(schema).expect("SpawnArgs schema must be a JSON object");
        Self {
            tool: Arc::new(Tool::new(
                Cow::Borrowed("run"),
                Cow::Borrowed(TOOL_DESCRIPTION),
                Arc::new(schema),
            )),
            config: Arc::new(config),
        }
    }
}

impl ServerHandler for Spawner {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder().enable_tools().build();
        ServerInfo::new(capabilities)
            .with_instructions("Use run to launch isolated Codex subagents.")
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tool = self.tool.clone();
        async move {
            Ok(ListToolsResult {
                tools: vec![(*tool).clone()],
                next_cursor: None,
                meta: None,
            })
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if request.name.as_ref() != "run" {
            return Err(unknown_tool(&request.name));
        }
        let args: SpawnArgs = deserialize_args(&request)?;
        let text = run_subagent(args, self.config.clone()).await;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            text,
        )]))
    }
}

fn unknown_tool(name: &str) -> McpError {
    McpError::invalid_params(format!("unknown tool: {name}"), None)
}

fn deserialize_args(request: &CallToolRequestParams) -> Result<SpawnArgs, McpError> {
    let Some(arguments) = request.arguments.as_ref() else {
        return Err(McpError::invalid_params("missing arguments", None));
    };
    let value = serde_json::Value::Object(arguments.clone().into_iter().collect());
    serde_json::from_value(value).map_err(|e| McpError::invalid_params(e.to_string(), None))
}

// ── subagent execution pipeline ───────────────────────────────────────

async fn run_subagent(args: SpawnArgs, config: Arc<SpawnerConfig>) -> String {
    let t_start = Instant::now();
    let trace_id = next_trace_id();
    let out_path = temp_path(&args.task_name);

    let provider = match config.providers.get(&args.provider) {
        Some(p) => p.clone(),
        None => {
            return diag(
                &args.task_name,
                format!("provider '{}' not in mcp-spawner.toml", args.provider),
                &trace_id,
            )
        }
    };
    let timeout_sec = effective_timeout_sec(&args, &provider);
    let timeout = Duration::from_secs(timeout_sec);
    let model_slug = args.model.as_deref().unwrap_or_else(|| {
        provider
            .models
            .first()
            .map(|m| m.slug.as_str())
            .unwrap_or("unknown")
    });
    let model = match provider.models.iter().find(|m| m.slug == model_slug) {
        Some(m) => m.clone(),
        None => {
            return diag(
                &args.task_name,
                format!("model '{model_slug}' not in provider '{}'", args.provider),
                &trace_id,
            )
        }
    };

    let codex_home = match setup_subagent_home(
        &args.provider,
        model_slug,
        model.context_window,
        &provider.proxy_url,
    ) {
        Ok(home) => home,
        Err(err) => return diag(&args.task_name, err, &trace_id),
    };

    let t_setup_done = Instant::now();
    let mut child = match spawn_codex(&args, &out_path, &codex_home, &trace_id).await {
        Ok(c) => c,
        Err(err) => {
            cleanup_if(true, &codex_home);
            return diag(
                &args.task_name,
                format!("failed to start: {err}"),
                &trace_id,
            );
        }
    };

    let stdout = child.stdout.take().expect("stdout not piped");
    let stderr_pipe = child.stderr.take().expect("stderr not piped");
    let trace_clone = trace_id.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let t = line.trim();
                    if !t.is_empty() {
                        log_spawner(&format!("[spawner] trace={trace_clone} stderr: {t}"));
                    }
                }
            }
        }
    });
    let stdout_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/codex-spawner-stdout.log")
            .ok();
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(ref mut f) = log_file {
                        let _ = std::io::Write::write_all(f, &chunk[..n]);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
        buf
    });
    let exit_status = tokio::select! {
        s = child.wait() => Some(s),
        _ = tokio::time::sleep(timeout) => { let _ = child.kill().await; let _ = child.wait().await; None }
    };
    let stdout_bytes = stdout_handle.await.unwrap_or_default();
    match exit_status {
        Some(Ok(status)) => {
            cleanup_if(true, &codex_home);
            let events = parse_events(&stdout_bytes);
            finish(
                args.task_name,
                args.model,
                args.provider,
                events,
                status,
                &stdout_bytes,
                &out_path,
                trace_id,
                timeout.as_secs(),
                t_start,
                t_setup_done,
            )
        }
        Some(Err(err)) => {
            cleanup_if(true, &codex_home);
            diag(&args.task_name, format!("io error: {err}"), &trace_id)
        }
        None => {
            cleanup_if(true, &codex_home);
            diag(
                &args.task_name,
                format!("timed out after {}s", timeout.as_secs()),
                &trace_id,
            )
        }
    }
}

async fn spawn_codex(
    args: &SpawnArgs,
    out_path: &std::path::Path,
    home: &std::path::Path,
    trace_id: &str,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new("codex");
    cmd.args([
        "exec",
        "--skip-git-repo-check",
        "--json",
        "--sandbox",
        "workspace-write",
    ]);
    if let Some(model) = &args.model {
        cmd.args(["-m", model]);
    }
    cmd.arg("-o").arg(out_path);
    cmd.env("CODEX_HOME", home.to_string_lossy().as_ref());
    if let Some(dir) = &args.cwd {
        cmd.args(["-C", dir]);
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    log_spawner(&format!(
        "[spawner] trace={trace_id} task={} model={} provider={} home={}",
        args.task_name,
        args.model.as_deref().unwrap_or("default"),
        args.provider,
        home.display(),
    ));
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(args.message.as_bytes()).await;
    }
    Ok(child)
}

fn parse_events(stdout: &[u8]) -> Vec<AgentEvent> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(extract_event)
        .collect()
}

fn cleanup_if(should: bool, path: &std::path::Path) {
    if should {
        let _ = std::fs::remove_dir_all(path);
    }
}

// ── result construction ───────────────────────────────────────────────

fn finish(
    task_name: String,
    model: Option<String>,
    provider: String,
    events: Vec<AgentEvent>,
    status: ExitStatus,
    stdout_bytes: &[u8],
    out_path: &std::path::Path,
    trace_id: String,
    timeout_sec: u64,
    t_start: Instant,
    t_setup_done: Instant,
) -> String {
    let final_message = std::fs::read_to_string(out_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let _ = std::fs::remove_file(out_path);
    let turn_count = events
        .iter()
        .filter(|e| e.event_type == "TurnComplete")
        .count();
    let tool_calls: Vec<&str> = events
        .iter()
        .filter(|e| e.event_type == "OutputItemDone")
        .filter_map(|e| e.brief.as_deref())
        .collect();
    let setup_ms = t_setup_done.duration_since(t_start).as_millis() as u64;
    let total_ms = t_start.elapsed().as_millis() as u64;
    let subagent_ms = total_ms.saturating_sub(setup_ms);
    let mut result = json!({
        "trace_id": trace_id, "task_name": task_name, "model": model.unwrap_or_else(|| "inherited".into()), "provider": provider,
        "timeout_sec": timeout_sec,
        "exit_code": status.code(), "turns": turn_count, "tool_calls": tool_calls,
        "timing_ms": {"setup": setup_ms, "subagent": subagent_ms, "total": total_ms},
        "final_message": if final_message.is_empty() { "(no output)" } else { final_message.as_str() },
    });
    if final_message.is_empty() && !status.success() {
        let t = String::from_utf8_lossy(&stdout_bytes[stdout_bytes.len().saturating_sub(2000)..])
            .trim()
            .to_string();
        if !t.is_empty() {
            result["stdout_tail"] = json!(tail(&t, 500));
        }
    }
    serde_json::to_string_pretty(&result)
        .unwrap_or_else(|_| diag(&task_name, "failed to serialize result".into(), &trace_id))
}

// ── event extraction ──────────────────────────────────────────────────

fn extract_event(value: Value) -> AgentEvent {
    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".into());
    let brief = match event_type.as_str() {
        "OutputItemDone" => value.get("item").and_then(item_brief),
        "TurnComplete" => Some("turn finished".into()),
        "TurnStarted" => Some("turn started".into()),
        _ => None,
    };
    AgentEvent { event_type, brief }
}

fn item_brief(item: &Value) -> Option<String> {
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") => item
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|part| part.get("text"))
            .and_then(|t| t.as_str())
            .map(|text| format_clipped(text, 80)),
        Some("function_call") => item
            .get("name")
            .and_then(|n| n.as_str())
            .map(|name| format!("[tool] {}", name)),
        Some("reasoning") => Some("[reasoning]".into()),
        Some(other) => Some(format!("[{}]", other)),
        None => None,
    }
}

fn format_clipped(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or(s);
    if one_line.len() <= max {
        one_line.to_string()
    } else {
        format!("{}…", &one_line[..max])
    }
}

// ── per-agent standalone CODEX_HOME ───────────────────────────────────

fn setup_subagent_home(
    provider_name: &str,
    model_slug: &str,
    context_window: i64,
    proxy_base_url: &str,
) -> Result<PathBuf, String> {
    let home = std::env::temp_dir().join(format!("codex-subagent-{}", sanitize(provider_name)));
    std::fs::create_dir_all(&home).map_err(|e| format!("cannot create temp CODEX_HOME: {e}"))?;

    // Standalone config.toml points subagent Codex at the provider daemon.
    let config_toml = format!(
        r#"model_provider = "custom"
model = "{model_slug}"
model_context_window = {ctx}
model_catalog_json = "model-catalog.json"

[model_providers.custom]
name = "{provider_name}"
base_url = "{proxy_base_url}"
wire_api = "responses"
"#,
        model_slug = model_slug,
        ctx = context_window,
        provider_name = provider_name,
        proxy_base_url = proxy_base_url,
    );
    std::fs::write(home.join("config.toml"), &config_toml)
        .map_err(|e| format!("cannot write config.toml: {e}"))?;

    // Model catalog is local to the generated subagent Codex home.
    let catalog = json!({"models":[{
        "slug": model_slug,
        "display_name": model_slug,
        "description": format!("{model_slug} via codex-mcp-spawner"),
        "context_window": context_window,
        "max_context_window": context_window,
        "auto_compact_token_limit": null,
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Fast"},
            {"effort": "medium", "description": "Balanced"},
            {"effort": "high", "description": "Deep reasoning"},
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "base_instructions": "",
        "supports_reasoning_summaries": false,
        "default_reasoning_summary": "none",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text_and_image",
        "experimental_supported_tools": [],
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": false,
        "effective_context_window_percent": 95,
        "input_modalities": ["text"],
        "supports_search_tool": false,
        "use_responses_lite": false,
        "truncation_policy": {"mode":"bytes","limit":10000},
    }]});
    std::fs::write(
        home.join("model-catalog.json"),
        serde_json::to_string_pretty(&catalog).unwrap(),
    )
    .map_err(|e| format!("cannot write model catalog: {e}"))?;

    Ok(home)
}

fn diag(task_name: &str, detail: String, trace_id: &str) -> String {
    log_spawner(&format!("[spawner] trace={trace_id} error: {detail}"));
    format!("[subagent trace={trace_id} task={task_name} {detail}]")
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

fn temp_path(task_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "codex_subagent_{}_{}.txt",
        sanitize(task_name),
        std::process::id()
    ))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_codex_home() -> PathBuf {
    if let Ok(val) = std::env::var("CODEX_HOME") {
        let p = PathBuf::from(val);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".codex")
}

fn parse_proxy_args(args: &[String]) -> Result<ProxyArgs, String> {
    let mut provider = None;
    let mut listen = None;
    let mut model = None;
    let mut i = 2;

    while i < args.len() {
        match args[i].as_str() {
            "--provider" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "missing value for --provider".to_string())?;
                provider = Some(value.clone());
            }
            "--listen" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "missing value for --listen".to_string())?;
                listen = Some(value.clone());
            }
            "--model" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "missing value for --model".to_string())?;
                model = Some(value.clone());
            }
            other => return Err(format!("unknown proxy argument: {other}")),
        }
        i += 1;
    }

    Ok(ProxyArgs {
        provider: provider.ok_or_else(|| "missing --provider".to_string())?,
        listen: listen.unwrap_or_else(|| "127.0.0.1:15722".to_string()),
        model,
    })
}

async fn run_proxy_daemon(args: ProxyArgs, config: SpawnerConfig) -> anyhow::Result<()> {
    let provider = config
        .providers
        .get(&args.provider)
        .ok_or_else(|| anyhow::anyhow!("provider '{}' not in mcp-spawner.toml", args.provider))?;
    let model_slug = args.model.as_deref().unwrap_or_else(|| {
        provider
            .models
            .first()
            .map(|m| m.slug.as_str())
            .unwrap_or("unknown")
    });
    if !provider.models.iter().any(|m| m.slug == model_slug) {
        anyhow::bail!("model '{model_slug}' not in provider '{}'", args.provider);
    }

    eprintln!(
        "[spawner-proxy] provider={} model={} listen={} upstream={}",
        args.provider, model_slug, args.listen, provider.base_url,
    );
    eprintln!(
        "[spawner-proxy] configured upstream wire_api={}",
        provider.wire_api
    );

    proxy::serve(
        &args.listen,
        ProxyConfig {
            provider_name: args.provider.clone(),
            upstream_base: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            model_slug: model_slug.to_string(),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            log_spawner(&format!("[spawner] config error: {e}"));
            anyhow::bail!("{e}");
        }
    };
    if args.get(1).map(|s| s.as_str()) == Some("proxy") {
        let proxy_args = parse_proxy_args(&args).map_err(|e| anyhow::anyhow!(e))?;
        return run_proxy_daemon(proxy_args, config).await;
    }

    log_spawner(&format!(
        "[spawner] loaded {} provider(s): {}",
        config.providers.len(),
        config
            .providers
            .keys()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ));
    let running = Spawner::new(config)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn setup_subagent_home_writes_config_with_proxy_url() {
        let home =
            setup_subagent_home("glm-test", "glm-5.2", 128000, "http://127.0.0.1:15722").unwrap();

        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let catalog = std::fs::read_to_string(home.join("model-catalog.json")).unwrap();
        assert!(config.contains("model = \"glm-5.2\""));
        assert!(config.contains("model_catalog_json = \"model-catalog.json\""));
        assert!(config.contains("base_url = \"http://127.0.0.1:15722\""));
        assert!(catalog.contains("\"supported_reasoning_levels\""));
        assert!(catalog.contains("\"apply_patch_tool_type\""));
        cleanup_if(true, &home);
    }

    #[test]
    fn effective_timeout_prefers_explicit_then_provider_then_default() {
        let mut provider = ProviderConfig {
            base_url: "https://example.test".into(),
            proxy_url: "http://127.0.0.1:15722".into(),
            api_key: "test-key".into(),
            wire_api: default_wire_api(),
            default_timeout_sec: Some(1800),
            models: Vec::new(),
        };
        let mut args = SpawnArgs {
            task_name: "timeout-test".into(),
            message: "test".into(),
            provider: "glm".into(),
            model: None,
            cwd: None,
            timeout_sec: None,
        };

        assert_eq!(effective_timeout_sec(&args, &provider), 1800);

        args.timeout_sec = Some(42);
        assert_eq!(effective_timeout_sec(&args, &provider), 42);

        args.timeout_sec = None;
        provider.default_timeout_sec = None;
        assert_eq!(effective_timeout_sec(&args, &provider), 600);
    }
}
