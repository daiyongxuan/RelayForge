//! Provider daemon: receives Codex Responses-API, translates to upstream Chat
//! Completions, and translates SSE back to Responses events.

use futures_util::StreamExt;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

#[path = "proxy/request.rs"]
mod request;
#[path = "proxy/stream.rs"]
mod stream;
#[path = "proxy/usage.rs"]
mod usage;

use request::responses_to_chat;
use stream::ChatSseConverter;
use usage::TokenUsage;
use usage::UsageRecord;

#[derive(Clone)]
pub struct ProxyConfig {
    pub provider_name: String,
    pub upstream_base: String,
    pub api_key: String,
    pub model_slug: String,
}

#[derive(Clone)]
struct UsageContext {
    request_id: String,
    session_id: String,
    provider: String,
    model: String,
    stream: bool,
    request_json: String,
    headers_json: String,
}

pub async fn serve(listen_addr: &str, cfg: ProxyConfig) -> Result<(), String> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| format!("proxy bind {listen_addr}: {e}"))?;
    run_forever(listener, cfg).await;
    Ok(())
}

async fn run_forever(listener: TcpListener, cfg: ProxyConfig) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle(stream, cfg.clone()));
            }
            Err(e) => {
                eprintln!("[spawner-proxy] accept error: {e}");
                return;
            }
        }
    }
}

// ── HTTP layer (minimal — no framework dependency) ────────────────────

async fn handle(stream: TcpStream, cfg: ProxyConfig) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let (method, path, headers, body) = match read_request(&mut reader).await {
        Some(r) => r,
        None => return,
    };

    match (method.as_str(), path.as_str()) {
        ("POST", path) if is_responses_path(path) => {
            serve_responses(&mut writer, &headers, &body, &cfg).await
        }
        _ => write_404(&mut writer).await,
    }
}

fn is_responses_path(path: &str) -> bool {
    path == "/v1/responses" || path == "/responses"
}

async fn read_request(
    reader: &mut BufReader<impl AsyncReadExt + Unpin>,
) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.trim().split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.ok()?;
        let raw = line.trim();
        if raw.is_empty() {
            break;
        }
        if let Some((name, value)) = raw.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
            continue;
        }
        let t = raw.to_ascii_lowercase();
        if let Some(val) = t.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }
    Some((method, path, headers, body))
}

async fn write_json(w: &mut (impl AsyncWriteExt + Unpin), code: u16, body: &str) {
    let _ = w.write_all(format!(
        "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
        len = body.len(),
    ).as_bytes()).await;
}

async fn write_404(w: &mut (impl AsyncWriteExt + Unpin)) {
    write_json(w, 404, "{\"error\":\"not_found\"}").await;
}

// ── /v1/responses ─────────────────────────────────────────────────────

async fn serve_responses(
    w: &mut (impl AsyncWriteExt + Unpin),
    headers: &[(String, String)],
    body: &[u8],
    cfg: &ProxyConfig,
) {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            write_json(w, 400, "{\"error\":\"bad json\"}").await;
            return;
        }
    };
    let input_count = req
        .get("input")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let user_msgs = req
        .get("input")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|i| i.get("role").and_then(|v| v.as_str()) == Some("user"))
                .count()
        })
        .unwrap_or(0);
    let is_stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    eprintln!("[proxy] req input_items={input_count} user_turns={user_msgs} stream={is_stream}");
    let usage_ctx = UsageContext {
        request_id: usage::request_id(),
        session_id: {
            let from_body = usage::extract_session_id(&req);
            if from_body == "unknown" {
                usage::extract_session_id_from_headers(headers).unwrap_or(from_body)
            } else {
                from_body
            }
        },
        provider: cfg.provider_name.clone(),
        model: cfg.model_slug.clone(),
        stream: is_stream,
        request_json: String::from_utf8_lossy(body).to_string(),
        headers_json: headers_json(headers),
    };
    let chat = responses_to_chat(&req, &cfg.model_slug);

    if is_stream {
        stream_sse(w, &chat, &cfg, usage_ctx).await;
    } else {
        nonstream(w, &chat, &cfg, usage_ctx).await;
    }
}

// ── non-streaming ─────────────────────────────────────────────────────

async fn nonstream(
    w: &mut (impl AsyncWriteExt + Unpin),
    body: &Value,
    cfg: &ProxyConfig,
    usage_ctx: UsageContext,
) {
    let text = match upstream_fetch(&cfg.upstream_base, &cfg.api_key, body).await {
        Ok(t) => t,
        Err(e) => {
            write_usage(&usage_ctx, "error", zero_usage(), Some(e.clone()));
            write_json(w, 502, &json!({"error":e}).to_string()).await;
            return;
        }
    };
    let cr: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            write_usage(
                &usage_ctx,
                "error",
                zero_usage(),
                Some("bad upstream".into()),
            );
            write_json(w, 502, "{\"error\":\"bad upstream\"}").await;
            return;
        }
    };

    let result =
        chat_completion_to_response(&cr, body["model"].as_str().unwrap_or(""), body.get("tools"));
    write_usage(&usage_ctx, "completed", usage::usage_from_chat(&cr), None);
    write_json(w, 200, &result.to_string()).await;
}

fn chat_completion_to_response(cr: &Value, fallback_model: &str, tools: Option<&Value>) -> Value {
    let msg = &cr["choices"][0]["message"];
    let usage = &cr["usage"];
    let mut output: Vec<Value> = Vec::new();
    if let Some(r) = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        output.push(
            json!({"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":r}]}),
        );
    }
    if let Some(c) = msg
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        output.push(json!({"id":"msg_1","type":"message","role":"assistant",
            "content":[{"type":"output_text","text":c,"annotations":[]}]}));
    }
    let custom_tools = custom_tool_names(tools);
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tool_call in tool_calls {
            output.push(chat_tool_call_to_response_item(tool_call, &custom_tools));
        }
    }
    let token_usage = usage::usage_from_chat_usage(Some(usage));
    json!({"id":cr["id"],"object":"response","model":cr.get("model").cloned().unwrap_or_else(|| json!(fallback_model)),"status":"completed",
        "output":output,"usage":responses_usage(&token_usage)})
}

fn responses_usage(tokens: &TokenUsage) -> Value {
    json!({
        "input_tokens": tokens.input_tokens,
        "output_tokens": tokens.output_tokens,
        "total_tokens": tokens.total_tokens,
        "input_tokens_details": {
            "cached_tokens": tokens.cached_input_tokens,
            "cache_miss_tokens": tokens.cache_miss_input_tokens
        },
        "output_tokens_details": {
            "reasoning_tokens": tokens.reasoning_output_tokens
        }
    })
}

fn chat_tool_call_to_response_item(
    tool_call: &Value,
    custom_tools: &std::collections::BTreeSet<String>,
) -> Value {
    let call_id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let function = tool_call.get("function").unwrap_or(&Value::Null);
    let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = function
        .get("arguments")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if name == "tool_search" {
        return json!({"type":"tool_search_call","call_id":call_id,"status":"completed","execution":"client","arguments":parse_arguments_object(arguments)});
    }
    if custom_tools.contains(name) {
        return json!({"id":format!("ctc_{call_id}"),"type":"custom_tool_call","status":"completed","call_id":call_id,"name":name,"input":custom_tool_input(arguments)});
    }
    let (namespace, name) = split_chat_tool_name(name);
    let mut item = json!({"id":format!("fc_{call_id}"),"type":"function_call","status":"completed","call_id":call_id,"name":name,"arguments":arguments});
    if let Some(namespace) = namespace {
        item["namespace"] = Value::String(namespace);
    }
    item
}

fn custom_tool_names(tools: Option<&Value>) -> std::collections::BTreeSet<String> {
    tools
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name").and_then(|v| v.as_str())?;
            let required_input = function
                .pointer("/parameters/required")
                .and_then(|v| v.as_array())
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("input")));
            let has_input_property = function.pointer("/parameters/properties/input").is_some();
            let description_marks_custom = function
                .get("description")
                .and_then(|v| v.as_str())
                .is_some_and(|description| description.contains("Original custom tool definition"));
            (required_input && has_input_property && description_marks_custom)
                .then(|| name.to_string())
        })
        .collect()
}

fn custom_tool_input(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return String::new();
    }
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("input").cloned())
        .and_then(|value| match value {
            Value::String(input) => Some(input),
            other => Some(other.to_string()),
        })
        .unwrap_or_else(|| arguments.to_string())
}

fn parse_arguments_object(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn split_chat_tool_name(name: &str) -> (Option<String>, String) {
    if let Some((namespace, rest)) = name.rsplit_once("___") {
        if !namespace.is_empty() && !rest.is_empty() {
            return (Some(namespace.to_string()), format!("_{rest}"));
        }
    }
    match name.rsplit_once("__") {
        Some((namespace, name)) if !namespace.is_empty() && !name.is_empty() => {
            (Some(namespace.to_string()), name.to_string())
        }
        _ => (None, name.to_string()),
    }
}

// ── streaming SSE ─────────────────────────────────────────────────────

async fn stream_sse(
    w: &mut (impl AsyncWriteExt + Unpin),
    body: &Value,
    cfg: &ProxyConfig,
    usage_ctx: UsageContext,
) {
    let resp = match upstream_send(&cfg.upstream_base, &cfg.api_key, body).await {
        Ok(resp) => resp,
        Err(e) => {
            write_usage(&usage_ctx, "error", zero_usage(), Some(e.clone()));
            write_json(w, 502, &json!({"error":e}).to_string()).await;
            return;
        }
    };

    let _ = w
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
        .await;
    let (id, model) = ("resp_1", body["model"].as_str().unwrap_or(""));
    let mut converter = ChatSseConverter::new(id, model, body.get("tools"));
    for event in converter.initial_events() {
        let _ = w.write_all(event.as_bytes()).await;
    }
    let _ = w.flush().await;

    let mut pending = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                pending.push_str(&String::from_utf8_lossy(&bytes));
                match converter.push_text(&mut pending) {
                    Ok(events) => {
                        for event in events {
                            let _ = w.write_all(event.as_bytes()).await;
                        }
                        let _ = w.flush().await;
                    }
                    Err(e) => {
                        write_usage(&usage_ctx, "error", zero_usage(), Some(e.clone()));
                        let _ = w.write_all(sse_error(&e).as_bytes()).await;
                        let _ = w.flush().await;
                        return;
                    }
                }
                if converter.is_finished() {
                    write_usage(
                        &usage_ctx,
                        "completed",
                        converter.usage().unwrap_or_else(zero_usage),
                        None,
                    );
                    return;
                }
            }
            Err(e) => {
                write_usage(&usage_ctx, "error", zero_usage(), Some(e.to_string()));
                let _ = w.write_all(sse_error(&e.to_string()).as_bytes()).await;
                let _ = w.flush().await;
                return;
            }
        }
    }
    match converter.finish_without_done() {
        Ok(events) => {
            for event in events {
                let _ = w.write_all(event.as_bytes()).await;
            }
            write_usage(
                &usage_ctx,
                "completed",
                converter.usage().unwrap_or_else(zero_usage),
                None,
            );
            let _ = w.flush().await;
        }
        Err(e) => {
            write_usage(&usage_ctx, "error", zero_usage(), Some(e.clone()));
            let _ = w.write_all(sse_error(&e).as_bytes()).await;
            let _ = w.flush().await;
        }
    }
}

fn zero_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cached_input_tokens: 0,
        cache_miss_input_tokens: 0,
        reasoning_output_tokens: 0,
        usage_json: String::new(),
    }
}

fn write_usage(ctx: &UsageContext, status: &str, tokens: TokenUsage, error: Option<String>) {
    let record = UsageRecord {
        request_id: ctx.request_id.clone(),
        session_id: ctx.session_id.clone(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        stream: ctx.stream,
        status: status.to_string(),
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        total_tokens: tokens.total_tokens,
        cached_input_tokens: tokens.cached_input_tokens,
        cache_miss_input_tokens: tokens.cache_miss_input_tokens,
        reasoning_output_tokens: tokens.reasoning_output_tokens,
        usage_json: tokens.usage_json,
        error,
        request_json: ctx.request_json.clone(),
        headers_json: ctx.headers_json.clone(),
    };
    if let Err(e) = usage::write_usage_record(&record) {
        eprintln!("[proxy] failed to write usage record: {e}");
    }
}

fn headers_json(headers: &[(String, String)]) -> String {
    let mut object = Map::new();
    for (name, value) in headers {
        if name == "authorization" || name == "api-key" || name == "x-api-key" {
            object.insert(name.clone(), Value::String("[redacted]".into()));
        } else {
            object.insert(name.clone(), Value::String(value.clone()));
        }
    }
    Value::Object(object).to_string()
}

fn sse_error(message: &str) -> String {
    let payload = json!({"type":"error","error":{"message":message}});
    format!("event: error\ndata: {payload}\n\n")
}

// ── low-level helpers ─────────────────────────────────────────────────

async fn upstream_fetch(base: &str, key: &str, body: &Value) -> Result<String, String> {
    let resp = upstream_send(base, key, body).await?;
    resp.text().await.map_err(|e| e.to_string())
}

async fn upstream_send(base: &str, key: &str, body: &Value) -> Result<reqwest::Response, String> {
    // If the base URL already contains a version prefix, don't add /v1.
    let base = base.trim_end_matches('/');
    let url = if base.ends_with("/v1") || base.contains("/v1/") || base.ends_with("/paas/v4") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    };
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.map_err(|e| e.to_string())?;
        return Err(format!("upstream HTTP {status}: {}", clip(&text, 800)));
    }
    Ok(resp)
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── smoke test: start proxy, send real HTTP, verify translated response ──

    #[tokio::test]
    async fn smoke_proxy_roundtrip() {
        // 1. Mock upstream: accept one connection, read HTTP request, return Chat SSE.
        let mock = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping TCP smoke test because sandbox denies bind: {e}");
                return;
            }
            Err(e) => panic!("mock bind failed: {e}"),
        };
        let mock_port = mock.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = mock.accept().await.unwrap();
            // Read the full HTTP request (simple approach: read until \r\n\r\n, then body).
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req_str = String::from_utf8_lossy(&buf[..n]);
            // Find body after \r\n\r\n
            let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
            let body = &buf[body_start..n];

            let req: Value = serde_json::from_slice(body).unwrap();
            assert!(req["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("hello"));

            // Write Chat SSE response on the same stream.
            let sse = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"t\",\
                \"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
                data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"t\",\
                \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
                \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\n\
                data: [DONE]\n\n";
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{sse}", sse.len());
            let _ = stream.write_all(resp.as_bytes()).await;
        });

        // Give the mock a moment to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 2. Start provider daemon pointed at the mock upstream.
        let cfg = ProxyConfig {
            provider_name: "test".into(),
            upstream_base: format!("http://127.0.0.1:{mock_port}"),
            api_key: "test-key".into(),
            model_slug: "test-model".into(),
        };
        let proxy_listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping TCP smoke test because sandbox denies bind: {e}");
                return;
            }
            Err(e) => panic!("proxy bind failed: {e}"),
        };
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        tokio::spawn(run_forever(proxy_listener, cfg));

        // 3. Send a Responses-API request to the proxy via raw HTTP.
        let req_body = json!({
            "model": "test-model",
            "input": [{"role": "user", "content": "say hello"}],
            "stream": true,
        })
        .to_string();
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}"))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        let http_req = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            req_body.len(), req_body
        );
        stream.write_all(http_req.as_bytes()).await.unwrap();

        // 4. Read response — keep reading until we have the full SSE stream.
        let mut buf = vec![0u8; 65536];
        let mut total = 0;
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf[total..])
                .await
                .unwrap();
            if n == 0 {
                break;
            }
            total += n;
            let so_far = String::from_utf8_lossy(&buf[..total]);
            if so_far.contains("response.completed") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let response = String::from_utf8_lossy(&buf[..total]);
        assert!(response.contains("HTTP/1.1 200"), "response:\n{response}");
        assert!(
            response.contains("response.created"),
            "missing response.created in:\n{response}"
        );
        assert!(response.contains("response.output_item.added"));
        assert!(response.contains("response.output_text.delta"));
        assert!(response.contains("\"delta\":\"hi\""));
        assert!(response.contains("response.completed"));
        assert!(response.contains("\"total_tokens\":6"));
        // Let background tasks settle.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn streaming_proxy_flushes_first_delta_before_upstream_finishes() {
        let mock = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping TCP streaming test because sandbox denies bind: {e}");
                return;
            }
            Err(e) => panic!("mock bind failed: {e}"),
        };
        let mock_port = mock.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = mock.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            stream.write_all(headers.as_bytes()).await.unwrap();

            let first = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"t\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
            let first_chunk = format!("{:x}\r\n{}\r\n", first.len(), first);
            stream.write_all(first_chunk.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;

            let done = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"t\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\ndata: [DONE]\n\n";
            let done_chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", done.len(), done);
            stream.write_all(done_chunk.as_bytes()).await.unwrap();
        });

        let cfg = ProxyConfig {
            provider_name: "test".into(),
            upstream_base: format!("http://127.0.0.1:{mock_port}"),
            api_key: "test-key".into(),
            model_slug: "test-model".into(),
        };
        let proxy_listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping TCP streaming test because sandbox denies bind: {e}");
                return;
            }
            Err(e) => panic!("proxy bind failed: {e}"),
        };
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        tokio::spawn(run_forever(proxy_listener, cfg));

        let req_body = json!({"model":"test-model","input":[{"role":"user","content":"say hello"}],"stream":true}).to_string();
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}"))
            .await
            .unwrap();
        let http_req = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            req_body.len(), req_body
        );
        stream.write_all(http_req.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 8192];
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
        )
        .await
        .expect("proxy did not flush first SSE response before upstream finished")
        .unwrap();
        let response = String::from_utf8_lossy(&buf[..read]);
        assert!(
            response.contains("response.output_text.delta"),
            "{response}"
        );
        assert!(response.contains("\"delta\":\"hi\""), "{response}");
        assert!(!response.contains("response.completed"), "{response}");
    }

    #[test]
    fn chat_completion_response_includes_nonstream_tool_calls() {
        let chat = json!({
            "id": "chatcmpl_1",
            "model": "deepseek-v4-pro",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_echo",
                        "type": "function",
                        "function": {
                            "name": "mcp__test_echo__echo",
                            "arguments": "{\"message\":\"hi\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "total_tokens": 13,
                "prompt_cache_hit_tokens": 4,
                "prompt_cache_miss_tokens": 6,
                "completion_tokens_details": {"reasoning_tokens": 2}
            }
        });

        let response = chat_completion_to_response(&chat, "deepseek-v4-pro", None);

        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["namespace"], "mcp__test_echo");
        assert_eq!(response["output"][0]["name"], "echo");
        assert_eq!(response["output"][0]["arguments"], "{\"message\":\"hi\"}");
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            4
        );
        assert_eq!(
            response["usage"]["input_tokens_details"]["cache_miss_tokens"],
            6
        );
        assert_eq!(
            response["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
    }
}
