use serde_json::json;
use serde_json::Value;

pub(super) fn responses_to_chat(req: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(|v| v.as_str()) {
        messages.push(json!({"role":"system","content":instructions}));
    }
    for item in req
        .get("input")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        messages.extend(input_item_to_chat_messages(item));
    }

    let mut chat = json!({"model":model,"messages":messages});
    for (from, to) in [
        ("max_output_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stream_options", "stream_options"),
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
    }
    if let Some(enabled) = reasoning_requested(req) {
        chat["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
    }
    chat
}

fn input_item_to_chat_messages(item: &Value) -> Vec<Value> {
    match item.get("type").and_then(|v| v.as_str()) {
        Some("function_call") => vec![assistant_tool_call_message(item, false)],
        Some("custom_tool_call") => vec![assistant_tool_call_message(item, true)],
        Some("function_call_output") | Some("custom_tool_call_output") => {
            vec![tool_output_message(item)]
        }
        _ => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = flatten_content(item.get("content").cloned().unwrap_or(Value::Null));
            vec![json!({"role":role,"content":content})]
        }
    }
}

fn assistant_tool_call_message(item: &Value, custom: bool) -> Value {
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
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        }]
    })
}

fn tool_output_message(item: &Value) -> Value {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": output_text(item.get("output").unwrap_or(&Value::Null))
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
    match &content {
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(|t| t.as_str()) {
                    Some("input_text" | "output_text" | "text") => {
                        p.get("text").and_then(|t| t.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                content
            } else {
                Value::String(text)
            }
        }
        _ => content,
    }
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
}
