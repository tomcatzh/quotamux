pub mod anthropic;
pub mod chat;
pub mod responses;

use axum::http::StatusCode;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct ValidationError {
    pub status: StatusCode,
    pub message: String,
    pub param: Option<&'static str>,
}

impl ValidationError {
    pub fn invalid(message: impl Into<String>, param: Option<&'static str>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            param,
        }
    }

    pub fn response(&self) -> Value {
        json!({"error": {
            "message": self.message,
            "type": "invalid_request_error",
            "param": self.param,
            "code": "invalid_request_error"
        }})
    }
}

pub fn model_name(body: &Value) -> Result<&str, ValidationError> {
    body.get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationError::invalid("model is required", Some("model")))
}

pub fn validate_model(body: &Value) -> Result<(), ValidationError> {
    model_name(body)?;
    if body.get("provider").is_some() {
        return Err(ValidationError::invalid(
            "clients cannot select a provider",
            Some("provider"),
        ));
    }
    Ok(())
}

pub fn thinking_enabled(body: &Value) -> bool {
    if body.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled") {
        return false;
    }
    if body.pointer("/reasoning/effort").and_then(Value::as_str) == Some("none") {
        return false;
    }
    true
}

pub fn validate_named_tool_choice(body: &Value) -> Result<(), ValidationError> {
    if !thinking_enabled(body) {
        return Ok(());
    }
    let named = body.get("tool_choice").is_some_and(|choice| {
        choice.is_object()
            && (choice.pointer("/function/name").is_some()
                || choice.get("name").is_some()
                || choice.get("type").and_then(Value::as_str) == Some("tool"))
    });
    if named {
        return Err(ValidationError::invalid(
            "DeepSeek V4 thinking mode does not support named tool_choice; use auto/required or disable thinking",
            Some("tool_choice"),
        ));
    }
    Ok(())
}

pub fn require_reasoning_for_tool_history(
    messages: &[Value],
    thinking: bool,
) -> Result<(), ValidationError> {
    if !thinking {
        return Ok(());
    }
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
            && message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(ValidationError::invalid(
                "assistant tool-call history must include the complete reasoning_content in thinking mode",
                Some("messages"),
            ));
        }
    }
    Ok(())
}

pub fn set_model(body: &mut Value, model: &str) {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), Value::String(model.into()));
        object.remove("provider");
    }
}
