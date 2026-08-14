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
    Ok(())
}

pub fn set_model(body: &mut Value, model: &str) {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), Value::String(model.into()));
    }
}
