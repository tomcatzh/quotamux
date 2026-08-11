use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{config::LOGICAL_MODEL, sse::SseEvent, types::Usage};

use super::{ValidationError, chat, set_model, thinking_enabled, validate_model};

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
    copy_field(
        &body,
        &mut chat,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
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

pub fn prepare_from_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    let chat = chat::prepare(body, upstream_model)?;
    reject_chat_fields(
        &chat,
        &[
            "stop",
            "n",
            "frequency_penalty",
            "presence_penalty",
            "seed",
            "logprobs",
            "top_logprobs",
            "logit_bias",
            "service_tier",
            "user",
        ],
        "OpenAI Responses",
    )?;
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .expect("validated chat messages");
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            let text = content_text(message.get("content"));
            if !text.is_empty() {
                instructions.push(text);
            }
            continue;
        }
        if role == "tool" {
            input.push(json!({
                "type":"function_call_output",
                "call_id":message.get("tool_call_id").cloned().unwrap_or(Value::String(String::new())),
                "output":content_text(message.get("content")),
            }));
            continue;
        }
        if role == "assistant"
            && let Some(reasoning) = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        {
            input.push(json!({
                "type":"reasoning",
                "content":[{"type":"reasoning_text","text":reasoning}],
                "summary":[]
            }));
        }
        let content = content_text(message.get("content"));
        if !content.is_empty() {
            input.push(json!({"type":"message","role":role,"content":content}));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                input.push(json!({
                    "type":"function_call",
                    "call_id":call.get("id").cloned().unwrap_or(Value::String(String::new())),
                    "name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),
                    "arguments":call.pointer("/function/arguments").cloned().unwrap_or(Value::String("{}".into()))
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
    copy_field(&chat, &mut response, "max_tokens", "max_output_tokens");
    if let Some(reasoning) = chat.get("reasoning") {
        response.insert("reasoning".into(), reasoning.clone());
    } else if let Some(effort) = chat.get("reasoning_effort") {
        response.insert("reasoning".into(), json!({"effort":effort}));
    }
    if let Some(format) = chat.get("response_format") {
        response.insert(
            "text".into(),
            json!({"format":chat_format_to_responses(format)?}),
        );
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                json!({
                    "type":"function",
                    "name":function.get("name").cloned().unwrap_or(Value::String(String::new())),
                    "description":function.get("description").cloned().unwrap_or(Value::String(String::new())),
                    "parameters":function.get("parameters").cloned().unwrap_or_else(||json!({"type":"object","properties":{}})),
                    "strict":function.get("strict").cloned().unwrap_or(Value::Bool(false))
                })
            })
            .collect::<Vec<_>>();
        response.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = chat.get("tool_choice") {
        response.insert("tool_choice".into(), chat_tool_choice_to_responses(choice)?);
    }
    copy_field(
        &chat,
        &mut response,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    Ok(Value::Object(response))
}

fn chat_format_to_responses(format: &Value) -> Result<Value, ValidationError> {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = format.get("json_schema").ok_or_else(|| {
                ValidationError::invalid(
                    "response_format.json_schema is required",
                    Some("response_format"),
                )
            })?;
            Ok(json!({
                "type":"json_schema",
                "name":schema.get("name").cloned().unwrap_or(Value::String("response".into())),
                "schema":schema.get("schema").cloned().unwrap_or_else(||json!({"type":"object"})),
                "strict":schema.get("strict").cloned().unwrap_or(Value::Bool(false))
            }))
        }
        Some("json_object" | "text") => Ok(format.clone()),
        Some(kind) => Err(ValidationError::invalid(
            format!("unsupported Chat response_format type {kind} for OpenAI Responses"),
            Some("response_format"),
        )),
        None => Err(ValidationError::invalid(
            "response_format.type is required",
            Some("response_format"),
        )),
    }
}

fn chat_tool_choice_to_responses(choice: &Value) -> Result<Value, ValidationError> {
    if choice.is_string() {
        return Ok(choice.clone());
    }
    if choice.get("type").and_then(Value::as_str) == Some("function") {
        let name = choice
            .pointer("/function/name")
            .or_else(|| choice.get("name"))
            .cloned()
            .ok_or_else(|| {
                ValidationError::invalid(
                    "named tool_choice requires a function name",
                    Some("tool_choice"),
                )
            })?;
        return Ok(json!({"type":"function","name":name}));
    }
    Err(ValidationError::invalid(
        "unsupported Chat tool_choice for OpenAI Responses",
        Some("tool_choice"),
    ))
}

fn reject_chat_fields(
    chat: &Value,
    fields: &[&'static str],
    destination: &str,
) -> Result<(), ValidationError> {
    if let Some(field) = fields.iter().find(|field| chat.get(**field).is_some()) {
        return Err(ValidationError::invalid(
            format!("{destination} cannot represent Chat field {field} without loss"),
            Some(field),
        ));
    }
    Ok(())
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
                let content = content_text(item.get("content"));
                if role == "assistant" && content.is_empty() {
                    continue;
                }
                let mut message = json!({"role":role,"content":content});
                if role == "assistant" && !pending_reasoning.is_empty() {
                    message.as_object_mut().unwrap().insert(
                        "reasoning_content".into(),
                        Value::String(std::mem::take(&mut pending_reasoning)),
                    );
                } else if role != "assistant" {
                    pending_reasoning.clear();
                }
                messages.push(message);
            }
            "reasoning" => {
                let text = reasoning_text(item);
                if !text.is_empty() {
                    pending_reasoning.push_str(&text);
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
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id":item.get("call_id").cloned().unwrap_or(Value::String(String::new())),
                    "content":content_text(item.get("output")),
                }));
            }
            "agent_message" => {
                pending_reasoning.clear();
                let author = item
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("agent");
                let recipient = item
                    .get("recipient")
                    .and_then(Value::as_str)
                    .unwrap_or("agent");
                let content = content_text(item.get("content"));
                if content.is_empty() {
                    let detail = if item.get("encrypted_content").is_some() {
                        "encrypted agent_message content cannot be translated by a third-party provider"
                    } else {
                        "agent_message content must not be empty"
                    };
                    return Err(ValidationError::invalid(detail, Some("input")));
                }
                messages.push(json!({
                    "role":"user",
                    "content":format!(
                        "[Agent message from {author} to {recipient}]\n{}",
                        content
                    )
                }));
            }
            unsupported => {
                let keys = item
                    .as_object()
                    .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
                return Err(ValidationError::invalid(
                    format!(
                        "unsupported Responses input item type {unsupported} (keys={keys}; role={role})"
                    ),
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
        let mut translated = Vec::new();
        for tool in tools {
            match tool.get("type").and_then(Value::as_str) {
                Some("function") => translated.push(response_function_to_chat(tool)),
                Some("namespace") => {
                    let members = tool.get("tools").and_then(Value::as_array).ok_or_else(|| {
                        ValidationError::invalid(
                            "Responses namespace tool requires a tools array",
                            Some("tools"),
                        )
                    })?;
                    for member in members {
                        if member.get("type").and_then(Value::as_str) != Some("function") {
                            return Err(unsupported_tool(member));
                        }
                        translated.push(response_function_to_chat(member));
                    }
                }
                _ => return Err(unsupported_tool(tool)),
            }
        }
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

fn response_function_to_chat(tool: &Value) -> Value {
    json!({"type":"function","function":{
        "name":tool.get("name").cloned().unwrap_or(Value::String(String::new())),
        "description":tool.get("description").cloned().unwrap_or(Value::String(String::new())),
        "parameters":tool.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
        "strict":tool.get("strict").cloned().unwrap_or(Value::Bool(false))
    }})
}

fn unsupported_tool(tool: &Value) -> ValidationError {
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let keys = tool
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
    ValidationError::invalid(
        format!("unsupported Responses tool type {tool_type} (keys={keys}; name={name})"),
        Some("tools"),
    )
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

pub fn chat_to_response(chat: &Value, response_model: &str) -> Value {
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
        "instructions":Value::Null,"max_output_tokens":Value::Null,"model":response_model,
        "output":output,"parallel_tool_calls":true,"store":false,
        "usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}
    })
}

pub fn response_to_chat(response: &Value) -> Value {
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
            Some("message") => text.push_str(&content_text(item.get("content"))),
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
        "model":response.get("model").cloned().unwrap_or(Value::String(LOGICAL_MODEL.into())),
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
    pub fn new(response_model: impl Into<String>) -> Self {
        Self {
            response_id: format!("resp_{}", Uuid::now_v7()),
            response_model: response_model.into(),
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
        json!({"id":self.response_id,"object":"response","created_at":chrono::Utc::now().timestamp(),"status":status,"error":Value::Null,"incomplete_details":Value::Null,"model":self.response_model,"output":[],"parallel_tool_calls":true,"store":false,"usage":{"input_tokens":usage.input_tokens,"input_tokens_details":{"cached_tokens":usage.cache_hit_tokens},"output_tokens":usage.output_tokens,"output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},"total_tokens":usage.total_tokens}})
    }
}

pub struct ResponsesToChatStream {
    id: String,
    model: String,
    calls: BTreeMap<String, u64>,
}

impl Default for ResponsesToChatStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesToChatStream {
    pub fn new() -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::now_v7()),
            model: LOGICAL_MODEL.into(),
            calls: BTreeMap::new(),
        }
    }

    pub fn translate(&mut self, event: &Value) -> Vec<Value> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(response) = event.get("response") {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.id = id.replacen("resp_", "chatcmpl-", 1);
            }
            if let Some(model) = response.get("model").and_then(Value::as_str) {
                self.model = model.into();
            }
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
    fn flattens_response_tool_namespaces_for_chat() {
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
        assert_eq!(chat["tools"][0]["function"]["name"], "spawn_agent");
        assert_eq!(chat["tools"][1]["function"]["name"], "wait_agent");
    }

    #[test]
    fn translates_agent_messages_to_user_messages_for_chat() {
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
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(
            chat["messages"][0]["content"],
            "[Agent message from parent to worker]\nRead BRIEF.md"
        );
    }

    #[test]
    fn rejects_encrypted_agent_messages_instead_of_dropping_the_task() {
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
        let error = prepare_for_chat(body, "deepseek-v4-flash").unwrap_err();
        assert!(error.message.contains("encrypted agent_message"));
    }

    #[test]
    fn ignores_reasoning_items_without_forwardable_text() {
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
        let response = chat_to_response(&chat, "public-kimi-k3");
        assert_eq!(response["model"], "public-kimi-k3");
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
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
    fn chat_request_rejects_unrepresentable_stop_field() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "messages":[{"role":"user","content":"hello"}],
            "stop":["END"]
        });
        let error = prepare_from_chat(body, "provider-model").unwrap_err();
        assert!(error.message.contains("cannot represent Chat field stop"));
    }

    #[test]
    fn response_source_stream_preserves_reasoning_tools_and_usage() {
        let mut stream = ResponsesToChatStream::new();
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
            "response":{"status":"completed","usage":{
                "input_tokens":10,"input_tokens_details":{"cached_tokens":6},
                "output_tokens":4,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":14
            }}
        }));
        assert_eq!(completed[0]["choices"][0]["finish_reason"], "tool_calls");
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
        let mut stream = ChatToResponsesStream::new("public-kimi-k3");
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
