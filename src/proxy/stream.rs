use std::collections::BTreeMap;

use serde_json::json;
use serde_json::Value;

pub(super) fn chat_sse_to_responses_events(
    id: &str,
    model: &str,
    text: &str,
) -> Result<Vec<String>, String> {
    let mut state = SseState {
        id,
        model,
        ..Default::default()
    };
    let mut events = vec![
        sse(
            "response.created",
            &json!({"response": base_resp(id, model, "in_progress", &[])}),
        ),
        sse(
            "response.in_progress",
            &json!({"response": base_resp(id, model, "in_progress", &[])}),
        ),
    ];
    let mut saw_valid_chunk = false;

    for block in sse_blocks(text) {
        let event_name = block.event.as_deref();
        let data = block.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
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
            continue;
        };

        saw_valid_chunk = true;
        events.extend(state.push_delta(choice.get("delta").unwrap_or(&Value::Null)));
        if choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .is_some()
        {
            if !state.has_substantive_output() {
                return Err("upstream stream completed without assistant output".into());
            }
            events.extend(state.finalize(&chunk));
            return Ok(events);
        }
    }

    if !saw_valid_chunk {
        return Err("upstream stream contained no valid chat completion chunks".into());
    }
    if !state.has_substantive_output() {
        return Err("upstream stream completed without assistant output".into());
    }
    events.extend(state.finalize(&json!({})));
    Ok(events)
}

struct SseBlock {
    event: Option<String>,
    data: String,
}

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
    output_index: usize,
    reasoning: TextStage,
    text: TextStage,
    tools: BTreeMap<usize, ToolStage>,
    finished: bool,
}

#[derive(Default)]
struct TextStage {
    added: bool,
    item_id: String,
    text: String,
    output_index: usize,
}

#[derive(Default)]
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
            for tool_call in tool_calls {
                events.extend(self.push_tool_call_delta(tool_call));
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

    fn push_tool_call_delta(&mut self, tool_call: &Value) -> Vec<String> {
        let index = tool_call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let function = tool_call.get("function").unwrap_or(&Value::Null);
        let id = tool_call.get("id").and_then(|v| v.as_str());
        let name = function.get("name").and_then(|v| v.as_str());
        let arguments = function
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let should_add = {
            let stage = self.tools.entry(index).or_default();
            if let Some(id) = id {
                stage.call_id = id.to_string();
            }
            if let Some(name) = name {
                stage.name = name.to_string();
            }
            stage.arguments.push_str(arguments);
            !stage.added && (!stage.call_id.is_empty() || !stage.name.is_empty())
        };
        let mut events = Vec::new();
        if should_add {
            events.extend(self.add_tool_call(index));
        }
        if !arguments.is_empty() {
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

    fn add_tool_call(&mut self, index: usize) -> Vec<String> {
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
        vec![sse(
            "response.output_item.added",
            &json!({"output_index": output_index, "item": function_call_item(stage, "in_progress")}),
        )]
    }

    fn finalize(&mut self, chunk: &Value) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut events = Vec::new();
        let usage = chunk.get("usage");
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
                    "input_tokens": usage.and_then(|u| u["prompt_tokens"].as_i64()).unwrap_or(0),
                    "output_tokens": usage.and_then(|u| u["completion_tokens"].as_i64()).unwrap_or(0),
                    "total_tokens": usage.and_then(|u| u["total_tokens"].as_i64()).unwrap_or(0)
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
                        "item": function_call_item(stage, "completed")
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
                .map(|stage| function_call_item(stage, "completed")),
        );
        items
    }

    fn alloc_index(&mut self) -> usize {
        let i = self.output_index;
        self.output_index += 1;
        i
    }

    fn has_substantive_output(&self) -> bool {
        self.reasoning.added || self.text.added || self.tools.values().any(|stage| stage.added)
    }
}

fn function_call_item(stage: &ToolStage, status: &str) -> Value {
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
    item
}

fn split_chat_tool_name(name: &str) -> (Option<String>, String) {
    match name.split_once("__") {
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
    fn sse_event_format() {
        let out = sse(
            "response.output_text.delta",
            &json!({"delta":"hi","item_id":"msg_1"}),
        );
        assert!(out.starts_with("event: response.output_text.delta\n"));
        assert!(out.contains("\"delta\":\"hi\""));
        assert!(out.contains("\"type\":\"response.output_text.delta\""));
    }

    #[test]
    fn chat_sse_without_valid_choices_is_an_error() {
        let err = chat_sse_to_responses_events("resp_1", "glm-5.2", "data: {}\n\n").unwrap_err();
        assert!(err.contains("no valid chat completion chunks"));
    }

    #[test]
    fn chat_sse_error_event_is_an_error() {
        let err = chat_sse_to_responses_events(
            "resp_1",
            "glm-5.2",
            "event: error\ndata: {\"error\":{\"message\":\"bad request\",\"type\":\"invalid_request_error\"}}\n\n",
        )
        .unwrap_err();
        assert!(err.contains("bad request"));
        assert!(err.contains("invalid_request_error"));
    }

    #[test]
    fn chat_sse_valid_stream_completes_with_output() {
        let events = chat_sse_to_responses_events(
            "resp_1",
            "glm-5.2",
            "data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
             data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
             data: [DONE]\n\n",
        )
        .unwrap();
        let joined = events.join("");
        assert!(joined.contains("response.output_text.delta"));
        assert!(joined.contains("\"delta\":\"ok\""));
        assert!(joined.contains("response.completed"));
        assert!(joined.contains("\"total_tokens\":2"));
    }

    #[test]
    fn chat_sse_tool_call_completes_with_function_call_item() {
        let events = chat_sse_to_responses_events(
            "resp_1",
            "glm-5.2",
            "data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"functions__exec_command\",\"arguments\":\"{\\\"cmd\\\":\"}}]},\"finish_reason\":null}]}\n\n\
             data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        )
        .unwrap();
        let joined = events.join("");
        assert!(joined.contains("response.output_item.done"));
        assert!(joined.contains("\"type\":\"function_call\""));
        assert!(joined.contains("\"namespace\":\"functions\""));
        assert!(joined.contains("\"name\":\"exec_command\""));
        assert!(joined.contains(r#""arguments":"{\"cmd\":\"pwd\"}""#));
        assert!(joined.contains("response.completed"));
    }

    #[test]
    fn chat_sse_finish_without_output_is_an_error() {
        let err = chat_sse_to_responses_events(
            "resp_1",
            "glm-5.2",
            "data: {\"id\":\"c1\",\"model\":\"glm-5.2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        )
        .unwrap_err();
        assert!(err.contains("without assistant output"));
    }

    #[test]
    fn sse_state_full_lifecycle() {
        let mut state = SseState {
            id: "r1",
            model: "m1",
            ..Default::default()
        };

        let events = state.push_delta(&json!({"content": "Hello"}));
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("response.output_item.added"));
        assert!(events[1].contains("response.content_part.added"));
        assert!(events[2].contains("response.output_text.delta"));

        let events = state.push_delta(&json!({"content": " world"}));
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("response.output_text.delta"));

        let chunk = json!({"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}});
        let events = state.finalize(&chunk);
        assert!(events
            .iter()
            .any(|e| e.contains("response.output_text.done")));
        assert!(events
            .iter()
            .any(|e| e.contains("response.content_part.done")));
        assert!(events
            .iter()
            .any(|e| e.contains("response.output_item.done")));
        let completed = events.last().unwrap();
        assert!(completed.contains("response.completed"));
        assert!(completed.contains("\"total_tokens\":13"));
    }
}
