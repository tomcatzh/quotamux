use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{config::LOGICAL_MODEL, sse::SseEvent, types::Usage};

use super::{ValidationError, set_model, validate_model};

pub fn prepare_direct(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    validate_messages(&body)?;
    set_model(&mut body, upstream_model);
    Ok(body)
}

pub fn prepare_for_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    validate_messages(&body)?;
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        messages.push(json!({"role":"system","content":blocks_text(system)}));
    }
    let thinking_enabled = body.pointer("/thinking/type").and_then(Value::as_str)
        != Some("disabled")
        && body
            .pointer("/output_config/effort")
            .and_then(Value::as_str)
            != Some("none");
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        translate_message(message, &mut messages, thinking_enabled)?;
    }
    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(upstream_model.into()));
    chat.insert("messages".into(), Value::Array(messages));
    copy_field(&body, &mut chat, "max_tokens", "max_tokens");
    copy_field(&body, &mut chat, "temperature", "temperature");
    copy_field(&body, &mut chat, "top_p", "top_p");
    copy_field(&body, &mut chat, "stop_sequences", "stop");
    copy_field(&body, &mut chat, "stream", "stream");
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        chat.insert("stream_options".into(), json!({"include_usage":true}));
    }
    translate_thinking(&body, &mut chat);
    translate_tools(&body, &mut chat)?;
    Ok(Value::Object(chat))
}

fn validate_messages(body: &Value) -> Result<(), ValidationError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ValidationError::invalid("messages must be an array", Some("messages")))?;
    if messages.is_empty() {
        return Err(ValidationError::invalid(
            "messages must not be empty",
            Some("messages"),
        ));
    }
    Ok(())
}

fn translate_message(
    message: &Value,
    messages: &mut Vec<Value>,
    thinking_enabled: bool,
) -> Result<(), ValidationError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        messages.push(json!({"role":role,"content":blocks_text(message.get("content").unwrap_or(&Value::Null))}));
        return Ok(());
    };
    let mut current = Map::new();
    current.insert("role".into(), Value::String(role.into()));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut tool_results = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id":block.get("id").cloned().unwrap_or_else(|| Value::String(Uuid::now_v7().to_string())),
                "type":"function",
                "function":{"name":block.get("name").cloned().unwrap_or(Value::String(String::new())),"arguments":block.get("input").cloned().unwrap_or_else(|| json!({})).to_string()}
            })),
            Some("tool_result") => tool_results.push(json!({
                "role":"tool","tool_call_id":block.get("tool_use_id").cloned().unwrap_or(Value::String(String::new())),"content":blocks_text(block.get("content").unwrap_or(&Value::Null))
            })),
            Some(unsupported) => return Err(ValidationError::invalid(format!("unsupported Anthropic content block {unsupported}"), Some("messages"))),
            None => {}
        }
    }
    let has_text = !text.is_empty();
    let has_reasoning = !reasoning.is_empty();
    let has_calls = !calls.is_empty();
    if has_text {
        current.insert("content".into(), Value::String(text));
    } else {
        current.insert("content".into(), Value::Null);
    }
    if has_reasoning {
        current.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if has_calls {
        if !has_reasoning && thinking_enabled {
            return Err(ValidationError::invalid(
                "assistant tool_use history must include its thinking block",
                Some("messages"),
            ));
        }
        current.insert("tool_calls".into(), Value::Array(calls));
    }
    if has_text || has_reasoning || has_calls {
        messages.push(Value::Object(current));
    }
    messages.extend(tool_results);
    Ok(())
}

fn blocks_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn translate_thinking(body: &Value, chat: &mut Map<String, Value>) {
    let disabled = body.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled")
        || body
            .pointer("/output_config/effort")
            .and_then(Value::as_str)
            == Some("none");
    chat.insert(
        "thinking".into(),
        json!({"type":if disabled {"disabled"} else {"enabled"}}),
    );
    if !disabled {
        let effort = body
            .pointer("/output_config/effort")
            .and_then(Value::as_str)
            .unwrap_or("high");
        chat.insert(
            "reasoning_effort".into(),
            Value::String(
                match effort {
                    "low" => "low",
                    "max" => "max",
                    _ => "high",
                }
                .into(),
            ),
        );
    }
}

fn translate_tools(body: &Value, chat: &mut Map<String, Value>) -> Result<(), ValidationError> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let tools = tools.iter().map(|tool| Ok(json!({"type":"function","function":{
            "name":tool.get("name").cloned().unwrap_or(Value::String(String::new())),
            "description":tool.get("description").cloned().unwrap_or(Value::String(String::new())),
            "parameters":tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
        }}))).collect::<Result<Vec<Value>, ValidationError>>()?;
        chat.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = body.get("tool_choice") {
        let kind = choice.get("type").and_then(Value::as_str).unwrap_or("auto");
        let translated = match kind {
            "none" | "auto" => Value::String(kind.into()),
            "any" => Value::String("required".into()),
            "tool" => {
                if body.pointer("/thinking/type").and_then(Value::as_str) != Some("disabled") {
                    return Err(ValidationError::invalid(
                        "DeepSeek V4 thinking mode does not support named tool_choice",
                        Some("tool_choice"),
                    ));
                }
                json!({"type":"function","function":{"name":choice.get("name").cloned().unwrap_or(Value::String(String::new()))}})
            }
            _ => {
                return Err(ValidationError::invalid(
                    "invalid Anthropic tool_choice",
                    Some("tool_choice"),
                ));
            }
        };
        chat.insert("tool_choice".into(), translated);
    }
    Ok(())
}

fn copy_field(
    source: &Value,
    target: &mut Map<String, Value>,
    source_name: &str,
    target_name: &str,
) {
    if let Some(value) = source.get(source_name) {
        target.insert(target_name.into(), value.clone());
    }
}

pub fn chat_to_message(chat: &Value) -> Value {
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = Vec::new();
    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        content.push(json!({"type":"thinking","thinking":reasoning,"signature":"quotamux"}));
    }
    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        content.push(json!({"type":"text","text":text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            content.push(json!({"type":"tool_use","id":call.get("id").cloned().unwrap_or(Value::String(String::new())),"name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),"input":serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({}))}));
        }
    }
    let usage = Usage::from_openai(chat);
    json!({"id":format!("msg_{}", Uuid::now_v7()),"type":"message","role":"assistant","model":LOGICAL_MODEL,"content":content,"stop_reason":match choice.get("finish_reason").and_then(Value::as_str) {Some("tool_calls")=>"tool_use",Some("length")=>"max_tokens",_=>"end_turn"},"stop_sequence":Value::Null,"usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_input_tokens":usage.cache_hit_tokens,"cache_creation_input_tokens":0}})
}

pub struct ChatToAnthropicStream {
    id: String,
    block_index: u64,
    reasoning_open: bool,
    text_open: bool,
    calls: std::collections::BTreeMap<u64, (u64, String, String, String)>,
    usage: Usage,
    started: bool,
    stop_reason: String,
}

impl Default for ChatToAnthropicStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatToAnthropicStream {
    pub fn new() -> Self {
        Self {
            id: format!("msg_{}", Uuid::now_v7()),
            block_index: 0,
            reasoning_open: false,
            text_open: false,
            calls: Default::default(),
            usage: Usage::default(),
            started: false,
            stop_reason: "end_turn".into(),
        }
    }

    pub fn translate(&mut self, chunk: &Value) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(SseEvent::json("message_start", &json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":LOGICAL_MODEL,"content":[],"stop_reason":Value::Null,"stop_sequence":Value::Null,"usage":{"input_tokens":0,"output_tokens":0}}})));
        }
        if chunk.get("usage").is_some_and(|usage| !usage.is_null()) {
            self.usage = Usage::from_openai(chunk);
        }
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop_reason = match reason {
                    "tool_calls" => "tool_use",
                    "length" => "max_tokens",
                    _ => "end_turn",
                }
                .into();
            }
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                if !self.reasoning_open {
                    self.reasoning_open = true;
                    events.push(SseEvent::json("content_block_start", &json!({"type":"content_block_start","index":self.block_index,"content_block":{"type":"thinking","thinking":"","signature":"quotamux"}})));
                }
                events.push(SseEvent::json("content_block_delta", &json!({"type":"content_block_delta","index":self.block_index,"delta":{"type":"thinking_delta","thinking":reasoning}})));
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                if self.reasoning_open {
                    events.push(SseEvent::json(
                        "content_block_stop",
                        &json!({"type":"content_block_stop","index":self.block_index}),
                    ));
                    self.reasoning_open = false;
                    self.block_index += 1;
                }
                if !self.text_open {
                    self.text_open = true;
                    events.push(SseEvent::json("content_block_start", &json!({"type":"content_block_start","index":self.block_index,"content_block":{"type":"text","text":""}})));
                }
                events.push(SseEvent::json("content_block_delta", &json!({"type":"content_block_delta","index":self.block_index,"delta":{"type":"text_delta","text":text}})));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                if self.reasoning_open || self.text_open {
                    events.push(SseEvent::json(
                        "content_block_stop",
                        &json!({"type":"content_block_stop","index":self.block_index}),
                    ));
                    self.reasoning_open = false;
                    self.text_open = false;
                    self.block_index += 1;
                }
                for call in calls {
                    let call_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    if !self.calls.contains_key(&call_index) {
                        let id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let index = self.block_index;
                        self.block_index += 1;
                        self.calls
                            .insert(call_index, (index, id.clone(), name.clone(), String::new()));
                        events.push(SseEvent::json("content_block_start", &json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}})));
                    }
                    if let Some(arguments) = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                    {
                        let state = self.calls.get_mut(&call_index).unwrap();
                        state.3.push_str(arguments);
                        events.push(SseEvent::json("content_block_delta", &json!({"type":"content_block_delta","index":state.0,"delta":{"type":"input_json_delta","partial_json":arguments}})));
                    }
                }
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.reasoning_open || self.text_open {
            events.push(SseEvent::json(
                "content_block_stop",
                &json!({"type":"content_block_stop","index":self.block_index}),
            ));
        }
        for (index, _, _, _) in self.calls.values() {
            events.push(SseEvent::json(
                "content_block_stop",
                &json!({"type":"content_block_stop","index":index}),
            ));
        }
        events.push(SseEvent::json("message_delta", &json!({"type":"message_delta","delta":{"stop_reason":self.stop_reason,"stop_sequence":Value::Null},"usage":{"output_tokens":self.usage.output_tokens}})));
        events.push(SseEvent::json(
            "message_stop",
            &json!({"type":"message_stop"}),
        ));
        events
    }
}

pub fn estimate_tokens(body: &Value) -> u64 {
    fn visit(value: &Value, bytes: &mut usize) {
        match value {
            Value::String(text) => *bytes += text.len(),
            Value::Array(items) => items.iter().for_each(|item| visit(item, bytes)),
            Value::Object(object) => object.values().for_each(|value| visit(value, bytes)),
            _ => {}
        }
    }
    let mut bytes = 0;
    visit(body, &mut bytes);
    ((bytes as u64).saturating_add(3) / 4).saturating_add(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_thinking_and_tool_history() {
        let body = json!({"model":LOGICAL_MODEL,"max_tokens":128,"messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"x"},{"type":"tool_use","id":"t1","name":"f","input":{"x":1}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}
        ]});
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert_eq!(chat["messages"][0]["reasoning_content"], "reason");
        assert_eq!(chat["messages"][1]["role"], "tool");
    }

    #[test]
    fn exposes_reasoning_as_thinking_block() {
        let chat = json!({"choices":[{"finish_reason":"stop","message":{"reasoning_content":"r","content":"a"}}],"usage":{"prompt_tokens":2,"completion_tokens":3}});
        let message = chat_to_message(&chat);
        assert_eq!(message["content"][0]["type"], "thinking");
        assert_eq!(message["content"][1]["type"], "text");
    }
}
