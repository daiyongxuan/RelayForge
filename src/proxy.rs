//! Provider daemon: receives Codex Responses-API, translates to upstream Chat
//! Completions, and translates SSE back to Responses events.

use serde_json::json;
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

use request::responses_to_chat;
use stream::chat_sse_to_responses_events;

#[derive(Clone)]
pub struct ProxyConfig {
    pub upstream_base: String,
    pub api_key: String,
    pub model_slug: String,
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
    let (method, path, body) = match read_request(&mut reader).await {
        Some(r) => r,
        None => return,
    };

    match (method.as_str(), path.as_str()) {
        ("POST", path) if is_responses_path(path) => {
            serve_responses(&mut writer, &body, &cfg).await
        }
        _ => write_404(&mut writer).await,
    }
}

fn is_responses_path(path: &str) -> bool {
    path == "/v1/responses" || path == "/responses"
}

async fn read_request(
    reader: &mut BufReader<impl AsyncReadExt + Unpin>,
) -> Option<(String, String, Vec<u8>)> {
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.trim().split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    loop {
        line.clear();
        reader.read_line(&mut line).await.ok()?;
        let t = line.trim().to_ascii_lowercase();
        if t.is_empty() {
            break;
        }
        if let Some(val) = t.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }
    Some((method, path, body))
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

async fn serve_responses(w: &mut (impl AsyncWriteExt + Unpin), body: &[u8], cfg: &ProxyConfig) {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            write_json(w, 400, "{\"error\":\"bad json\"}").await;
            return;
        }
    };
    let input_count = req.get("input").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let user_msgs = req.get("input")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter(|i| i.get("role").and_then(|v| v.as_str()) == Some("user")).count())
        .unwrap_or(0);
    let is_stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    eprintln!("[proxy] req input_items={input_count} user_turns={user_msgs} stream={is_stream}");
    let chat = responses_to_chat(&req, &cfg.model_slug);

    if is_stream {
        stream_sse(w, &chat, &cfg).await;
    } else {
        nonstream(w, &chat, &cfg).await;
    }
}

// ── non-streaming ─────────────────────────────────────────────────────

async fn nonstream(w: &mut (impl AsyncWriteExt + Unpin), body: &Value, cfg: &ProxyConfig) {
    let text = match upstream_fetch(&cfg.upstream_base, &cfg.api_key, body).await {
        Ok(t) => t,
        Err(e) => {
            write_json(w, 502, &json!({"error":e}).to_string()).await;
            return;
        }
    };
    let cr: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            write_json(w, 502, "{\"error\":\"bad upstream\"}").await;
            return;
        }
    };

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
    let result = json!({"id":cr["id"],"object":"response","model":cr["model"],"status":"completed",
        "output":output,"usage":{"input_tokens":usage["prompt_tokens"],"output_tokens":usage["completion_tokens"],
        "total_tokens":usage["total_tokens"]}});
    write_json(w, 200, &result.to_string()).await;
}

// ── streaming SSE ─────────────────────────────────────────────────────

async fn stream_sse(w: &mut (impl AsyncWriteExt + Unpin), body: &Value, cfg: &ProxyConfig) {
    let bytes = match upstream_fetch(&cfg.upstream_base, &cfg.api_key, body).await {
        Ok(b) => b,
        Err(e) => {
            write_json(w, 502, &json!({"error":e}).to_string()).await;
            return;
        }
    };

    let (id, model) = ("resp_1", body["model"].as_str().unwrap_or(""));
    let events = match chat_sse_to_responses_events(id, model, &bytes) {
        Ok(events) => events,
        Err(e) => {
            write_json(w, 502, &json!({"error":e}).to_string()).await;
            return;
        }
    };
    let _ = w
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
        .await;
    for evt in events {
        let _ = w.write_all(evt.as_bytes()).await;
    }
}

// ── low-level helpers ─────────────────────────────────────────────────

async fn upstream_fetch(base: &str, key: &str, body: &Value) -> Result<String, String> {
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
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("upstream HTTP {status}: {}", clip(&text, 800)));
    }
    Ok(text)
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
}
