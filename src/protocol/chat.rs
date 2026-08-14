use serde_json::Value;

use super::{ValidationError, set_model, validate_model};

pub fn prepare(mut body: Value, upstream_model: &str) -> Result<Value, ValidationError> {
    validate_model(&body)?;
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let object = body.as_object_mut().expect("validated object");
        match object.get_mut("stream_options") {
            Some(Value::Object(options)) => {
                options.insert("include_usage".into(), Value::Bool(true));
            }
            None | Some(Value::Null) => {
                object.insert(
                    "stream_options".into(),
                    serde_json::json!({"include_usage": true}),
                );
            }
            Some(_) => {}
        }
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

    #[test]
    fn merges_usage_into_valid_stream_options_without_repairing_invalid_values() {
        let prepared = prepare(
            json!({
                "model":"logical",
                "messages":[],
                "stream":true,
                "stream_options":{"include_obfuscation":false}
            }),
            "provider-model",
        )
        .unwrap();
        assert_eq!(prepared["stream_options"]["include_usage"], true);
        assert_eq!(prepared["stream_options"]["include_obfuscation"], false);

        let invalid = prepare(
            json!({"model":"logical","messages":[],"stream":true,"stream_options":"invalid"}),
            "provider-model",
        )
        .unwrap();
        assert_eq!(invalid["stream_options"], "invalid");
    }
}
