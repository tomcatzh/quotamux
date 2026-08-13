use serde_json::Value;

use super::{ValidationError, set_model, validate_model};

pub fn prepare(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
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
    fn preserves_missing_reasoning_history_for_upstream_validation() {
        let body = json!({
            "model":"kimi-k3",
            "reasoning_effort":"high",
            "messages":[
                {"role":"assistant","tool_calls":[{"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"x","content":"ok"}
            ]
        });
        let prepared = prepare(body, "kimi-k3").unwrap();
        assert_eq!(prepared["reasoning_effort"], "high");
        assert_eq!(
            prepared["messages"][0]["tool_calls"][0]["function"]["name"],
            "f"
        );
        assert!(prepared["messages"][0].get("reasoning_content").is_none());
        assert_eq!(prepared["messages"][1]["tool_call_id"], "x");
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

    #[test]
    fn leaves_semantically_invalid_chat_body_for_upstream_validation() {
        let body = json!({
            "model":"kimi-k3",
            "messages":"not-an-array",
            "temperature":"not-a-number"
        });
        let prepared = prepare(body, "kimi-k3").unwrap();
        assert_eq!(prepared["messages"], "not-an-array");
        assert_eq!(prepared["temperature"], "not-a-number");
    }
}
