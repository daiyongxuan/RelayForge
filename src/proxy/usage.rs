use std::path::PathBuf;

use rusqlite::params;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_miss_input_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub usage_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UsageRecord {
    pub request_id: String,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub stream: bool,
    pub status: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_miss_input_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub usage_json: String,
    pub error: Option<String>,
    pub request_json: String,
    pub headers_json: String,
}

pub(super) fn usage_db_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
        .join(".codex")
        .join("codex-proxy-requests.sqlite")
}

pub(super) fn write_usage_record(record: &UsageRecord) -> Result<(), String> {
    write_usage_record_to(&usage_db_path(), record)
}

fn write_usage_record_to(path: &std::path::Path, record: &UsageRecord) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create usage db dir: {e}"))?;
    }
    let conn = rusqlite::Connection::open(path).map_err(|e| format!("open usage db: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS proxy_requests (
            request_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            stream INTEGER NOT NULL,
            status TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            cached_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_miss_input_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
            usage_json TEXT NOT NULL DEFAULT '',
            error TEXT,
            request_json TEXT NOT NULL DEFAULT '',
            headers_json TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_proxy_requests_session ON proxy_requests(session_id);
        CREATE INDEX IF NOT EXISTS idx_proxy_requests_provider_model ON proxy_requests(provider, model);
        CREATE INDEX IF NOT EXISTS idx_proxy_requests_created_at ON proxy_requests(created_at);",
    )
    .map_err(|e| format!("init usage db: {e}"))?;
    ensure_column(&conn, "request_json", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(&conn, "headers_json", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(&conn, "cached_input_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(
        &conn,
        "cache_miss_input_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        &conn,
        "reasoning_output_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(&conn, "usage_json", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute(
        "INSERT OR REPLACE INTO proxy_requests
         (request_id, session_id, provider, model, stream, status, input_tokens, output_tokens, total_tokens,
          cached_input_tokens, cache_miss_input_tokens, reasoning_output_tokens, usage_json, error, request_json, headers_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            record.request_id,
            record.session_id,
            record.provider,
            record.model,
            if record.stream { 1 } else { 0 },
            record.status,
            record.input_tokens,
            record.output_tokens,
            record.total_tokens,
            record.cached_input_tokens,
            record.cache_miss_input_tokens,
            record.reasoning_output_tokens,
            record.usage_json,
            record.error,
            record.request_json,
            record.headers_json,
        ],
    )
    .map_err(|e| format!("insert usage record: {e}"))?;
    Ok(())
}

fn ensure_column(conn: &rusqlite::Connection, name: &str, definition: &str) -> Result<(), String> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(proxy_requests)")
        .map_err(|e| format!("inspect usage db columns: {e}"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("read usage db columns: {e}"))?
        .filter_map(Result::ok)
        .any(|column| column == name);
    if !exists {
        conn.execute(
            &format!("ALTER TABLE proxy_requests ADD COLUMN {name} {definition}"),
            [],
        )
        .map_err(|e| format!("migrate usage db column {name}: {e}"))?;
    }
    Ok(())
}

pub(super) fn extract_session_id(req: &serde_json::Value) -> String {
    for pointer in [
        "/metadata/session_id",
        "/metadata/codex_session_id",
        "/metadata/thread_id",
        "/metadata/conversation_id",
        "/metadata/trace_id",
    ] {
        if let Some(session_id) = req
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            return session_id.to_string();
        }
    }
    "unknown".to_string()
}

pub(super) fn extract_session_id_from_headers(headers: &[(String, String)]) -> Option<String> {
    for name in ["session-id", "thread-id", "x-client-request-id"] {
        if let Some(value) = header_value(headers, name).filter(|s| !s.trim().is_empty()) {
            return Some(value.to_string());
        }
    }
    let metadata = header_value(headers, "x-codex-turn-metadata")?;
    let value = serde_json::from_str::<serde_json::Value>(metadata).ok()?;
    for pointer in ["/session_id", "/thread_id", "/turn_id"] {
        if let Some(session_id) = value
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            return Some(session_id.to_string());
        }
    }
    None
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub(super) fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "req_{:x}_{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn usage_from_chat(value: &serde_json::Value) -> TokenUsage {
    usage_from_chat_usage(value.get("usage"))
}

pub(super) fn usage_from_chat_usage(usage: Option<&serde_json::Value>) -> TokenUsage {
    let input_tokens = usage.and_then(|u| u["prompt_tokens"].as_i64()).unwrap_or(0);
    let cached_input_tokens = usage
        .and_then(|u| u["prompt_cache_hit_tokens"].as_i64())
        .or_else(|| usage.and_then(|u| u["prompt_tokens_details"]["cached_tokens"].as_i64()))
        .unwrap_or(0);
    TokenUsage {
        input_tokens,
        output_tokens: usage
            .and_then(|u| u["completion_tokens"].as_i64())
            .unwrap_or(0),
        total_tokens: usage.and_then(|u| u["total_tokens"].as_i64()).unwrap_or(0),
        cached_input_tokens,
        cache_miss_input_tokens: usage
            .and_then(|u| u["prompt_cache_miss_tokens"].as_i64())
            .unwrap_or_else(|| input_tokens.saturating_sub(cached_input_tokens)),
        reasoning_output_tokens: usage
            .and_then(|u| u["completion_tokens_details"]["reasoning_tokens"].as_i64())
            .unwrap_or(0),
        usage_json: usage.map_or_else(String::new, serde_json::Value::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_session_id_from_headers() {
        let headers = vec![("session-id".into(), "session-header".into())];
        assert_eq!(
            extract_session_id_from_headers(&headers),
            Some("session-header".into())
        );

        let headers = vec![(
            "x-codex-turn-metadata".into(),
            r#"{"session_id":"session-meta","thread_id":"thread-meta"}"#.into(),
        )];
        assert_eq!(
            extract_session_id_from_headers(&headers),
            Some("session-meta".into())
        );
    }

    #[test]
    fn writes_usage_record_with_provider_and_model() {
        let path = std::env::temp_dir().join(format!(
            "codex-proxy-usage-test-{}-{}.sqlite",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        let _ = std::fs::remove_file(&path);
        let record = UsageRecord {
            request_id: "req_1".into(),
            session_id: "session_a".into(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            stream: true,
            status: "completed".into(),
            input_tokens: 10,
            output_tokens: 3,
            total_tokens: 13,
            cached_input_tokens: 4,
            cache_miss_input_tokens: 6,
            reasoning_output_tokens: 2,
            usage_json: r#"{"prompt_cache_hit_tokens":4}"#.into(),
            error: None,
            request_json: "{\"input\":[]}".into(),
            headers_json: "{\"x-test\":\"1\"}".into(),
        };
        write_usage_record_to(&path, &record).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let row = conn
            .query_row(
                "SELECT session_id, provider, model, stream, input_tokens, output_tokens, total_tokens, cached_input_tokens, cache_miss_input_tokens, reasoning_output_tokens, usage_json, request_json, headers_json FROM proxy_requests WHERE request_id='req_1'",
                [],
                |row| {
                    Ok(json!({
                        "session_id": row.get::<_, String>(0)?,
                        "provider": row.get::<_, String>(1)?,
                        "model": row.get::<_, String>(2)?,
                        "stream": row.get::<_, i64>(3)?,
                        "input_tokens": row.get::<_, i64>(4)?,
                        "output_tokens": row.get::<_, i64>(5)?,
                        "total_tokens": row.get::<_, i64>(6)?,
                        "cached_input_tokens": row.get::<_, i64>(7)?,
                        "cache_miss_input_tokens": row.get::<_, i64>(8)?,
                        "reasoning_output_tokens": row.get::<_, i64>(9)?,
                        "usage_json": row.get::<_, String>(10)?,
                        "request_json": row.get::<_, String>(11)?,
                        "headers_json": row.get::<_, String>(12)?,
                    }))
                },
            )
            .unwrap();
        assert_eq!(row["session_id"], "session_a");
        assert_eq!(row["provider"], "deepseek");
        assert_eq!(row["model"], "deepseek-v4-pro");
        assert_eq!(row["stream"], 1);
        assert_eq!(row["input_tokens"], 10);
        assert_eq!(row["output_tokens"], 3);
        assert_eq!(row["total_tokens"], 13);
        assert_eq!(row["cached_input_tokens"], 4);
        assert_eq!(row["cache_miss_input_tokens"], 6);
        assert_eq!(row["reasoning_output_tokens"], 2);
        assert_eq!(row["usage_json"], r#"{"prompt_cache_hit_tokens":4}"#);
        assert_eq!(row["request_json"], "{\"input\":[]}");
        assert_eq!(row["headers_json"], "{\"x-test\":\"1\"}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extracts_cache_and_reasoning_usage_details() {
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 64,
            "prompt_cache_miss_tokens": 36,
            "prompt_tokens_details": {"cached_tokens": 64},
            "completion_tokens_details": {"reasoning_tokens": 15}
        });

        let tokens = usage_from_chat_usage(Some(&usage));

        assert_eq!(tokens.input_tokens, 100);
        assert_eq!(tokens.output_tokens, 20);
        assert_eq!(tokens.total_tokens, 120);
        assert_eq!(tokens.cached_input_tokens, 64);
        assert_eq!(tokens.cache_miss_input_tokens, 36);
        assert_eq!(tokens.reasoning_output_tokens, 15);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&tokens.usage_json).unwrap(),
            usage
        );
    }
}
