use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{config::LOGICAL_MODEL, sse::SseEvent, types::Usage};

use super::{ValidationError, chat, set_model, validate_model};

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
    translate_output_format(&body, &mut chat)?;
    Ok(Value::Object(chat))
}

pub fn prepare_from_chat(body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    let chat = chat::prepare(body, upstream_model)?;
    reject_chat_fields(
        &chat,
        &[
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
        "Anthropic Messages",
    )?;
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
        if role == "assistant"
            && let Some(reasoning) = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        {
            content.push(json!({
                "type":"thinking",
                "thinking":reasoning,
                "signature":"quotamux"
            }));
        }
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
                    "input":serde_json::from_str::<Value>(arguments).unwrap_or_else(|_|json!({}))
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
        chat.get("max_tokens")
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );
    if !system.is_empty() {
        anthropic.insert("system".into(), Value::String(system.join("\n\n")));
    }
    copy_field(&chat, &mut anthropic, "stream", "stream");
    copy_field(&chat, &mut anthropic, "temperature", "temperature");
    copy_field(&chat, &mut anthropic, "top_p", "top_p");
    copy_field(&chat, &mut anthropic, "stop", "stop_sequences");
    if let Some(thinking) = chat.get("thinking") {
        anthropic.insert("thinking".into(), thinking.clone());
    }
    let mut output_config = Map::new();
    if let Some(effort) = chat.get("reasoning_effort") {
        output_config.insert("effort".into(), effort.clone());
    }
    if let Some(format) = chat.get("response_format") {
        output_config.insert("format".into(), chat_format_to_anthropic(format)?);
    }
    if !output_config.is_empty() {
        anthropic.insert("output_config".into(), Value::Object(output_config));
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        anthropic.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .filter_map(|tool| tool.get("function"))
                    .map(|function| {
                        json!({
                            "name":function.get("name").cloned().unwrap_or(Value::String(String::new())),
                            "description":function.get("description").cloned().unwrap_or(Value::String(String::new())),
                            "input_schema":function.get("parameters").cloned().unwrap_or_else(||json!({"type":"object","properties":{}})),
                            "strict":function.get("strict").cloned().unwrap_or(Value::Bool(false))
                        })
                    })
                    .collect(),
            ),
        );
    }
    if chat.get("tool_choice").is_some() || chat.get("parallel_tool_calls").is_some() {
        anthropic.insert(
            "tool_choice".into(),
            chat_tool_choice_to_anthropic(
                chat.get("tool_choice"),
                chat.get("parallel_tool_calls").and_then(Value::as_bool),
            )?,
        );
    }
    Ok(Value::Object(anthropic))
}

fn chat_format_to_anthropic(format: &Value) -> Result<Value, ValidationError> {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = format
                .pointer("/json_schema/schema")
                .cloned()
                .ok_or_else(|| {
                    ValidationError::invalid(
                        "response_format.json_schema.schema is required",
                        Some("response_format"),
                    )
                })?;
            Ok(json!({"type":"json_schema","schema":schema}))
        }
        Some(kind) => Err(ValidationError::invalid(
            format!(
                "Anthropic Messages cannot represent Chat response_format type {kind} without loss"
            ),
            Some("response_format"),
        )),
        None => Err(ValidationError::invalid(
            "response_format.type is required",
            Some("response_format"),
        )),
    }
}

fn chat_tool_choice_to_anthropic(
    choice: Option<&Value>,
    parallel: Option<bool>,
) -> Result<Value, ValidationError> {
    let mut translated = match choice {
        None => json!({"type":"auto"}),
        Some(Value::String(value)) if value == "auto" => json!({"type":"auto"}),
        Some(Value::String(value)) if value == "none" => json!({"type":"none"}),
        Some(Value::String(value)) if value == "required" => json!({"type":"any"}),
        Some(value) if value.get("type").and_then(Value::as_str) == Some("function") => {
            let name = value
                .pointer("/function/name")
                .or_else(|| value.get("name"))
                .cloned()
                .ok_or_else(|| {
                    ValidationError::invalid(
                        "named tool_choice requires a function name",
                        Some("tool_choice"),
                    )
                })?;
            json!({"type":"tool","name":name})
        }
        _ => {
            return Err(ValidationError::invalid(
                "unsupported Chat tool_choice for Anthropic Messages",
                Some("tool_choice"),
            ));
        }
    };
    if parallel == Some(false) {
        translated
            .as_object_mut()
            .expect("tool choice is an object")
            .insert("disable_parallel_tool_use".into(), Value::Bool(true));
    }
    Ok(translated)
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

fn translate_output_format(
    body: &Value,
    chat: &mut Map<String, Value>,
) -> Result<(), ValidationError> {
    let Some(format) = body.pointer("/output_config/format") else {
        return Ok(());
    };
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Err(ValidationError::invalid(
            "unsupported Anthropic output_config.format type",
            Some("output_config"),
        ));
    }
    let schema = format.get("schema").cloned().ok_or_else(|| {
        ValidationError::invalid(
            "output_config.format.schema is required",
            Some("output_config"),
        )
    })?;
    chat.insert(
        "response_format".into(),
        json!({
            "type":"json_schema",
            "json_schema":{"name":"response","schema":schema,"strict":true}
        }),
    );
    Ok(())
}

fn translate_tools(body: &Value, chat: &mut Map<String, Value>) -> Result<(), ValidationError> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let tools = tools.iter().map(|tool| Ok(json!({"type":"function","function":{
            "name":tool.get("name").cloned().unwrap_or(Value::String(String::new())),
            "description":tool.get("description").cloned().unwrap_or(Value::String(String::new())),
            "parameters":tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
            "strict":tool.get("strict").cloned().unwrap_or(Value::Bool(false))
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
        if choice
            .get("disable_parallel_tool_use")
            .and_then(Value::as_bool)
            == Some(true)
        {
            chat.insert("parallel_tool_calls".into(), Value::Bool(false));
        }
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

pub fn message_to_chat(message: &Value) -> Value {
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
        "model":message.get("model").cloned().unwrap_or(Value::String(LOGICAL_MODEL.into())),
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

pub struct AnthropicToChatStream {
    id: String,
    model: String,
    blocks: std::collections::BTreeMap<u64, AnthropicBlock>,
    next_call_index: u64,
    input_tokens: u64,
    cache_hit_tokens: u64,
}

enum AnthropicBlock {
    Thinking,
    Text,
    Tool { call_index: u64 },
}

impl Default for AnthropicToChatStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicToChatStream {
    pub fn new() -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::now_v7()),
            model: LOGICAL_MODEL.into(),
            blocks: Default::default(),
            next_call_index: 0,
            input_tokens: 0,
            cache_hit_tokens: 0,
        }
    }

    pub fn translate(&mut self, event: &Value) -> Vec<Value> {
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let message = event.get("message").unwrap_or(&Value::Null);
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    self.id = id.replacen("msg_", "chatcmpl-", 1);
                }
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    self.model = model.into();
                }
                self.input_tokens = message
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.cache_hit_tokens = message
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
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
                let output_tokens = event
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
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
                        "prompt_tokens":self.input_tokens,
                        "completion_tokens":output_tokens,
                        "total_tokens":self.input_tokens+output_tokens,
                        "prompt_tokens_details":{"cached_tokens":self.cache_hit_tokens}
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
    fn anthropic_source_stream_preserves_reasoning_tools_and_usage() {
        let mut stream = AnthropicToChatStream::new();
        let start = stream.translate(&json!({
            "type":"message_start","message":{
                "id":"msg-source","model":"provider-model",
                "usage":{"input_tokens":12,"cache_read_input_tokens":8}
            }
        }));
        assert_eq!(start[0]["choices"][0]["delta"]["role"], "assistant");
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
        assert_eq!(
            finish[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
            8
        );
    }
}
