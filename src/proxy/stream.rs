use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde_json::json;
use serde_json::Value;

use super::usage::TokenUsage;

#[cfg(test)]
pub(super) fn chat_sse_to_responses_events(
    id: &str,
    model: &str,
    text: &str,
) -> Result<Vec<String>, String> {
    chat_sse_to_responses_events_with_tools(id, model, text, None)
}

#[cfg(test)]
pub(super) fn chat_sse_to_responses_events_with_tools(
    id: &str,
    model: &str,
    text: &str,
    tools: Option<&Value>,
) -> Result<Vec<String>, String> {
    let mut converter = ChatSseConverter::new(id, model, tools);
    let mut events = converter.initial_events();

    for block in sse_blocks(text) {
        events.extend(converter.push_block(block)?);
        if converter.is_finished() {
            return Ok(events);
        }
    }
    events.extend(converter.finish_without_done()?);
    Ok(events)
}

pub(super) struct ChatSseConverter<'a> {
    state: SseState<'a>,
    saw_valid_chunk: bool,
}

impl<'a> ChatSseConverter<'a> {
    pub(super) fn new(id: &'a str, model: &'a str, tools: Option<&Value>) -> Self {
        Self {
            state: SseState {
                id,
                model,
                custom_tools: custom_tool_names(tools),
                ..Default::default()
            },
            saw_valid_chunk: false,
        }
    }

    pub(super) fn initial_events(&self) -> Vec<String> {
        vec![
            sse(
                "response.created",
                &json!({"response": base_resp(self.state.id, self.state.model, "in_progress", &[])}),
            ),
            sse(
                "response.in_progress",
                &json!({"response": base_resp(self.state.id, self.state.model, "in_progress", &[])}),
            ),
        ]
    }

    pub(super) fn push_text(&mut self, text: &mut String) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        while let Some(block) = pop_sse_block(text) {
            events.extend(self.push_block(block)?);
            if self.is_finished() {
                break;
            }
        }
        Ok(events)
    }

    pub(super) fn finish_without_done(&mut self) -> Result<Vec<String>, String> {
        if self.state.finished {
            return Ok(Vec::new());
        }
        if !self.saw_valid_chunk {
            return Err("upstream stream contained no valid chat completion chunks".into());
        }
        if !self.state.has_substantive_output() {
            return Err("upstream stream completed without assistant output".into());
        }
        Ok(self.state.finalize(&json!({})))
    }

    pub(super) fn is_finished(&self) -> bool {
        self.state.finished
    }

    pub(super) fn usage(&self) -> Option<TokenUsage> {
        self.state.usage.clone()
    }

    fn push_block(&mut self, block: SseBlock) -> Result<Vec<String>, String> {
        let event_name = block.event.as_deref();
        let data = block.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| format!("invalid chat SSE JSON: {e}; data={}", clip(data, 400)))?;
        if event_name == Some("error") || meaningful_error(&chunk) {
            return Err(chat_sse_error_message(&chunk));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
        else {
            return Ok(Vec::new());
        };

        self.saw_valid_chunk = true;
        let mut events = self
            .state
            .push_delta(choice.get("delta").unwrap_or(&Value::Null));
        if choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .is_some()
        {
            if !self.state.has_substantive_output() {
                return Err("upstream stream completed without assistant output".into());
            }
            events.extend(self.state.finalize(&chunk));
        }
        Ok(events)
    }
}

fn pop_sse_block(text: &mut String) -> Option<SseBlock> {
    let normalized = text.replace("\r\n", "\n");
    let split = normalized.find("\n\n")?;
    let raw = normalized[..split].to_string();
    let rest = normalized[split + 2..].to_string();
    *text = rest;
    parse_sse_block(&raw)
}

fn custom_tool_names(tools: Option<&Value>) -> BTreeSet<String> {
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

struct SseBlock {
    event: Option<String>,
    data: String,
}

#[cfg(test)]
fn sse_blocks(text: &str) -> Vec<SseBlock> {
    text.replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(parse_sse_block)
        .collect()
}

fn parse_sse_block(raw: &str) -> Option<SseBlock> {
    let mut event = None;
    let mut data = Vec::new();
    for line in raw.lines().map(str::trim_end) {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_string());
        }
    }
    (!data.is_empty()).then(|| SseBlock {
        event,
        data: data.join("\n"),
    })
}

fn meaningful_error(chunk: &Value) -> bool {
    match chunk.get("error") {
        Some(Value::Null) | None => false,
        Some(Value::Object(obj)) => !obj.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn chat_sse_error_message(chunk: &Value) -> String {
    let error = chunk.get("error").unwrap_or(chunk);
    let message = error
        .get("message")
        .or_else(|| error.get("msg"))
        .or_else(|| error.get("detail"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| error.as_str().unwrap_or("upstream chat SSE error"));
    let kind = error
        .get("type")
        .or_else(|| error.get("code"))
        .map(|v| v.to_string());
    match kind {
        Some(kind) => format!("{message} ({kind})"),
        None => message.to_string(),
    }
}

#[derive(Default)]
struct SseState<'a> {
    id: &'a str,
    model: &'a str,
    custom_tools: BTreeSet<String>,
    output_index: usize,
    reasoning: TextStage,
    text: TextStage,
    tools: BTreeMap<usize, ToolStage>,
    finished: bool,
    usage: Option<TokenUsage>,
}

#[derive(Default)]
struct TextStage {
    added: bool,
    item_id: String,
    text: String,
    output_index: usize,
}

#[derive(Default, Clone)]
struct ToolStage {
    added: bool,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
}

impl SseState<'_> {
    fn push_delta(&mut self, delta: &Value) -> Vec<String> {
        let mut events = Vec::new();
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if !self.reasoning.added {
                events.extend(self.add_reasoning());
            }
            self.reasoning.text.push_str(reasoning);
            events.push(sse(
                "response.reasoning_summary_text.delta",
                &json!({
                    "item_id": self.reasoning.item_id,
                    "output_index": self.reasoning.output_index,
                    "summary_index": 0,
                    "delta": reasoning
                }),
            ));
        }
        if let Some(content) = delta
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if !self.text.added {
                events.extend(self.add_text());
            }
            self.text.text.push_str(content);
            events.push(sse(
                "response.output_text.delta",
                &json!({
                    "item_id": self.text.item_id,
                    "output_index": self.text.output_index,
                    "content_index": 0,
                    "delta": content
                }),
            ));
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            let reasoning = self.current_reasoning_text();
            for tool_call in tool_calls {
                events.extend(self.push_tool_call_delta(tool_call, reasoning.as_deref()));
            }
        }
        events
    }

    fn add_reasoning(&mut self) -> Vec<String> {
        let output_index = self.alloc_index();
        self.reasoning.output_index = output_index;
        self.reasoning.item_id = format!("rs_{}", self.id);
        self.reasoning.added = true;
        vec![
            sse(
                "response.output_item.added",
                &json!({
                    "output_index": output_index,
                    "item": {
                        "id": self.reasoning.item_id,
                        "type": "reasoning",
                        "status": "in_progress",
                        "summary": []
                    }
                }),
            ),
            sse(
                "response.reasoning_summary_part.added",
                &json!({
                    "item_id": self.reasoning.item_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": {"type":"summary_text","text":""}
                }),
            ),
        ]
    }

    fn add_text(&mut self) -> Vec<String> {
        let output_index = self.alloc_index();
        self.text.output_index = output_index;
        self.text.item_id = format!("{}_msg", self.id);
        self.text.added = true;
        vec![
            sse(
                "response.output_item.added",
                &json!({
                    "output_index": output_index,
                    "item": {
                        "id": self.text.item_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }
                }),
            ),
            sse(
                "response.content_part.added",
                &json!({
                    "item_id": self.text.item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {"type":"output_text","text":"","annotations":[]}
                }),
            ),
        ]
    }

    fn push_tool_call_delta(&mut self, tool_call: &Value, reasoning: Option<&str>) -> Vec<String> {
        let index = tool_call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let function = tool_call.get("function").unwrap_or(&Value::Null);
        let id = tool_call.get("id").and_then(|v| v.as_str());
        let name = function.get("name").and_then(|v| v.as_str());
        let arguments = function
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let (should_add, pending_arguments): (bool, String) = {
            let stage = self.tools.entry(index).or_default();
            if let Some(id) = id {
                stage.call_id = id.to_string();
            }
            if let Some(name) = name {
                stage.name = name.to_string();
            }
            stage.arguments.push_str(arguments);
            let added = !stage.added && (!stage.call_id.is_empty() || !stage.name.is_empty());
            (added, stage.arguments.clone())
        };
        let mut events = Vec::new();
        if should_add {
            let output_index = self.alloc_index();
            let stage = self.tools.entry(index).or_default();
            if stage.call_id.is_empty() {
                stage.call_id = format!("call_{index}");
            }
            if stage.name.is_empty() {
                stage.name = "unknown_tool".to_string();
            }
            stage.output_index = output_index;
            stage.item_id = format!("fc_{}", stage.call_id);
            stage.added = true;
            let mut item = function_call_item(stage, "in_progress", None, &self.custom_tools);
            if let Some(reasoning) = reasoning {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert(
                        "reasoning_content".to_string(),
                        Value::String(reasoning.to_string()),
                    );
                }
            }
            events.push(sse(
                "response.output_item.added",
                &json!({"output_index": output_index, "item": item}),
            ));
            if !pending_arguments.is_empty() {
                events.push(sse(
                    "response.function_call_arguments.delta",
                    &json!({
                        "item_id": stage.item_id,
                        "output_index": output_index,
                        "delta": pending_arguments
                    }),
                ));
            }
        } else if !arguments.is_empty() {
            if let Some(stage) = self.tools.get(&index).filter(|stage| stage.added) {
                events.push(sse(
                    "response.function_call_arguments.delta",
                    &json!({
                        "item_id": stage.item_id,
                        "output_index": stage.output_index,
                        "delta": arguments
                    }),
                ));
            }
        }
        events
    }

    fn finalize(&mut self, chunk: &Value) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut events = Vec::new();
        let token_usage = super::usage::usage_from_chat_usage(chunk.get("usage"));
        let input_tokens = token_usage.input_tokens;
        let output_tokens = token_usage.output_tokens;
        let total_tokens = token_usage.total_tokens;
        self.usage = Some(token_usage.clone());
        eprintln!("[proxy] usage input={input_tokens} output={output_tokens} total={total_tokens}");
        events.extend(self.finalize_reasoning());
        events.extend(self.finalize_text());
        events.extend(self.finalize_tools());
        events.push(sse(
            "response.completed",
            &json!({"response":{
                "id": self.id,
                "object": "response",
                "model": self.model,
                "status": "completed",
                "output": self.completed_items(),
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "total_tokens": total_tokens,
                    "input_tokens_details": {
                        "cached_tokens": token_usage.cached_input_tokens,
                        "cache_miss_tokens": token_usage.cache_miss_input_tokens
                    },
                    "output_tokens_details": {
                        "reasoning_tokens": token_usage.reasoning_output_tokens
                    }
                }
            }}),
        ));
        events
    }

    fn finalize_reasoning(&self) -> Vec<String> {
        if !self.reasoning.added {
            return Vec::new();
        }
        vec![
            sse(
                "response.reasoning_summary_text.done",
                &json!({
                    "item_id": self.reasoning.item_id,
                    "output_index": self.reasoning.output_index,
                    "summary_index": 0,
                    "text": self.reasoning.text
                }),
            ),
            sse(
                "response.reasoning_summary_part.done",
                &json!({
                    "item_id": self.reasoning.item_id,
                    "output_index": self.reasoning.output_index,
                    "summary_index": 0,
                    "part": {"type":"summary_text","text":self.reasoning.text}
                }),
            ),
            sse(
                "response.output_item.done",
                &json!({
                    "output_index": self.reasoning.output_index,
                    "item": {
                        "id": self.reasoning.item_id,
                        "type": "reasoning",
                        "status": "completed",
                        "summary": [{"type":"summary_text","text":self.reasoning.text}]
                    }
                }),
            ),
        ]
    }

    fn finalize_text(&self) -> Vec<String> {
        if !self.text.added {
            return Vec::new();
        }
        let item = json!({
            "id": self.text.item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type":"output_text","text":self.text.text,"annotations":[]}]
        });
        vec![
            sse(
                "response.output_text.done",
                &json!({
                    "item_id": self.text.item_id,
                    "output_index": self.text.output_index,
                    "content_index": 0,
                    "text": self.text.text
                }),
            ),
            sse(
                "response.content_part.done",
                &json!({
                    "item_id": self.text.item_id,
                    "output_index": self.text.output_index,
                    "content_index": 0,
                    "part": {"type":"output_text","text":self.text.text,"annotations":[]}
                }),
            ),
            sse(
                "response.output_item.done",
                &json!({"output_index": self.text.output_index, "item": item}),
            ),
        ]
    }

    fn finalize_tools(&self) -> Vec<String> {
        self.tools
            .values()
            .map(|stage| {
                sse(
                    "response.output_item.done",
                    &json!({
                        "output_index": stage.output_index,
                        "item": function_call_item(stage, "completed", None, &self.custom_tools)
                    }),
                )
            })
            .collect()
    }

    fn completed_items(&self) -> Vec<Value> {
        let mut items = Vec::new();
        if self.reasoning.added {
            items.push(json!({
                "id": self.reasoning.item_id,
                "type": "reasoning",
                "summary": [{"type":"summary_text","text":self.reasoning.text}]
            }));
        }
        if self.text.added {
            items.push(json!({
                "id": self.text.item_id,
                "type": "message",
                "role": "assistant",
                "content": [{"type":"output_text","text":self.text.text,"annotations":[]}]
            }));
        }
        items.extend(
            self.tools
                .values()
                .map(|stage| function_call_item(stage, "completed", None, &self.custom_tools)),
        );
        items
    }

    fn alloc_index(&mut self) -> usize {
        let i = self.output_index;
        self.output_index += 1;
        i
    }

    fn current_reasoning_text(&self) -> Option<String> {
        (!self.reasoning.text.trim().is_empty()).then(|| self.reasoning.text.trim().to_string())
    }

    fn has_substantive_output(&self) -> bool {
        self.reasoning.added || self.text.added || self.tools.values().any(|stage| stage.added)
    }
}

fn function_call_item(
    stage: &ToolStage,
    status: &str,
    reasoning: Option<&str>,
    custom_tools: &BTreeSet<String>,
) -> Value {
    if stage.name == "tool_search" {
        let mut item = json!({
            "type": "tool_search_call",
            "call_id": stage.call_id,
            "status": status,
            "execution": "client",
            "arguments": parse_arguments_object(&stage.arguments)
        });
        attach_reasoning(&mut item, reasoning);
        return item;
    }
    if custom_tools.contains(&stage.name) {
        let mut item = json!({
            "id": stage.item_id,
            "type": "custom_tool_call",
            "status": status,
            "call_id": stage.call_id,
            "name": stage.name,
            "input": custom_tool_input(&stage.arguments)
        });
        attach_reasoning(&mut item, reasoning);
        return item;
    }
    let (namespace, name) = split_chat_tool_name(&stage.name);
    let mut item = json!({
        "id": stage.item_id,
        "type": "function_call",
        "status": status,
        "call_id": stage.call_id,
        "name": name,
        "arguments": stage.arguments
    });
    if let Some(namespace) = namespace {
        item["namespace"] = Value::String(namespace);
    }
    attach_reasoning(&mut item, reasoning);
    item
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

fn attach_reasoning(item: &mut Value, reasoning: Option<&str>) {
    let Some(reasoning) = reasoning.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    if let Some(obj) = item.as_object_mut() {
        obj.insert(
            "reasoning_content".to_string(),
            Value::String(reasoning.to_string()),
        );
    }
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

fn base_resp(id: &str, model: &str, status: &str, output: &[Value]) -> Value {
    json!({"id":id,"object":"response","model":model,"status":status,"output":output})
}

fn sse(evt: &str, payload: &Value) -> String {
    let mut data = payload.clone();
    if let Some(obj) = data.as_object_mut() {
        obj.insert("type".into(), Value::String(evt.into()));
    }
    format!("event: {evt}\ndata: {data}\n\n")
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

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn chat_sse_valid_stream_completes_with_output() {
        let events = chat_sse_to_responses_events(
            "resp_1",
            "glm-5.2",
            "data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
             data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14,\"prompt_tokens_details\":{\"cached_tokens\":7},\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\n\n\
             data: [DONE]\n\n",
        )
        .unwrap();
        let joined = events.join("");
        assert!(joined.contains("response.output_text.delta"));
        assert!(joined.contains("\"delta\":\"ok\""));
        assert!(joined.contains("response.completed"));
        assert!(joined.contains("\"total_tokens\":14"));
        assert!(joined.contains("\"cached_tokens\":7"));
        assert!(joined.contains("\"cache_miss_tokens\":3"));
        assert!(joined.contains("\"reasoning_tokens\":3"));
    }

    #[test]
    fn chat_sse_tool_call_arguments_before_id_and_name_are_not_dropped() {
        let chunk1 = json!({
            "id": "c1", "model": "glm-5.2",
            "choices": [{"index": 0,
                "delta": {"tool_calls": [{"index": 0, "type": "function",
                    "function": {"arguments": "{\"cmd\":"}}]},
                "finish_reason": null}]
        });
        let chunk2 = json!({
            "id": "c1", "model": "glm-5.2",
            "choices": [{"index": 0,
                "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "functions__exec_command", "arguments": "\"pwd\"}"}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let stream = format!(
            "data: {}

data: {}

data: [DONE]

",
            chunk1, chunk2
        );
        let events = chat_sse_to_responses_events("resp_1", "glm-5.2", &stream).unwrap();
        let joined = events.join("");
        assert!(joined.contains("response.output_item.added"));
        assert!(joined.contains("response.function_call_arguments.delta"));
        assert!(joined.contains("response.output_item.done"));
        assert!(joined.contains("response.completed"));
    }

    #[test]
    fn chat_sse_restores_mcp_namespace_tool_call() {
        let chunk = json!({
            "id": "c1", "model": "glm-5.2",
            "choices": [{"index": 0,
                "delta": {"tool_calls": [{"index": 0, "id": "call_echo", "type": "function",
                    "function": {"name": "mcp__test_echo__echo", "arguments": "{\"message\":\"hi\"}"}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let stream = format!("data: {}\n\ndata: [DONE]\n\n", chunk);

        let events = chat_sse_to_responses_events("resp_1", "glm-5.2", &stream).unwrap();
        let joined = events.join("");

        assert!(
            joined.contains("\"namespace\":\"mcp__test_echo\""),
            "{joined}"
        );
        assert!(joined.contains("\"name\":\"echo\""), "{joined}");
    }
}
