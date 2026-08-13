use serde_json::Value;

use super::{
    ValidationError, require_reasoning_for_tool_history, set_model, thinking_enabled,
    validate_model,
};

pub fn prepare(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
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
    require_reasoning_for_tool_history(messages, thinking_enabled(&body))?;
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        body.as_object_mut().expect("validated object").insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    set_model(&mut body, upstream_model);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_reasoning_history() {
        let body = json!({
            "model":"deepseek-v4-flash-0731",
            "messages":[
                {"role":"assistant","reasoning_content":"thought","tool_calls":[{"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"x","content":"ok"}
            ]
        });
        let prepared = prepare(body, "deepseek-v4-flash").unwrap();
        assert_eq!(prepared["messages"][0]["reasoning_content"], "thought");
    }

    #[test]
    fn rejects_missing_reasoning_history() {
        let body = json!({
            "model":"deepseek-v4-flash-0731",
            "messages":[{"role":"assistant","tool_calls":[{"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]}]
        });
        assert!(prepare(body, "deepseek-v4-flash").is_err());
    }

    #[test]
    fn preserves_named_tool_choice_for_upstream_validation() {
        let body = json!({
            "model":"kimi-k3",
            "messages":[{"role":"user","content":"hi"}],
            "reasoning_effort":"high",
            "tool_choice":{"type":"function","function":{"name":"f"}}
        });
        let prepared = prepare(body, "kimi-k3").unwrap();
        assert_eq!(prepared["reasoning_effort"], "high");
        assert_eq!(prepared["tool_choice"]["function"]["name"], "f");
    }
}
