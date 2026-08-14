use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{sse::SseEvent, types::Usage};

use super::{ValidationError, chat, set_model, validate_model};

// Anthropic Messages requires max_tokens, while Chat Completions permits it to
// be omitted. Cross-protocol routing therefore needs one explicit transport
// default; direct Messages requests remain untouched.
const DEFAULT_TRANSLATED_MAX_TOKENS: u64 = 4096;

pub fn prepare_direct(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    set_model(&mut body, upstream_model);
    Ok(body)
}

pub fn prepare_for_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let text = blocks_text(system);
        if !text.is_empty() {
            messages.push(json!({"role":"system","content":text}));
        }
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        translate_message(message, &mut messages);
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
    translate_tools(&body, &mut chat);
    translate_output_format(&body, &mut chat);
    Ok(Value::Object(chat))
}

pub fn prepare_from_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    let chat = chat::prepare(body, upstream_model)?;
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in chat
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            let text = blocks_text(message.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                system.push(text);
            }
            continue;
        }
        if role == "tool" {
            messages.push(json!({
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":message.get("tool_call_id").cloned().unwrap_or(Value::String(String::new())),
                    "content":blocks_text(message.get("content").unwrap_or(&Value::Null))
                }]
            }));
            continue;
        }
        let mut content = Vec::new();
        let text = blocks_text(message.get("content").unwrap_or(&Value::Null));
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let arguments = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                content.push(json!({
                    "type":"tool_use",
                    "id":call.get("id").cloned().unwrap_or(Value::String(String::new())),
                    "name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),
                    "input":serde_json::from_str::<Value>(arguments)
                        .unwrap_or_else(|_|Value::String(arguments.into()))
                }));
            }
        }
        if !content.is_empty() {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    let mut anthropic = Map::new();
    anthropic.insert("model".into(), Value::String(upstream_model.into()));
    anthropic.insert("messages".into(), Value::Array(messages));
    anthropic.insert(
        "max_tokens".into(),
        chat.get("max_completion_tokens")
            .or_else(|| chat.get("max_tokens"))
            .cloned()
            .unwrap_or_else(|| json!(DEFAULT_TRANSLATED_MAX_TOKENS)),
    );
    if !system.is_empty() {
        anthropic.insert("system".into(), Value::String(system.join("\n\n")));
    }
    copy_field(&chat, &mut anthropic, "stream", "stream");
    copy_field(&chat, &mut anthropic, "temperature", "temperature");
    copy_field(&chat, &mut anthropic, "top_p", "top_p");
    copy_field(&chat, &mut anthropic, "stop", "stop_sequences");
    if let Some(user) = chat.get("user") {
        anthropic.insert("metadata".into(), json!({"user_id":user}));
    }
    let mut output_config = Map::new();
    if let Some(effort) = chat.get("reasoning_effort").filter(|effort| {
        effort
            .as_str()
            .is_some_and(|value| matches!(value, "low" | "medium" | "high" | "xhigh" | "max"))
    }) {
        output_config.insert("effort".into(), effort.clone());
    }
    if let Some(format) = chat.get("response_format")
        && let Some(format) = chat_format_to_anthropic(format)
    {
        output_config.insert("format".into(), format);
    }
    if !output_config.is_empty() {
        anthropic.insert("output_config".into(), Value::Object(output_config));
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter_map(|tool| {
                (tool.get("type").and_then(Value::as_str) == Some("function"))
                    .then(|| tool.get("function"))
                    .flatten()
                    .map(|function| {
                    json!({
                        "name":function.get("name").cloned().unwrap_or(Value::Null),
                        "description":function.get("description").cloned().unwrap_or(Value::Null),
                        "input_schema":function.get("parameters").cloned().unwrap_or(Value::Null),
                        "strict":function.get("strict").cloned().unwrap_or(Value::Bool(false))
                    })
                })
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            anthropic.insert("tools".into(), Value::Array(tools));
        }
    }
    if (chat.get("tool_choice").is_some() || chat.get("parallel_tool_calls").is_some())
        && let Some(choice) = chat_tool_choice_to_anthropic(
            chat.get("tool_choice"),
            chat.get("parallel_tool_calls").and_then(Value::as_bool),
        )
    {
        anthropic.insert("tool_choice".into(), choice);
    }
    Ok(Value::Object(anthropic))
}

fn chat_format_to_anthropic(format: &Value) -> Option<Value> {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = format
                .pointer("/json_schema/schema")
                .cloned()
                .unwrap_or(Value::Null);
            Some(json!({"type":"json_schema","schema":schema}))
        }
        _ => None,
    }
}

fn chat_tool_choice_to_anthropic(choice: Option<&Value>, parallel: Option<bool>) -> Option<Value> {
    let mut translated = match choice {
        None if parallel.is_some() => json!({"type":"auto"}),
        None => return None,
        Some(Value::String(value)) if value == "auto" => json!({"type":"auto"}),
        Some(Value::String(value)) if value == "none" => json!({"type":"none"}),
        Some(Value::String(value)) if value == "required" => json!({"type":"any"}),
        Some(value) if value.get("type").and_then(Value::as_str) == Some("function") => {
            let name = value
                .pointer("/function/name")
                .or_else(|| value.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            json!({"type":"tool","name":name})
        }
        Some(_) => return None,
    };
    if parallel == Some(false)
        && let Some(translated) = translated.as_object_mut()
    {
        translated.insert("disable_parallel_tool_use".into(), Value::Bool(true));
    }
    Some(translated)
}

fn translate_message(message: &Value, messages: &mut Vec<Value>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        messages.push(json!({"role":role,"content":blocks_text(message.get("content").unwrap_or(&Value::Null))}));
        return;
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
            Some(_) | None => {}
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
        current.insert("tool_calls".into(), Value::Array(calls));
    }
    messages.extend(tool_results);
    if has_text || has_reasoning || has_calls {
        messages.push(Value::Object(current));
    }
}

fn blocks_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        _ => String::new(),
    }
}

fn translate_thinking(body: &Value, chat: &mut Map<String, Value>) {
    let thinking_type = body.pointer("/thinking/type").and_then(Value::as_str);
    let effort = body.pointer("/output_config/effort");
    if thinking_type == Some("disabled") {
        chat.insert("reasoning_effort".into(), Value::String("none".into()));
    } else if let Some(effort) = effort.filter(|effort| {
        effort
            .as_str()
            .is_some_and(|value| matches!(value, "low" | "medium" | "high" | "xhigh" | "max"))
    }) {
        chat.insert("reasoning_effort".into(), effort.clone());
    }
}

fn translate_output_format(body: &Value, chat: &mut Map<String, Value>) {
    let Some(format) = body.pointer("/output_config/format") else {
        return;
    };
    if format.get("type").and_then(Value::as_str) == Some("json_schema") {
        chat.insert(
            "response_format".into(),
            json!({
                "type":"json_schema",
                "json_schema":{
                    "name":"response",
                    "schema":format.get("schema").cloned().unwrap_or(Value::Null),
                    "strict":true
                }
            }),
        );
    }
}

fn translate_tools(body: &Value, chat: &mut Map<String, Value>) {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter(|tool| {
                matches!(
                    tool.get("type").and_then(Value::as_str),
                    None | Some("custom")
                ) && tool.get("input_schema").is_some()
            })
            .map(|tool| {
                json!({"type":"function","function":{
                    "name":tool.get("name").cloned().unwrap_or(Value::Null),
                    "description":tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters":tool.get("input_schema").cloned().unwrap_or(Value::Null),
                    "strict":tool.get("strict").cloned().unwrap_or(Value::Bool(false))
                }})
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            chat.insert("tools".into(), Value::Array(tools));
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let translated = match choice.get("type").and_then(Value::as_str) {
            Some(kind @ ("none" | "auto")) => Some(Value::String(kind.into())),
            Some("any") => Some(Value::String("required".into())),
            Some("tool") => Some(
                json!({"type":"function","function":{"name":choice.get("name").cloned().unwrap_or(Value::Null)}}),
            ),
            _ => None,
        };
        if let Some(translated) = translated {
            chat.insert("tool_choice".into(), translated);
            if choice
                .get("disable_parallel_tool_use")
                .and_then(Value::as_bool)
                == Some(true)
            {
                chat.insert("parallel_tool_calls".into(), Value::Bool(false));
            }
        }
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

pub fn chat_to_message(chat: &Value, response_model: &str) -> Value {
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = Vec::new();
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
            content.push(json!({"type":"tool_use","id":call.get("id").cloned().unwrap_or(Value::String(String::new())),"name":call.pointer("/function/name").cloned().unwrap_or(Value::String(String::new())),"input":serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| Value::String(arguments.into()))}));
        }
    }
    let usage = Usage::from_openai(chat);
    json!({"id":format!("msg_{}", Uuid::now_v7()),"type":"message","role":"assistant","model":response_model,"content":content,"stop_reason":match choice.get("finish_reason").and_then(Value::as_str) {Some("tool_calls")=>"tool_use",Some("length")=>"max_tokens",_=>"end_turn"},"stop_sequence":Value::Null,"usage":{"input_tokens":usage.cache_miss_tokens,"output_tokens":usage.output_tokens,"cache_read_input_tokens":usage.cache_hit_tokens,"cache_creation_input_tokens":0}})
}

pub fn message_to_chat(message: &Value, response_model: &str) -> Value {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or(""))
            }
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id":block.get("id").cloned().unwrap_or(Value::String(String::new())),
                "type":"function",
                "function":{
                    "name":block.get("name").cloned().unwrap_or(Value::String(String::new())),
                    "arguments":block.get("input").cloned().unwrap_or_else(||json!({})).to_string()
                }
            })),
            _ => {}
        }
    }
    let mut chat_message = Map::new();
    chat_message.insert("role".into(), Value::String("assistant".into()));
    chat_message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !reasoning.is_empty() {
        chat_message.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        chat_message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let finish_reason = match message.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    };
    let usage = Usage::from_anthropic(message);
    json!({
        "id":message.get("id").cloned().unwrap_or_else(||Value::String(format!("chatcmpl-{}",Uuid::now_v7()))),
        "object":"chat.completion",
        "created":chrono::Utc::now().timestamp(),
        "model":response_model,
        "choices":[{"index":0,"message":Value::Object(chat_message),"finish_reason":finish_reason}],
        "usage":{
            "prompt_tokens":usage.input_tokens,
            "completion_tokens":usage.output_tokens,
            "total_tokens":usage.total_tokens,
            "prompt_tokens_details":{"cached_tokens":usage.cache_hit_tokens}
        }
    })
}

pub struct ChatToAnthropicStream {
    id: String,
    response_model: String,
    block_index: u64,
    text_open: bool,
    calls: std::collections::BTreeMap<u64, (u64, String, String, String)>,
    usage: Usage,
    started: bool,
    stop_reason: String,
}

impl ChatToAnthropicStream {
    pub fn new(response_model: impl Into<String>) -> Self {
        Self {
            id: format!("msg_{}", Uuid::now_v7()),
            response_model: response_model.into(),
            block_index: 0,
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
            events.push(SseEvent::json("message_start", &json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.response_model,"content":[],"stop_reason":Value::Null,"stop_sequence":Value::Null,"usage":{"input_tokens":0,"output_tokens":0}}})));
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
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                if !self.text_open {
                    self.text_open = true;
                    events.push(SseEvent::json("content_block_start", &json!({"type":"content_block_start","index":self.block_index,"content_block":{"type":"text","text":""}})));
                }
                events.push(SseEvent::json("content_block_delta", &json!({"type":"content_block_delta","index":self.block_index,"delta":{"type":"text_delta","text":text}})));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                if self.text_open {
                    events.push(SseEvent::json(
                        "content_block_stop",
                        &json!({"type":"content_block_stop","index":self.block_index}),
                    ));
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
        if self.text_open {
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
        events.push(SseEvent::json("message_delta", &json!({"type":"message_delta","delta":{"stop_reason":self.stop_reason,"stop_sequence":Value::Null},"usage":{"input_tokens":self.usage.cache_miss_tokens,"cache_creation_input_tokens":0,"cache_read_input_tokens":self.usage.cache_hit_tokens,"output_tokens":self.usage.output_tokens}})));
        events.push(SseEvent::json(
            "message_stop",
            &json!({"type":"message_stop"}),
        ));
        events
    }
}

pub struct AnthropicToChatStream {
    id: String,
    model: String,
    blocks: std::collections::BTreeMap<u64, AnthropicBlock>,
    next_call_index: u64,
    usage: Usage,
}

enum AnthropicBlock {
    Thinking,
    Text,
    Tool { call_index: u64 },
}

impl AnthropicToChatStream {
    pub fn new(response_model: impl Into<String>) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::now_v7()),
            model: response_model.into(),
            blocks: Default::default(),
            next_call_index: 0,
            usage: Usage::default(),
        }
    }

    pub fn translate(&mut self, event: &Value) -> Vec<Value> {
        self.usage.observe_anthropic_stream_event(event);
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let message = event.get("message").unwrap_or(&Value::Null);
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    self.id = id.replacen("msg_", "chatcmpl-", 1);
                }
                vec![self.delta(json!({"role":"assistant"}))]
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = event.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        self.blocks.insert(index, AnthropicBlock::Thinking);
                        Vec::new()
                    }
                    Some("text") => {
                        self.blocks.insert(index, AnthropicBlock::Text);
                        Vec::new()
                    }
                    Some("tool_use") => {
                        let call_index = self.next_call_index;
                        self.next_call_index += 1;
                        self.blocks
                            .insert(index, AnthropicBlock::Tool { call_index });
                        vec![self.delta(json!({"tool_calls":[{
                            "index":call_index,
                            "id":block.get("id").cloned().unwrap_or(Value::String(String::new())),
                            "type":"function",
                            "function":{
                                "name":block.get("name").cloned().unwrap_or(Value::String(String::new())),
                                "arguments":""
                            }
                        }]}))]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match self.blocks.get(&index) {
                    Some(AnthropicBlock::Thinking) => vec![self.delta(json!({
                        "reasoning_content":delta.get("thinking").cloned().unwrap_or(Value::String(String::new()))
                    }))],
                    Some(AnthropicBlock::Text) => vec![self.delta(json!({
                        "content":delta.get("text").cloned().unwrap_or(Value::String(String::new()))
                    }))],
                    Some(AnthropicBlock::Tool { call_index }) => vec![self.delta(json!({
                        "tool_calls":[{
                            "index":call_index,
                            "function":{"arguments":delta.get("partial_json").cloned().unwrap_or(Value::String(String::new()))}
                        }]
                    }))],
                    None => Vec::new(),
                }
            }
            "message_delta" => {
                let finish = match event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    Some("tool_use") => "tool_calls",
                    Some("max_tokens") => "length",
                    _ => "stop",
                };
                vec![json!({
                    "id":self.id,
                    "object":"chat.completion.chunk",
                    "model":self.model,
                    "choices":[{"index":0,"delta":{},"finish_reason":finish}],
                    "usage":{
                        "prompt_tokens":self.usage.input_tokens,
                        "completion_tokens":self.usage.output_tokens,
                        "total_tokens":self.usage.total_tokens,
                        "prompt_tokens_details":{"cached_tokens":self.usage.cache_hit_tokens}
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
    use crate::config::LOGICAL_MODEL;

    #[test]
    fn translates_thinking_and_tool_history() {
        let body = json!({"model":LOGICAL_MODEL,"max_tokens":128,"messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"x"},{"type":"tool_use","id":"t1","name":"f","input":{"x":1}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"ok"},
                {"type":"text","text":"continue after the tool result"}
            ]}
        ]});
        let chat = prepare_for_chat(body, "deepseek-v4-flash").unwrap();
        assert_eq!(chat["messages"][0]["reasoning_content"], "reason");
        assert_eq!(chat["messages"][1]["role"], "tool");
        assert_eq!(chat["messages"][1]["tool_call_id"], "t1");
        assert_eq!(chat["messages"][2]["role"], "user");
        assert_eq!(
            chat["messages"][2]["content"],
            "continue after the tool result"
        );
    }

    #[test]
    fn messages_request_passes_missing_thinking_history_to_upstream() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "max_tokens":128,
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"high"},
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t1","name":"f","input":{"x":1}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"ok"}
                ]}
            ]
        });
        let chat = prepare_for_chat(body, "kimi-k3").unwrap();
        assert_eq!(chat["reasoning_effort"], "high");
        assert!(chat.get("thinking").is_none());
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "f"
        );
        assert!(chat["messages"][0].get("reasoning_content").is_none());
        assert_eq!(chat["messages"][1]["tool_call_id"], "t1");
    }

    #[test]
    fn does_not_fabricate_anthropic_thinking_signatures() {
        let chat = json!({"choices":[{"finish_reason":"stop","message":{"reasoning_content":"r","content":"a"}}],"usage":{"prompt_tokens":2,"completion_tokens":3}});
        let message = chat_to_message(&chat, "public-anthropic-model");
        assert_eq!(message["model"], "public-anthropic-model");
        assert_eq!(message["content"].as_array().unwrap().len(), 1);
        assert_eq!(message["content"][0]["type"], "text");
        assert!(!message.to_string().contains("quotamux"));
    }

    #[test]
    fn translated_chat_history_and_streams_do_not_fabricate_thinking_blocks() {
        let request = prepare_from_chat(
            json!({
                "model":LOGICAL_MODEL,
                "messages":[{"role":"assistant","reasoning_content":"private","content":"answer"}]
            }),
            "provider-model",
        )
        .unwrap();
        assert_eq!(request["messages"][0]["content"][0]["type"], "text");
        assert!(!request.to_string().contains("thinking"));
        assert!(!request.to_string().contains("signature"));

        let mut stream = ChatToAnthropicStream::new("public-anthropic-model");
        let events = stream.translate(&json!({
            "choices":[{"delta":{"reasoning_content":"private","content":"answer"}}]
        }));
        let started: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(started["message"]["model"], "public-anthropic-model");
        let serialized = events
            .iter()
            .map(|event| event.data.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!serialized.contains("thinking"));
        assert!(!serialized.contains("signature"));
        assert!(serialized.contains("answer"));
    }

    #[test]
    fn chat_request_maps_output_format_tools_and_parallel_choice() {
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
        let message = prepare_from_chat(body, "provider-model").unwrap();
        assert_eq!(message["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            message["output_config"]["format"]["schema"]["type"],
            "object"
        );
        assert_eq!(message["tools"][0]["strict"], true);
        assert_eq!(message["tool_choice"]["type"], "tool");
        assert_eq!(message["tool_choice"]["name"], "weather");
        assert_eq!(message["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn messages_request_maps_output_format_to_canonical_chat() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "max_tokens":128,
            "messages":[{"role":"user","content":"weather"}],
            "output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}
        });
        let chat = prepare_for_chat(body, "provider-model").unwrap();
        assert_eq!(chat["response_format"]["type"], "json_schema");
        assert_eq!(
            chat["response_format"]["json_schema"]["schema"]["type"],
            "object"
        );
    }

    #[test]
    fn messages_request_preserves_named_tool_choice_while_thinking() {
        let body = json!({
            "model":LOGICAL_MODEL,
            "max_tokens":128,
            "thinking":{"type":"enabled","budget_tokens":64},
            "messages":[{"role":"user","content":"weather"}],
            "tools":[{"name":"weather","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"weather"}
        });
        let chat = prepare_for_chat(body, "provider-model").unwrap();
        assert!(chat.get("thinking").is_none());
        assert_eq!(chat["tool_choice"]["type"], "function");
        assert_eq!(chat["tool_choice"]["function"]["name"], "weather");
    }

    #[test]
    fn direct_requests_are_transparent_but_translation_ignores_unknown_extensions() {
        let direct = prepare_direct(
            json!({"model":LOGICAL_MODEL,"messages":null,"unknown":true}),
            "provider-model",
        )
        .unwrap();
        assert!(direct["messages"].is_null());
        assert_eq!(direct["unknown"], true);

        let body = json!({
            "model":LOGICAL_MODEL,
            "max_tokens":128,
            "system":[{"type":"future_block","text":"must not become a system prompt"}],
            "messages":[{"role":"user","content":[{
                "type":"future_block","text":"must not become prompt text","payload":{"x":1}
            }]}],
            "thinking":{"type":"future_thinking","budget_tokens":"many"},
            "tools":[{"type":"future_tool","name":"future","input_schema":{"type":"object"}}],
            "tool_choice":{"name":"future"},
            "output_config":{
                "effort":"future_effort",
                "format":{"type":"future_format","mode":"strict"}
            }
        });
        let chat = prepare_for_chat(body, "provider-model").unwrap();
        assert!(chat["messages"].as_array().unwrap().is_empty());
        assert!(chat.get("thinking").is_none());
        assert!(chat.get("reasoning_effort").is_none());
        assert!(chat.get("tools").is_none());
        assert!(chat.get("tool_choice").is_none());
        assert!(chat.get("response_format").is_none());

        let message = prepare_from_chat(
            json!({
                "model":LOGICAL_MODEL,
                "messages":[{"role":"user","content":"hello"}],
                "frequency_penalty":0.5,
                "user":"opaque-user",
                "tools":[{"type":"future_tool","function":{"name":"future"}}],
                "response_format":{"type":"future_format","mode":"strict"},
                "tool_choice":{"type":"future_choice","name":"future"}
            }),
            "provider-model",
        )
        .unwrap();
        assert!(message.get("frequency_penalty").is_none());
        assert!(message.get("output_config").is_none());
        assert!(message.get("tool_choice").is_none());
        assert!(message.get("tools").is_none());
        assert_eq!(message["metadata"]["user_id"], "opaque-user");
    }

    #[test]
    fn anthropic_source_stream_preserves_reasoning_tools_and_usage() {
        let mut stream = AnthropicToChatStream::new("public-chat-model");
        let start = stream.translate(&json!({
            "type":"message_start","message":{
                "id":"msg-source","model":"provider-model",
                "usage":{"input_tokens":12,"cache_read_input_tokens":8}
            }
        }));
        assert_eq!(start[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(start[0]["model"], "public-chat-model");
        stream.translate(&json!({
            "type":"content_block_start","index":0,
            "content_block":{"type":"thinking","thinking":"","signature":""}
        }));
        let reasoning = stream.translate(&json!({
            "type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"think"}
        }));
        assert_eq!(
            reasoning[0]["choices"][0]["delta"]["reasoning_content"],
            "think"
        );
        let tool = stream.translate(&json!({
            "type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"tool-1","name":"lookup","input":{}}
        }));
        assert_eq!(
            tool[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "tool-1"
        );
        let arguments = stream.translate(&json!({
            "type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}
        }));
        assert_eq!(
            arguments[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        let finish = stream.translate(&json!({
            "type":"message_delta","delta":{"stop_reason":"tool_use"},
            "usage":{"output_tokens":5}
        }));
        assert_eq!(finish[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(finish[0]["usage"]["prompt_tokens"], 20);
        assert_eq!(finish[0]["usage"]["total_tokens"], 25);
        assert_eq!(
            finish[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
            8
        );
    }
}
