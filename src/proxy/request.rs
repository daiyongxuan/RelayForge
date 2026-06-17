use serde_json::json;
use serde_json::Value;

pub(super) fn responses_to_chat(req: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(|v| v.as_str()) {
        messages.push(json!({"role":"system","content":instructions}));
    }

    let mut pending_reasoning: Option<String> = None;
    let mut pending_tool_calls: Vec<Value> = Vec::new();
    let mut last_assistant_index: Option<usize> = None;

    for item in req
        .get("input")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        append_item_as_chat_message(
            item,
            &mut messages,
            &mut pending_tool_calls,
            &mut pending_reasoning,
            &mut last_assistant_index,
        );
    }

    flush_pending_tool_calls(
        &mut messages,
        &mut pending_tool_calls,
        &mut pending_reasoning,
        &mut last_assistant_index,
    );

    backfill_tool_call_reasoning(&mut messages);

    let messages = collapse_system_messages(messages);

    let mut chat = json!({"model":model,"messages":messages});
    for (from, to) in [
        ("max_output_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = req.get(from) {
            chat[to] = value.clone();
        }
    }

    let tools = convert_tools(req.get("tools"));
    if !tools.is_empty() {
        chat["tools"] = Value::Array(tools);
        if let Some(tool_choice) = convert_tool_choice(req.get("tool_choice")) {
            chat["tool_choice"] = tool_choice;
        }
    }
    if let Some(stream) = req.get("stream") {
        chat["stream"] = stream.clone();
        let is_stream = stream.as_bool().unwrap_or(false);
        if is_stream {
            match chat.get_mut("stream_options") {
                Some(Value::Object(opts)) => {
                    opts.insert("include_usage".to_string(), json!(true));
                }
                _ => {
                    chat["stream_options"] = json!({"include_usage": true});
                }
            }
        }
    }
    if let Some(enabled) = reasoning_requested(req) {
        chat["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
    }
    chat
}

// ── item dispatch (stateful, per-item) ──────────────────────────────────

fn append_item_as_chat_message(
    item: &Value,
    messages: &mut Vec<Value>,
    pending_tool_calls: &mut Vec<Value>,
    pending_reasoning: &mut Option<String>,
    last_assistant_index: &mut Option<usize>,
) {
    match item.get("type").and_then(|v| v.as_str()) {
        Some("function_call") => {
            append_reasoning(pending_reasoning, extract_item_reasoning_text(item));
            pending_tool_calls.push(build_tool_call(item, false));
        }
        Some("custom_tool_call") => {
            append_reasoning(pending_reasoning, extract_item_reasoning_text(item));
            pending_tool_calls.push(build_tool_call(item, true));
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            flush_pending_tool_calls(
                messages,
                pending_tool_calls,
                pending_reasoning,
                last_assistant_index,
            );
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = output_text(item.get("output").unwrap_or(&Value::Null));
            messages.push(json!({"role":"tool","tool_call_id":call_id,"content":content}));
        }
        Some("reasoning") => {
            let text = extract_reasoning_summary_text(item);
            let attached = pending_tool_calls.is_empty()
                && attach_reasoning_to_last_assistant(messages, *last_assistant_index, text.as_deref());
            if !attached {
                append_reasoning(pending_reasoning, text);
            }
        }
        Some("message") | None => {
            flush_pending_tool_calls(
                messages,
                pending_tool_calls,
                pending_reasoning,
                last_assistant_index,
            );
            if item.get("role").is_some() || item.get("content").is_some() {
                let msg = build_chat_message(item, pending_reasoning);
                update_last_assistant_index(messages, &msg, last_assistant_index);
                messages.push(msg);
            }
        }
        _ => {
            flush_pending_tool_calls(
                messages,
                pending_tool_calls,
                pending_reasoning,
                last_assistant_index,
            );
            if item.get("role").is_some() || item.get("content").is_some() {
                let msg = build_chat_message(item, pending_reasoning);
                update_last_assistant_index(messages, &msg, last_assistant_index);
                messages.push(msg);
            }
        }
    }
}

// ── pending tool call flushing ──────────────────────────────────────────

fn flush_pending_tool_calls(
    messages: &mut Vec<Value>,
    pending_tool_calls: &mut Vec<Value>,
    pending_reasoning: &mut Option<String>,
    last_assistant_index: &mut Option<usize>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }
    let mut msg = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": std::mem::take(pending_tool_calls)
    });
    attach_reasoning_to_message(&mut msg, pending_reasoning);
    *last_assistant_index = Some(messages.len());
    messages.push(msg);
}

// ── per-tool-call assistant message backfill ────────────────────────────

fn backfill_tool_call_reasoning(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        let is_assistant_tool_call = msg.get("role").and_then(|v| v.as_str()) == Some("assistant")
            && msg
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .is_some_and(|calls| !calls.is_empty());
        if is_assistant_tool_call {
            let has_reasoning = msg
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty());
            if !has_reasoning {
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert(
                        "reasoning_content".to_string(),
                        Value::String("tool call".to_string()),
                    );
                }
            }
        }
    }
}

// ── collapse consecutive system messages to head ────────────────────────

fn collapse_system_messages(mut messages: Vec<Value>) -> Vec<Value> {
    let mut system_texts: Vec<String> = Vec::new();
    let mut others: Vec<Value> = Vec::new();
    let mut collecting = true;
    for msg in messages.drain(..) {
        if collecting && msg.get("role").and_then(|v| v.as_str()) == Some("system") {
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    system_texts.push(trimmed.to_string());
                }
            }
        } else {
            collecting = false;
            others.push(msg);
        }
    }
    if !system_texts.is_empty() {
        others.insert(0, json!({"role":"system","content":system_texts.join("\n\n")}));
    }
    others
}

// ── single chat message from Responses message item ─────────────────────

fn build_chat_message(item: &Value, pending_reasoning: &mut Option<String>) -> Value {
    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
    let chat_role = normalize_role(role);
    let content = flatten_content(item.get("content").cloned().unwrap_or(Value::Null));
    let mut msg = json!({"role":chat_role,"content":content});
    if chat_role == "assistant" {
        append_reasoning(pending_reasoning, extract_item_reasoning_text(item));
        attach_reasoning_to_message(&mut msg, pending_reasoning);
    } else {
        *pending_reasoning = None;
    }
    msg
}

// ── role normalization ──────────────────────────────────────────────────

fn normalize_role(role: &str) -> &str {
    match role {
        "system" | "developer" => "system",
        "assistant" => "assistant",
        "tool" => "tool",
        "user" | "latest_reminder" => "user",
        _ => "user",
    }
}

// ── reasoning state helpers ─────────────────────────────────────────────

fn extract_item_reasoning_text(item: &Value) -> Option<String> {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = item.get(key).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    if let Some(reasoning) = item.get("reasoning") {
        for key in ["content", "text", "summary"] {
            if let Some(text) = reasoning.get(key).and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

fn extract_reasoning_summary_text(item: &Value) -> Option<String> {
    for key in ["reasoning_content", "content", "text"] {
        if let Some(text) = item.get(key).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    let summary = item.get("summary")?;
    if let Some(text) = summary.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    let parts = summary.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(|v| v.as_str())
                .or_else(|| part.get("content").and_then(|v| v.as_str()))
                .or_else(|| part.as_str())
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn append_reasoning(pending: &mut Option<String>, extra: Option<String>) {
    let Some(extra) = extra.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return;
    };
    match pending {
        Some(existing) if !existing.is_empty() => {
            existing.push_str("\n\n");
            existing.push_str(&extra);
        }
        _ => *pending = Some(extra),
    }
}

fn attach_reasoning_to_message(msg: &mut Value, pending: &mut Option<String>) {
    let Some(reasoning) = pending.take().filter(|s| !s.trim().is_empty()) else {
        return;
    };
    if let Some(obj) = msg.as_object_mut() {
        if let Some(Value::String(existing)) = obj.get_mut("reasoning_content") {
            if !existing.is_empty() {
                existing.push_str("\n\n");
                existing.push_str(&reasoning);
                return;
            }
        }
        obj.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
}

fn attach_reasoning_to_last_assistant(
    messages: &mut [Value],
    last_assistant_index: Option<usize>,
    reasoning: Option<&str>,
) -> bool {
    let Some(reasoning) = reasoning.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let Some(index) = last_assistant_index else {
        return false;
    };
    let Some(msg) = messages.get_mut(index) else {
        return false;
    };
    if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return false;
    }
    if let Some(obj) = msg.as_object_mut() {
        match obj.get_mut("reasoning_content") {
            Some(Value::String(existing)) if !existing.is_empty() => {
                existing.push_str("\n\n");
                existing.push_str(reasoning);
            }
            _ => {
                obj.insert("reasoning_content".to_string(), Value::String(reasoning.to_string()));
            }
        }
        return true;
    }
    false
}

fn update_last_assistant_index(
    messages: &[Value],
    msg: &Value,
    last_assistant_index: &mut Option<usize>,
) {
    match msg.get("role").and_then(|v| v.as_str()) {
        Some("assistant") => *last_assistant_index = Some(messages.len()),
        Some("tool") => {}
        _ => *last_assistant_index = None,
    }
}

// ── tool call construction ──────────────────────────────────────────────

fn build_tool_call(item: &Value, custom: bool) -> Value {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = chat_tool_name(
        item.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        item.get("namespace").and_then(|v| v.as_str()),
    );
    let arguments = if custom {
        json!({"input": item.get("input").cloned().unwrap_or_else(|| json!(""))}).to_string()
    } else {
        match item.get("arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(value) => value.to_string(),
            None => "{}".to_string(),
        }
    };
    json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    })
}

fn output_text(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

// ── tool definition conversion ──────────────────────────────────────────

fn convert_tools(tools: Option<&Value>) -> Vec<Value> {
    tools
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .flat_map(convert_tool)
        .collect()
}

fn convert_tool(tool: &Value) -> Vec<Value> {
    match tool.get("type").and_then(|v| v.as_str()) {
        Some("function") => convert_function_tool(tool, None).into_iter().collect(),
        Some("custom") => convert_custom_tool(tool).into_iter().collect(),
        Some("tool_search") => vec![json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description": "Search and load tools for the current task.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["query"]
                }
            }
        })],
        Some("namespace") => convert_namespace_tool(tool),
        _ => Vec::new(),
    }
}

fn convert_namespace_tool(tool: &Value) -> Vec<Value> {
    let Some(namespace) = tool.get("name").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    tool.get("tools")
        .or_else(|| tool.get("children"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|child| convert_function_tool(child, Some(namespace)))
        .collect()
}

fn convert_function_tool(tool: &Value, namespace: Option<&str>) -> Option<Value> {
    let name = tool_name(tool)?;
    let chat_name = chat_tool_name(&name, namespace);
    let mut function = tool.get("function").cloned().unwrap_or_else(|| {
        json!({
            "name": chat_name,
            "description": tool.get("description").cloned().unwrap_or(Value::Null),
            "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({}))
        })
    });
    let obj = function.as_object_mut()?;
    obj.insert("name".to_string(), Value::String(chat_name));
    if let Some(strict) = tool.get("strict").cloned() {
        obj.entry("strict".to_string()).or_insert(strict);
    }
    Some(json!({"type":"function","function":function}))
}

fn convert_custom_tool(tool: &Value) -> Option<Value> {
    let name = tool_name(tool)?;
    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!("Original custom tool definition: {}", tool),
            "parameters": {
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "Raw string input for the original custom tool."
                    }
                },
                "required": ["input"]
            }
        }
    }))
}

fn convert_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    match tool_choice {
        Some(Value::Object(obj))
            if obj.get("type").and_then(|v| v.as_str()) == Some("function") =>
        {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({"type":"function","function":{"name":name}}))
        }
        Some(Value::Object(obj)) if obj.get("type").and_then(|v| v.as_str()) == Some("custom") => {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({"type":"function","function":{"name":name}}))
        }
        Some(Value::Object(obj))
            if obj.get("type").and_then(|v| v.as_str()) == Some("tool_search") =>
        {
            Some(json!({"type":"function","function":{"name":"tool_search"}}))
        }
        Some(Value::String(choice)) => Some(Value::String(choice.clone())),
        _ => None,
    }
}

fn tool_name(tool: &Value) -> Option<String> {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn chat_tool_name(name: &str, namespace: Option<&str>) -> String {
    namespace
        .map(|namespace| format!("{namespace}__{name}"))
        .unwrap_or_else(|| name.to_string())
}

fn reasoning_requested(req: &Value) -> Option<bool> {
    if let Some(effort) = req.pointer("/reasoning/effort").and_then(|v| v.as_str()) {
        return Some(!matches!(
            effort.trim().to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        ));
    }
    req.get("reasoning").map(|v| !v.is_null())
}

fn flatten_content(content: Value) -> Value {
    if content.is_null() {
        return Value::String(String::new());
    }
    if content.is_string() {
        return content;
    }
    let Some(parts) = content.as_array() else {
        return content;
    };
    let mut chat_parts: Vec<Value> = Vec::new();
    let mut has_non_text = false;
    for part in parts {
        match part.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "input_text" | "output_text" | "text" => {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        chat_parts.push(json!({"type":"text","text":text}));
                    }
                }
            }
            "refusal" => {
                if let Some(text) = part.get("refusal").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        chat_parts.push(json!({"type":"text","text":text}));
                    }
                }
            }
            "input_image" => {
                if let Some(image_url) = part.get("image_url") {
                    let image_url = if image_url.is_object() {
                        image_url.clone()
                    } else {
                        json!({"url": image_url.as_str().unwrap_or_default()})
                    };
                    chat_parts.push(json!({"type":"image_url","image_url":image_url}));
                    has_non_text = true;
                }
            }
            "input_file" => {
                let mut file = serde_json::Map::new();
                for key in ["file_id", "file_data", "filename"] {
                    if let Some(value) = part.get(key) {
                        file.insert(key.to_string(), value.clone());
                    }
                }
                if !file.is_empty() {
                    chat_parts.push(json!({"type":"file","file":file}));
                    has_non_text = true;
                }
            }
            "input_audio" => {
                if let Some(input_audio) = part.get("input_audio") {
                    chat_parts.push(json!({"type":"input_audio","input_audio":input_audio.clone()}));
                    has_non_text = true;
                }
            }
            _ => {}
        }
    }
    if !has_non_text {
        return Value::String(
            chat_parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Value::Array(chat_parts)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn responses_to_chat_basic() {
        let req = json!({
            "model": "glm-5.2",
            "input": [{"role": "user", "content": "hello"}],
            "stream": true,
            "max_output_tokens": 4096,
        });
        let chat = responses_to_chat(&req, "glm-5.2");
        assert_eq!(chat["model"], "glm-5.2");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hello");
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["max_tokens"], 4096);
    }

    #[test]
    fn reasoning_item_attached_to_next_assistant() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {"role": "user", "content": "solve 2+2"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "2+2=4"}]},
                {"type": "message", "role": "assistant", "content": "4"},
                {"role": "user", "content": "and 3+3?"}
            ],
        });
        let chat = responses_to_chat(&req, "deepseek-v4-pro");
        let msgs = chat["messages"].as_array().unwrap();
        let assistant = &msgs[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "2+2=4");
        let user2 = &msgs[2];
        assert_eq!(user2["role"], "user");
        assert_eq!(user2["content"], "and 3+3?");
    }

    #[test]
    fn developer_role_maps_to_system() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {"role": "developer", "content": "be helpful"},
                {"role": "user", "content": "hi"}
            ],
        });
        let chat = responses_to_chat(&req, "deepseek-v4-pro");
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be helpful");
    }

    #[test]
    fn null_content_becomes_empty_string() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {"role": "user"},
                {"role": "user", "content": "hi"}
            ],
        });
        let chat = responses_to_chat(&req, "deepseek-v4-pro");
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], "");
        assert_eq!(msgs[1]["content"], "hi");
    }
}
