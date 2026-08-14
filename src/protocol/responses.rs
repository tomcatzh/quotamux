use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{sse::SseEvent, types::Usage};

use super::{ValidationError, chat, set_model, validate_model};

pub fn prepare_direct(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    set_model(&mut body, upstream_model);
    Ok(body)
}

pub fn prepare_for_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role":"system","content":instructions}));
    }
    match body.get("input") {
        Some(Value::String(input)) => messages.push(json!({"role":"user","content":input})),
        Some(Value::Array(items)) => translate_input_items(items, &mut messages),
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
    copy_field(
        &body,
        &mut chat,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    if body.get("top_logprobs").is_some() {
        chat.insert("logprobs".into(), Value::Bool(true));
    }
    translate_reasoning(&body, &mut chat);
    translate_tools(&body, &mut chat);
    translate_text_format(&body, &mut chat);
    Ok(Value::Object(chat))
}

pub fn prepare_from_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    let chat = chat::prepare(body, upstream_model)?;
    let messages = chat.get("messages").and_then(Value::as_array);
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in messages.into_iter().flatten() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            let text = chat_content_text(message.get("content"));
            if !text.is_empty() {
                instructions.push(text);
            }
            continue;
        }
        if role == "tool" {
            input.push(json!({
                "type":"function_call_output",
                "call_id":message.get("tool_call_id").cloned().unwrap_or(Value::String(String::new())),
                "output":chat_content_text(message.get("content")),
            }));
            continue;
        }
        let content = chat_content_text(message.get("content"));
        if !content.is_empty() {
            input.push(json!({"type":"message","role":role,"content":content}));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                input.push(json!({
                    "type":"function_call",
                    "call_id":call.get("id").cloned().unwrap_or(Value::String(String::new())),
                    "name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),
                    "arguments":call.pointer("/function/arguments").cloned().unwrap_or(Value::Null)
                }));
            }
        }
    }
    let mut response = Map::new();
    response.insert("model".into(), Value::String(upstream_model.into()));
    response.insert("input".into(), Value::Array(input));
    if !instructions.is_empty() {
        response.insert(
            "instructions".into(),
            Value::String(instructions.join("\n\n")),
        );
    }
    copy_field(&chat, &mut response, "stream", "stream");
    copy_field(&chat, &mut response, "temperature", "temperature");
    copy_field(&chat, &mut response, "top_p", "top_p");
    if let Some(value) = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"))
    {
        response.insert("max_output_tokens".into(), value.clone());
    }
    for field in ["top_logprobs", "service_tier"] {
        copy_field(&chat, &mut response, field, field);
    }
    if let Some(effort) = chat.get("reasoning_effort") {
        response.insert("reasoning".into(), json!({"effort":effort}));
    }
    if let Some(format) = chat.get("response_format")
        && let Some(format) = chat_format_to_responses(format)
    {
        response.insert("text".into(), json!({"format":format}));
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter_map(|tool| {
                (tool.get("type").and_then(Value::as_str) == Some("function"))
                        .then(|| tool.get("function"))
                        .flatten()
                        .map(|function| json!({
                    "type":"function",
                    "name":function.get("name").cloned().unwrap_or(Value::Null),
                    "description":function.get("description").cloned().unwrap_or(Value::Null),
                    "parameters":function.get("parameters").cloned().unwrap_or(Value::Null),
                    "strict":function.get("strict").cloned().unwrap_or(Value::Bool(false))
                }))
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            response.insert("tools".into(), Value::Array(tools));
        }
    }
    if let Some(choice) = chat.get("tool_choice")
        && let Some(choice) = chat_tool_choice_to_responses(choice)
    {
        response.insert("tool_choice".into(), choice);
    }
    copy_field(
        &chat,
        &mut response,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    Ok(Value::Object(response))
}

fn chat_format_to_responses(format: &Value) -> Option<Value> {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = format.get("json_schema").unwrap_or(&Value::Null);
            Some(json!({
                "type":"json_schema",
                "name":schema.get("name").cloned().unwrap_or(Value::Null),
                "schema":schema.get("schema").cloned().unwrap_or(Value::Null),
                "strict":schema.get("strict").cloned().unwrap_or(Value::Bool(false))
            }))
        }
        Some("json_object" | "text") => Some(format.clone()),
        _ => None,
    }
}

fn chat_tool_choice_to_responses(choice: &Value) -> Option<Value> {
    if choice
        .as_str()
        .is_some_and(|value| matches!(value, "none" | "auto" | "required"))
    {
        return Some(choice.clone());
    }
    if choice.get("type").and_then(Value::as_str) == Some("function") {
        let name = choice
            .pointer("/function/name")
            .or_else(|| choice.get("name"))
            .cloned()
            .unwrap_or(Value::Null);
        return Some(json!({"type":"function","name":name}));
    }
    None
}

fn translate_input_items(items: &[Value], messages: &mut Vec<Value>) {
    let mut pending_reasoning = String::new();
    for item in items {
        match item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let role = if role == "developer" { "system" } else { role };
                let content = responses_content_text(item.get("content"));
                let mut message = json!({"role":role,"content":content});
                let mut carries_reasoning = false;
                if role == "assistant" && !pending_reasoning.is_empty() {
                    message.as_object_mut().unwrap().insert(
                        "reasoning_content".into(),
                        Value::String(std::mem::take(&mut pending_reasoning)),
                    );
                    carries_reasoning = true;
                } else if role != "assistant" {
                    pending_reasoning.clear();
                }
                if carries_reasoning
                    || message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                {
                    messages.push(message);
                }
            }
            "reasoning" => {
                let text = reasoning_text(item);
                if !text.is_empty() {
                    pending_reasoning.push_str(&text);
                } else {
                    pending_reasoning.clear();
                }
            }
            "function_call" => {
                if item.get("namespace").is_some() {
                    pending_reasoning.clear();
                    continue;
                }
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
                    if !pending_reasoning.is_empty() {
                        object.insert(
                            "reasoning_content".into(),
                            Value::String(std::mem::take(&mut pending_reasoning)),
                        );
                    }
                    object
                        .entry("tool_calls")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()
                        .unwrap()
                        .push(call);
                } else {
                    let reasoning = std::mem::take(&mut pending_reasoning);
                    let mut message =
                        json!({"role":"assistant","content":null,"tool_calls":[call]});
                    if !reasoning.is_empty() {
                        message
                            .as_object_mut()
                            .unwrap()
                            .insert("reasoning_content".into(), Value::String(reasoning));
                    }
                    messages.push(message);
                }
            }
            "function_call_output" => {
                pending_reasoning.clear();
                if item.get("namespace").is_some() {
                    continue;
                }
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id":item.get("call_id").cloned().unwrap_or(Value::String(String::new())),
                    "content":responses_content_text(item.get("output")),
                }));
            }
            _ => {
                pending_reasoning.clear();
            }
        }
    }
}

fn chat_content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| matches!(part.get("type").and_then(Value::as_str), Some("text")))
            .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>()
            .join(""),
        Some(_) => String::new(),
        None => String::new(),
    }
}

fn responses_content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text")
                )
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>()
            .join(""),
        Some(_) | None => String::new(),
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
    if let Some(effort) = body.pointer("/reasoning/effort").filter(|effort| {
        effort.as_str().is_some_and(|value| {
            matches!(
                value,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        })
    }) {
        chat.insert("reasoning_effort".into(), effort.clone());
    }
}

fn translate_tools(body: &Value, chat: &mut Map<String, Value>) {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut translated = Vec::new();
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function") {
                translated.push(response_function_to_chat(tool));
            }
        }
        if !translated.is_empty() {
            chat.insert("tools".into(), Value::Array(translated));
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let translated = if let Some(value @ ("none" | "auto" | "required")) = choice.as_str() {
            Some(Value::String(value.into()))
        } else if choice.get("type").and_then(Value::as_str) == Some("function") {
            Some(
                json!({"type":"function","function":{"name":choice.get("name").cloned().unwrap_or(Value::Null)}}),
            )
        } else {
            None
        };
        if let Some(translated) = translated {
            chat.insert("tool_choice".into(), translated);
        }
    }
}

fn response_function_to_chat(tool: &Value) -> Value {
    json!({"type":"function","function":{
        "name":tool.get("name").cloned().unwrap_or(Value::Null),
        "description":tool.get("description").cloned().unwrap_or(Value::Null),
        "parameters":tool.get("parameters").cloned().unwrap_or(Value::Null),
        "strict":tool.get("strict").cloned().unwrap_or(Value::Bool(false))
    }})
}

fn translate_text_format(body: &Value, chat: &mut Map<String, Value>) {
    let Some(format) = body.pointer("/text/format") else {
        return;
    };
    let response_format = match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => Some(json!({"type":"json_schema","json_schema":{
            "name":format.get("name").cloned().unwrap_or(Value::Null),
            "schema":format.get("schema").cloned().unwrap_or(Value::Null),
            "strict":format.get("strict").cloned().unwrap_or(Value::Bool(false))
        }})),
        Some("json_object" | "text") => Some(format.clone()),
        _ => None,
    };
    if let Some(response_format) = response_format {
        chat.insert("response_format".into(), response_format);
    }
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

pub fn chat_to_response(chat: &Value, response_model: &str, request: &Value) -> Value {
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
    let parallel_tool_calls = request
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    json!({
        "id":response_id,"object":"response","created_at":chat.get("created").cloned().unwrap_or_else(|| json!(chrono::Utc::now().timestamp())),
        "status":if finish == Some("length") {"incomplete"} else {"completed"},
        "error":Value::Null,
        "incomplete_details":if finish == Some("length") {json!({"reason":"max_output_tokens"})} else {Value::Null},
        "instructions":request.get("instructions").cloned().unwrap_or(Value::Null),
        "max_output_tokens":request.get("max_output_tokens").cloned().unwrap_or(Value::Null),
        "metadata":request.get("metadata").cloned().unwrap_or(Value::Null),
        "model":response_model,
        "output":output,
        "parallel_tool_calls":parallel_tool_calls,
        "temperature":request.get("temperature").cloned().unwrap_or(Value::Null),
        "tool_choice":request.get("tool_choice").cloned().unwrap_or_else(||Value::String("auto".into())),
        "tools":request.get("tools").cloned().unwrap_or_else(||Value::Array(Vec::new())),
        "top_p":request.get("top_p").cloned().unwrap_or(Value::Null),
        "usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}
    })
}

pub fn response_to_chat(response: &Value, response_model: &str) -> Value {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => reasoning.push_str(&reasoning_text(item)),
            Some("message") => text.push_str(&responses_content_text(item.get("content"))),
            Some("function_call") => tool_calls.push(json!({
                "id":item.get("call_id").or_else(||item.get("id")).cloned().unwrap_or(Value::String(String::new())),
                "type":"function",
                "function":{
                    "name":item.get("name").cloned().unwrap_or(Value::String(String::new())),
                    "arguments":item.get("arguments").cloned().unwrap_or(Value::String("{}".into()))
                }
            })),
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let finish_reason = if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        "length"
    } else if message.contains_key("tool_calls") {
        "tool_calls"
    } else {
        "stop"
    };
    let usage = Usage::from_responses(response);
    json!({
        "id":response.get("id").cloned().unwrap_or_else(||Value::String(format!("chatcmpl-{}",Uuid::now_v7()))),
        "object":"chat.completion",
        "created":response.get("created_at").cloned().unwrap_or_else(||json!(chrono::Utc::now().timestamp())),
        "model":response_model,
        "choices":[{"index":0,"message":Value::Object(message),"finish_reason":finish_reason}],
        "usage":{
            "prompt_tokens":usage.input_tokens,
            "completion_tokens":usage.output_tokens,
            "total_tokens":usage.total_tokens,
            "prompt_tokens_details":{"cached_tokens":usage.cache_hit_tokens},
            "completion_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}
        }
    })
}

pub struct ChatToResponsesStream {
    response_id: String,
    response_model: String,
    response_request: Value,
    sequence: u64,
    reasoning_id: Option<String>,
    message_id: Option<String>,
    reasoning: String,
    text: String,
    calls: BTreeMap<u64, StreamCall>,
    usage: Usage,
    started: bool,
}

struct StreamCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

impl ChatToResponsesStream {
    pub fn new(response_model: impl Into<String>, response_request: Value) -> Self {
        Self {
            response_id: format!("resp_{}", Uuid::now_v7()),
            response_model: response_model.into(),
            response_request,
            sequence: 0,
            reasoning_id: None,
            message_id: None,
            reasoning: String::new(),
            text: String::new(),
            calls: BTreeMap::new(),
            usage: Usage::default(),
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
            self.usage = Usage::from_openai(chunk);
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
        let usage = &self.usage;
        let request = &self.response_request;
        let parallel_tool_calls = request
            .get("parallel_tool_calls")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        json!({
            "id":self.response_id,
            "object":"response",
            "created_at":chrono::Utc::now().timestamp(),
            "status":status,
            "error":Value::Null,
            "incomplete_details":Value::Null,
            "instructions":request.get("instructions").cloned().unwrap_or(Value::Null),
            "max_output_tokens":request.get("max_output_tokens").cloned().unwrap_or(Value::Null),
            "metadata":request.get("metadata").cloned().unwrap_or(Value::Null),
            "model":self.response_model,
            "output":[],
            "parallel_tool_calls":parallel_tool_calls,
            "temperature":request.get("temperature").cloned().unwrap_or(Value::Null),
            "tool_choice":request.get("tool_choice").cloned().unwrap_or_else(||Value::String("auto".into())),
            "tools":request.get("tools").cloned().unwrap_or_else(||Value::Array(Vec::new())),
            "top_p":request.get("top_p").cloned().unwrap_or(Value::Null),
            "usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}
        })
    }
}

pub struct ResponsesToChatStream {
    id: String,
    model: String,
    calls: BTreeMap<String, u64>,
}

impl ResponsesToChatStream {
    pub fn new(response_model: impl Into<String>) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::now_v7()),
            model: response_model.into(),
            calls: BTreeMap::new(),
        }
    }

    pub fn translate(&mut self, event: &Value) -> Vec<Value> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(response) = event.get("response")
            && let Some(id) = response.get("id").and_then(Value::as_str)
        {
            self.id = id.replacen("resp_", "chatcmpl-", 1);
        }
        match kind {
            "response.reasoning_text.delta" => vec![self.delta(json!({
                "reasoning_content":event.get("delta").cloned().unwrap_or(Value::String(String::new()))
            }))],
            "response.output_text.delta" => vec![self.delta(json!({
                "content":event.get("delta").cloned().unwrap_or(Value::String(String::new()))
            }))],
            "response.output_item.added"
                if event.pointer("/item/type").and_then(Value::as_str)
                    == Some("function_call") =>
            {
                let item_id = event
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let index = self.calls.len() as u64;
                self.calls.insert(item_id, index);
                vec![self.delta(json!({"tool_calls":[{
                    "index":index,
                    "id":event.pointer("/item/call_id").cloned().unwrap_or(Value::String(String::new())),
                    "type":"function",
                    "function":{
                        "name":event.pointer("/item/name").cloned().unwrap_or(Value::String(String::new())),
                        "arguments":""
                    }
                }]}))]
            }
            "response.function_call_arguments.delta" => {
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let index = self.calls.get(item_id).copied().unwrap_or(0);
                vec![self.delta(json!({"tool_calls":[{
                    "index":index,
                    "function":{"arguments":event.get("delta").cloned().unwrap_or(Value::String(String::new()))}
                }]}))]
            }
            "response.completed" | "response.incomplete" => {
                let response = event.get("response").unwrap_or(event);
                let usage = Usage::from_responses(response);
                let finish = if kind == "response.incomplete"
                    || response.get("status").and_then(Value::as_str) == Some("incomplete")
                {
                    "length"
                } else if self.calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                vec![json!({
                    "id":self.id,
                    "object":"chat.completion.chunk",
                    "model":self.model,
                    "choices":[{"index":0,"delta":{},"finish_reason":finish}],
                    "usage":{
                        "prompt_tokens":usage.input_tokens,
                        "completion_tokens":usage.output_tokens,
                        "total_tokens":usage.total_tokens,
                        "prompt_tokens_details":{"cached_tokens":usage.cache_hit_tokens},
                        "completion_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}
                    }
                })]
            }
            _ => Vec::new(),
        }
    }

    fn delta(&self, delta: Value) -> Value {
        json!({
            "id":self.id,
            "object":"chat.completion.chunk",
            "model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOGICAL_MODEL;

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
    fn drops_response_tool_namespaces_that_chat_cannot_represent() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":"delegate",
            "tools":[{
                "type":"namespace",
                "name":"multi_agent_v1",
                "description":"Agent tools",
                "tools":[
                    {"type":"function","name":"spawn_agent","description":"Spawn","parameters":{"type":"object"}},
                    {"type":"function","name":"wait_agent","description":"Wait","parameters":{"type":"object"}}
                ]
            }]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert!(chat.get("tools").is_none());
    }

    #[test]
    fn does_not_fabricate_user_text_from_agent_messages() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[{
                "type":"agent_message",
                "author":"parent",
                "recipient":"worker",
                "content":"Read BRIEF.md"
            }]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert!(chat["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn does_not_expose_encrypted_agent_payloads_as_prompt_text() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[{
                "type":"agent_message",
                "author":"parent",
                "recipient":"worker",
                "content":"",
                "encrypted_content":"opaque"
            }]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert!(chat["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn drops_opaque_reasoning_items_without_fabricating_prompt_text() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[
                {"type":"reasoning","id":"rs_encrypted","summary":[]},
                {"type":"message","role":"user","content":"hello"}
            ]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert_eq!(chat["messages"].as_array().unwrap().len(), 1);
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hello");
    }

    #[test]
    fn merges_reasoning_with_following_assistant_message() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"plan"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"I will update the plan."}]},
                {"type":"function_call","call_id":"c1","name":"update_plan","arguments":"{}"}
            ]
        });
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert_eq!(chat["messages"].as_array().unwrap().len(), 1);
        assert_eq!(chat["messages"][0]["content"], "I will update the plan.");
        assert_eq!(chat["messages"][0]["reasoning_content"], "plan");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "update_plan"
        );
    }

    #[test]
    fn returns_reasoning_in_response_output() {
        let chat = json!({"id":"chatcmpl-x","choices":[{"finish_reason":"stop","message":{"reasoning_content":"r","content":"a"}}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}});
        let response = chat_to_response(
            &chat,
            "public-kimi-k3",
            &json!({
                "instructions":"be concise",
                "max_output_tokens":128,
                "parallel_tool_calls":false,
                "metadata":{"trace":"test"}
            }),
        );
        assert_eq!(response["model"], "public-kimi-k3");
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["instructions"], "be concise");
        assert_eq!(response["max_output_tokens"], 128);
        assert_eq!(response["parallel_tool_calls"], false);
        assert_eq!(response["metadata"]["trace"], "test");
        assert!(response.get("store").is_none());
    }

    #[test]
    fn responses_request_preserves_named_tool_choice_while_reasoning() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "input":"weather",
            "user":"unknown-responses-field",
            "reasoning":{"effort":"high"},
            "tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}],
            "tool_choice":{"type":"function","name":"weather"}
        });
        let chat = prepare_for_chat(body, "provider-model").unwrap();
        assert_eq!(chat["reasoning_effort"], "high");
        assert!(chat.get("user").is_none());
        assert!(chat.get("user_id").is_none());
        assert!(chat.get("thinking").is_none());
        assert_eq!(chat["tool_choice"]["type"], "function");
        assert_eq!(chat["tool_choice"]["function"]["name"], "weather");
    }

    #[test]
    fn direct_requests_are_transparent_but_translation_ignores_unknown_extensions() {
        let direct = prepare_direct(
            json!({"model":LOGICAL_MODEL,"input":null,"unknown":true}),
            "provider-model",
        )
        .unwrap();
        assert!(direct["input"].is_null());
        assert_eq!(direct["unknown"], true);

        let body = json!({
            "model":LOGICAL_MODEL,
            "input":[
                {"type":"future_item","content":"must not become prompt text"},
                {"type":"message","role":"user","content":[
                    {"type":"future_part","text":"must also not become prompt text"}
                ]},
                {"type":"function_call","namespace":"crm","call_id":"c1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","namespace":"crm","call_id":"c1","output":"secret result"}
            ],
            "reasoning":{"effort":"future_effort"},
            "tools":[
                {"type":"future_tool","function":{"name":"future"}},
                {"type":"namespace","name":"crm","description":"CRM","tools":[
                    {"type":"function","name":"lookup","parameters":{"type":"object"}}
                ]}
            ],
            "tool_choice":{"type":"future_choice","name":"future"},
            "text":{"format":{"type":"future_format","mode":"strict"}}
        });
        let chat = prepare_for_chat(body, "provider-model").unwrap();
        assert!(chat["messages"].as_array().unwrap().is_empty());
        assert!(chat.get("reasoning_effort").is_none());
        assert!(chat.get("thinking").is_none());
        assert!(chat.get("tools").is_none());
        assert!(chat.get("tool_choice").is_none());
        assert!(chat.get("response_format").is_none());
    }

    #[test]
    fn chat_request_maps_response_format_tools_and_parallel_choice() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "messages":[{"role":"user","content":"weather"}],
            "thinking":{"type":"disabled"},
            "response_format":{"type":"json_schema","json_schema":{
                "name":"weather","schema":{"type":"object"},"strict":true
            }},
            "tools":[{"type":"function","function":{
                "name":"weather","description":"lookup","parameters":{"type":"object"},"strict":true
            }}],
            "tool_choice":{"type":"function","function":{"name":"weather"}},
            "parallel_tool_calls":false
        });
        let response = prepare_from_chat(body, "provider-model").unwrap();
        assert_eq!(response["text"]["format"]["name"], "weather");
        assert_eq!(response["text"]["format"]["schema"]["type"], "object");
        assert_eq!(response["tools"][0]["name"], "weather");
        assert_eq!(response["tools"][0]["strict"], true);
        assert_eq!(
            response["tool_choice"],
            json!({"type":"function","name":"weather"})
        );
        assert_eq!(response["parallel_tool_calls"], false);
    }

    #[test]
    fn chat_translation_only_emits_documented_responses_fields() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "messages":[{"role":"user","content":"hello"}],
            "stop":["END"],
            "n":2,
            "frequency_penalty":0.5,
            "max_completion_tokens":512,
            "top_logprobs":3,
            "service_tier":"auto",
            "user":"opaque-user",
            "tools":[{"type":"future_tool","function":{"name":"future"}}],
            "response_format":{"type":"future_format","mode":"strict"},
            "tool_choice":{"type":"future_choice","name":"future"}
        });
        let response = prepare_from_chat(body, "provider-model").unwrap();
        assert!(response.get("stop").is_none());
        assert!(response.get("n").is_none());
        assert!(response.get("frequency_penalty").is_none());
        assert!(response.get("text").is_none());
        assert!(response.get("tool_choice").is_none());
        assert!(response.get("tools").is_none());
        assert_eq!(response["max_output_tokens"], 512);
        assert_eq!(response["top_logprobs"], 3);
        assert_eq!(response["service_tier"], "auto");
        assert!(response.get("user").is_none());
        assert!(response.get("safety_identifier").is_none());
        assert!(response.get("prompt_cache_key").is_none());
    }

    #[test]
    fn response_source_stream_preserves_reasoning_tools_and_usage() {
        let mut stream = ResponsesToChatStream::new("public-chat-model");
        let reasoning = stream.translate(&json!({
            "type":"response.reasoning_text.delta","delta":"think"
        }));
        assert_eq!(
            reasoning[0]["choices"][0]["delta"]["reasoning_content"],
            "think"
        );
        let added = stream.translate(&json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup"}
        }));
        assert_eq!(
            added[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call-1"
        );
        let arguments = stream.translate(&json!({
            "type":"response.function_call_arguments.delta","item_id":"fc-1","delta":"{\"x\":1}"
        }));
        assert_eq!(
            arguments[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        let completed = stream.translate(&json!({
            "type":"response.completed",
            "response":{"status":"completed","model":"provider-model","usage":{
                "input_tokens":10,"input_tokens_details":{"cached_tokens":6},
                "output_tokens":4,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":14
            }}
        }));
        assert_eq!(completed[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(completed[0]["model"], "public-chat-model");
        assert_eq!(
            completed[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
            6
        );
        assert_eq!(
            completed[0]["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn responses_destination_stream_preserves_canonical_usage() {
        let mut stream = ChatToResponsesStream::new(
            "public-kimi-k3",
            json!({"parallel_tool_calls":false,"instructions":"stream safely"}),
        );
        stream.translate(&json!({
            "choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}],
            "usage":{
                "prompt_tokens":14,"completion_tokens":6,"total_tokens":20,
                "prompt_tokens_details":{"cached_tokens":9},
                "completion_tokens_details":{"reasoning_tokens":2}
            }
        }));
        let completed = stream.finish().pop().expect("response.completed event");
        let completed: Value = serde_json::from_str(&completed.data).unwrap();
        assert_eq!(completed["response"]["model"], "public-kimi-k3");
        assert_eq!(completed["response"]["parallel_tool_calls"], false);
        assert_eq!(completed["response"]["instructions"], "stream safely");
        assert!(completed["response"].get("store").is_none());
        assert_eq!(
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            9
        );
        assert_eq!(
            completed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
    }
}
