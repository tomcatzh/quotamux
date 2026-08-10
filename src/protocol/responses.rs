use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{config::LOGICAL_MODEL, sse::SseEvent, types::Usage};

use super::{ValidationError, set_model, thinking_enabled, validate_model};

pub fn prepare_direct(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    validate_input(&body)?;
    set_model(&mut body, upstream_model);
    Ok(body)
}

pub fn prepare_for_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    validate_input(&body)?;
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role":"system","content":instructions}));
    }
    match body.get("input") {
        Some(Value::String(input)) => messages.push(json!({"role":"user","content":input})),
        Some(Value::Array(items)) => translate_input_items(items, &mut messages)?,
        _ => {}
    }

    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(upstream_model.into()));
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(value) = body.get("stream") {
        chat.insert("stream".into(), value.clone());
        if value.as_bool() == Some(true) {
            chat.insert("stream_options".into(), json!({"include_usage":true}));
        }
    }
    copy_field(&body, &mut chat, "temperature", "temperature");
    copy_field(&body, &mut chat, "top_p", "top_p");
    copy_field(&body, &mut chat, "max_output_tokens", "max_tokens");
    copy_field(&body, &mut chat, "top_logprobs", "top_logprobs");
    if body.get("top_logprobs").is_some() {
        chat.insert("logprobs".into(), Value::Bool(true));
    }
    if let Some(user) = body.get("user") {
        chat.insert("user_id".into(), user.clone());
    }
    translate_reasoning(&body, &mut chat);
    translate_tools(&body, &mut chat)?;
    translate_text_format(&body, &mut chat);
    Ok(Value::Object(chat))
}

fn validate_input(body: &Value) -> Result<(), ValidationError> {
    if body.get("input").is_none() && body.get("instructions").is_none() {
        return Err(ValidationError::invalid(
            "input or instructions is required",
            Some("input"),
        ));
    }
    Ok(())
}

fn translate_input_items(
    items: &[Value],
    messages: &mut Vec<Value>,
) -> Result<(), ValidationError> {
    for item in items {
        match item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let role = if role == "developer" { "system" } else { role };
                messages.push(json!({"role":role,"content":content_text(item.get("content"))}));
            }
            "reasoning" => {
                let text = reasoning_text(item);
                if let Some(previous) = messages.last_mut().filter(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                }) {
                    previous
                        .as_object_mut()
                        .unwrap()
                        .insert("reasoning_content".into(), Value::String(text));
                } else {
                    messages
                        .push(json!({"role":"assistant","content":null,"reasoning_content":text}));
                }
            }
            "function_call" => {
                let call = json!({
                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or_else(|| Value::String(Uuid::now_v7().to_string())),
                    "type":"function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::String(String::new())),
                        "arguments": item.get("arguments").cloned().unwrap_or(Value::String("{}".into()))
                    }
                });
                if let Some(previous) = messages.last_mut().filter(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                }) {
                    let object = previous.as_object_mut().unwrap();
                    object
                        .entry("tool_calls")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()
                        .unwrap()
                        .push(call);
                } else {
                    messages.push(json!({"role":"assistant","content":null,"tool_calls":[call]}));
                }
            }
            "function_call_output" => messages.push(json!({
                "role":"tool",
                "tool_call_id":item.get("call_id").cloned().unwrap_or(Value::String(String::new())),
                "content":content_text(item.get("output")),
            })),
            unsupported => {
                return Err(ValidationError::invalid(
                    format!("unsupported Responses input item type {unsupported}"),
                    Some("input"),
                ));
            }
        }
    }
    Ok(())
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(value) => value.as_str().unwrap_or_default().to_string(),
        None => String::new(),
    }
}

fn reasoning_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            item.get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default()
        })
}

fn translate_reasoning(body: &Value, chat: &mut Map<String, Value>) {
    let effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .unwrap_or("high");
    let (kind, effort) = match effort {
        "none" => ("disabled", None),
        "minimal" | "low" => ("enabled", Some("low")),
        "max" => ("enabled", Some("max")),
        _ => ("enabled", Some("high")),
    };
    chat.insert("thinking".into(), json!({"type":kind}));
    if let Some(effort) = effort {
        chat.insert("reasoning_effort".into(), Value::String(effort.into()));
    }
}

fn translate_tools(body: &Value, chat: &mut Map<String, Value>) -> Result<(), ValidationError> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated = tools.iter().map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(ValidationError::invalid("only function tools are supported", Some("tools")));
            }
            Ok(json!({"type":"function","function":{
                "name":tool.get("name").cloned().unwrap_or(Value::String(String::new())),
                "description":tool.get("description").cloned().unwrap_or(Value::String(String::new())),
                "parameters":tool.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
                "strict":tool.get("strict").cloned().unwrap_or(Value::Bool(false))
            }}))
        }).collect::<Result<Vec<_>, _>>()?;
        chat.insert("tools".into(), Value::Array(translated));
    }
    if let Some(choice) = body.get("tool_choice") {
        let translated = if let Some(value) = choice.as_str() {
            Value::String(value.into())
        } else if choice.get("type").and_then(Value::as_str) == Some("function") {
            if thinking_enabled(body) {
                return Err(ValidationError::invalid(
                    "DeepSeek V4 thinking mode does not support named tool_choice",
                    Some("tool_choice"),
                ));
            }
            json!({"type":"function","function":{"name":choice.get("name").cloned().unwrap_or(Value::String(String::new()))}})
        } else {
            choice.clone()
        };
        chat.insert("tool_choice".into(), translated);
    }
    Ok(())
}

fn translate_text_format(body: &Value, chat: &mut Map<String, Value>) {
    let Some(format) = body.pointer("/text/format") else {
        return;
    };
    let response_format = if format.get("type").and_then(Value::as_str) == Some("json_schema") {
        json!({"type":"json_schema","json_schema":{
            "name":format.get("name").cloned().unwrap_or(Value::String("response".into())),
            "schema":format.get("schema").cloned().unwrap_or_else(|| json!({"type":"object"})),
            "strict":format.get("strict").cloned().unwrap_or(Value::Bool(false))
        }})
    } else {
        format.clone()
    };
    chat.insert("response_format".into(), response_format);
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

pub fn chat_to_response(chat: &Value) -> Value {
    let id = chat
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl-quotamux");
    let response_id = format!("resp_{}", id.trim_start_matches("chatcmpl-"));
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut output = Vec::new();
    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        output.push(json!({"type":"reasoning","id":format!("rs_{}", Uuid::now_v7()),"status":"completed","summary":[],"content":[{"type":"reasoning_text","text":reasoning}]}));
    }
    if let Some(content) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        output.push(json!({"type":"message","id":format!("msg_{}", Uuid::now_v7()),"status":"completed","role":"assistant","content":[{"type":"output_text","text":content,"annotations":[],"logprobs":[]}]}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            output.push(json!({
                "type":"function_call","id":format!("fc_{}", Uuid::now_v7()),"status":"completed",
                "call_id":call.get("id").cloned().unwrap_or(Value::String(String::new())),
                "name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),
                "arguments":call.pointer("/function/arguments").cloned().unwrap_or(Value::String("{}".into()))
            }));
        }
    }
    let finish = choice.get("finish_reason").and_then(Value::as_str);
    let usage = Usage::from_openai(chat);
    json!({
        "id":response_id,"object":"response","created_at":chat.get("created").cloned().unwrap_or_else(|| json!(chrono::Utc::now().timestamp())),
        "status":if finish == Some("length") {"incomplete"} else {"completed"},
        "error":Value::Null,
        "incomplete_details":if finish == Some("length") {json!({"reason":"max_output_tokens"})} else {Value::Null},
        "instructions":Value::Null,"max_output_tokens":Value::Null,"model":LOGICAL_MODEL,
        "output":output,"parallel_tool_calls":true,"store":false,
        "usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}
    })
}

pub struct ChatToResponsesStream {
    response_id: String,
    sequence: u64,
    reasoning_id: Option<String>,
    message_id: Option<String>,
    reasoning: String,
    text: String,
    calls: BTreeMap<u64, StreamCall>,
    usage: Option<Value>,
    started: bool,
}

impl Default for ChatToResponsesStream {
    fn default() -> Self {
        Self::new()
    }
}

struct StreamCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

impl ChatToResponsesStream {
    pub fn new() -> Self {
        Self {
            response_id: format!("resp_{}", Uuid::now_v7()),
            sequence: 0,
            reasoning_id: None,
            message_id: None,
            reasoning: String::new(),
            text: String::new(),
            calls: BTreeMap::new(),
            usage: None,
            started: false,
        }
    }

    pub fn translate(&mut self, chunk: &Value) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(self.event(
                "response.created",
                json!({"response":self.response_skeleton("in_progress")}),
            ));
            events.push(self.event(
                "response.in_progress",
                json!({"response":self.response_skeleton("in_progress")}),
            ));
        }
        if chunk.get("usage").is_some_and(|value| !value.is_null()) {
            self.usage = chunk.get("usage").cloned();
        }
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                let item_id = self.ensure_reasoning(&mut events);
                self.reasoning.push_str(reasoning);
                events.push(self.event(
                    "response.reasoning_text.delta",
                    json!({"item_id":item_id,"output_index":0,"content_index":0,"delta":reasoning}),
                ));
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                let item_id = self.ensure_message(&mut events);
                self.text.push_str(text);
                let index = usize::from(self.reasoning_id.is_some());
                events.push(self.event("response.output_text.delta", json!({"item_id":item_id,"output_index":index,"content_index":0,"delta":text,"logprobs":[]})));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let is_new = !self.calls.contains_key(&index);
                    if is_new {
                        let item_id = format!("fc_{}", Uuid::now_v7());
                        let call_id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.calls.insert(
                            index,
                            StreamCall {
                                item_id: item_id.clone(),
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: String::new(),
                            },
                        );
                        let output_index = usize::from(self.reasoning_id.is_some())
                            + usize::from(self.message_id.is_some())
                            + index as usize;
                        events.push(self.event("response.output_item.added", json!({"output_index":output_index,"item":{"type":"function_call","id":item_id,"status":"in_progress","call_id":call_id,"name":name,"arguments":""}})));
                    }
                    if let Some(arguments) = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                    {
                        let (item_id, output_index) = {
                            let state = self.calls.get_mut(&index).unwrap();
                            state.arguments.push_str(arguments);
                            (
                                state.item_id.clone(),
                                usize::from(self.reasoning_id.is_some())
                                    + usize::from(self.message_id.is_some())
                                    + index as usize,
                            )
                        };
                        events.push(self.event("response.function_call_arguments.delta", json!({"item_id":item_id,"output_index":output_index,"delta":arguments})));
                    }
                }
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if let Some(id) = self.reasoning_id.clone() {
            events.push(self.event(
                "response.reasoning_text.done",
                json!({"item_id":id,"output_index":0,"content_index":0,"text":self.reasoning}),
            ));
            events.push(self.event("response.output_item.done", json!({"output_index":0,"item":{"type":"reasoning","id":id,"status":"completed","summary":[],"content":[{"type":"reasoning_text","text":self.reasoning}]}})));
        }
        if let Some(id) = self.message_id.clone() {
            let index = usize::from(self.reasoning_id.is_some());
            events.push(self.event("response.output_text.done", json!({"item_id":id,"output_index":index,"content_index":0,"text":self.text,"logprobs":[]})));
            events.push(self.event("response.content_part.done", json!({"item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":self.text,"annotations":[],"logprobs":[]}})));
            events.push(self.event("response.output_item.done", json!({"output_index":index,"item":{"type":"message","id":id,"status":"completed","role":"assistant","content":[{"type":"output_text","text":self.text,"annotations":[],"logprobs":[]}]}})));
        }
        let call_keys = self.calls.keys().copied().collect::<Vec<_>>();
        for index in call_keys {
            let state = self.calls.get(&index).unwrap();
            let output_index = usize::from(self.reasoning_id.is_some())
                + usize::from(self.message_id.is_some())
                + index as usize;
            let item_id = state.item_id.clone();
            let call_id = state.call_id.clone();
            let name = state.name.clone();
            let arguments = state.arguments.clone();
            events.push(self.event(
                "response.function_call_arguments.done",
                json!({"item_id":item_id,"output_index":output_index,"arguments":arguments}),
            ));
            events.push(self.event("response.output_item.done", json!({"output_index":output_index,"item":{"type":"function_call","id":item_id,"status":"completed","call_id":call_id,"name":name,"arguments":arguments}})));
        }
        events.push(self.event(
            "response.completed",
            json!({"response":self.response_skeleton("completed")}),
        ));
        events
    }

    fn ensure_reasoning(&mut self, events: &mut Vec<SseEvent>) -> String {
        if self.reasoning_id.is_none() {
            let id = format!("rs_{}", Uuid::now_v7());
            self.reasoning_id = Some(id.clone());
            events.push(self.event("response.output_item.added", json!({"output_index":0,"item":{"type":"reasoning","id":id,"status":"in_progress","summary":[],"content":[]}})));
            events.push(self.event("response.content_part.added", json!({"item_id":id,"output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}})));
        }
        self.reasoning_id.clone().unwrap()
    }

    fn ensure_message(&mut self, events: &mut Vec<SseEvent>) -> String {
        if self.message_id.is_none() {
            let id = format!("msg_{}", Uuid::now_v7());
            self.message_id = Some(id.clone());
            let index = usize::from(self.reasoning_id.is_some());
            events.push(self.event("response.output_item.added", json!({"output_index":index,"item":{"type":"message","id":id,"status":"in_progress","role":"assistant","content":[]}})));
            events.push(self.event("response.content_part.added", json!({"item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}})));
        }
        self.message_id.clone().unwrap()
    }

    fn event(&mut self, kind: &str, mut value: Value) -> SseEvent {
        if let Some(object) = value.as_object_mut() {
            object.insert("type".into(), Value::String(kind.into()));
            object.insert("sequence_number".into(), json!(self.sequence));
        }
        self.sequence += 1;
        SseEvent::json(kind, &value)
    }

    fn response_skeleton(&self, status: &str) -> Value {
        let usage = self
            .usage
            .as_ref()
            .map(Usage::from_openai)
            .unwrap_or_default();
        json!({"id":self.response_id,"object":"response","created_at":chrono::Utc::now().timestamp(),"status":status,"error":Value::Null,"incomplete_details":Value::Null,"model":LOGICAL_MODEL,"output":[],"parallel_tool_calls":true,"store":false,"usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_response_tools_and_reasoning_to_chat() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"weather"}]},
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"think"}]},
                {"type":"function_call","call_id":"c1","name":"weather","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"sunny"}
            ],
            "tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert_eq!(chat["messages"][1]["reasoning_content"], "think");
        assert_eq!(chat["messages"][1]["tool_calls"][0]["id"], "c1");
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn returns_reasoning_in_response_output() {
        let chat = json!({"id":"chatcmpl-x","choices":[{"finish_reason":"stop","message":{"reasoning_content":"r","content":"a"}}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}});
        let response = chat_to_response(&chat);
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
    }
}
