use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::AdapterKind;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Protocol {
    #[serde(rename = "openai-chat", alias = "open-ai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses", alias = "open-ai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

impl Protocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ClientRequest,
    ProviderAuth,
    ProviderBilling,
    ProviderConfiguration,
    ProviderCapacity,
    ProviderQuota,
    ProviderTransient,
    StreamFailure,
    ProviderUnknown4xx,
    ProviderUnknown5xxOrTransport,
    ClientCancelled,
    FallbackUnavailable,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClientRequest => "client_request",
            Self::ProviderAuth => "provider_auth",
            Self::ProviderBilling => "provider_billing",
            Self::ProviderConfiguration => "provider_configuration",
            Self::ProviderCapacity => "provider_capacity",
            Self::ProviderQuota => "provider_quota",
            Self::ProviderTransient => "provider_transient",
            Self::StreamFailure => "stream_failure",
            Self::ProviderUnknown4xx => "provider_unknown_4xx",
            Self::ProviderUnknown5xxOrTransport => "provider_unknown_5xx_or_transport",
            Self::ClientCancelled => "client_cancelled",
            Self::FallbackUnavailable => "fallback_unavailable",
        }
    }

    pub const fn allows_fallback(self) -> bool {
        !matches!(self, Self::ClientRequest | Self::ClientCancelled)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub provider_reported: bool,
}

impl Usage {
    pub fn from_openai(value: &Value) -> Self {
        let Some(usage) = value.get("usage") else {
            return Self::default();
        };
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
        let completion_details = usage
            .get("completion_tokens_details")
            .unwrap_or(&Value::Null);
        let cache_hit = usage
            .get("prompt_cache_hit_tokens")
            .and_then(Value::as_u64)
            .or_else(|| details.get("cached_tokens").and_then(Value::as_u64))
            .unwrap_or(0);
        let cache_miss = usage
            .get("prompt_cache_miss_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| prompt.saturating_sub(cache_hit));
        let reasoning = completion_details
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Self {
            input_tokens: prompt,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: cache_miss,
            reasoning_tokens: reasoning,
            output_tokens: output,
            total_tokens: usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| prompt.saturating_add(output)),
            provider_reported: true,
        }
    }

    pub fn as_json(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "cache_hit_tokens": self.cache_hit_tokens,
            "cache_miss_tokens": self.cache_miss_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "provider_reported": self.provider_reported,
        })
    }

    pub fn from_responses(value: &Value) -> Self {
        let Some(usage) = value.get("usage") else {
            return Self::default();
        };
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_hit = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Self {
            input_tokens: input,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: input.saturating_sub(cache_hit),
            reasoning_tokens: reasoning,
            output_tokens: output,
            total_tokens: usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| input.saturating_add(output)),
            provider_reported: true,
        }
    }

    pub fn from_anthropic(value: &Value) -> Self {
        let Some(usage) = value.get("usage") else {
            return Self::default();
        };
        let uncached_input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_hit = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_miss = uncached_input.saturating_add(cache_creation);
        let input = cache_miss.saturating_add(cache_hit);
        Self {
            input_tokens: input,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: cache_miss,
            reasoning_tokens: 0,
            output_tokens: output,
            total_tokens: input.saturating_add(output),
            provider_reported: true,
        }
    }

    pub fn observe_anthropic_stream_event(&mut self, event: &Value) {
        let event_type = event.get("type").and_then(Value::as_str);
        let usage = if event_type == Some("message_start") {
            event.pointer("/message/usage")
        } else {
            event.get("usage")
        };
        let Some(usage) = usage.filter(|usage| !usage.is_null()) else {
            return;
        };

        if usage.get("input_tokens").is_some()
            || usage.get("cache_creation_input_tokens").is_some()
            || usage.get("cache_read_input_tokens").is_some()
        {
            let observed = Self::from_anthropic(&json!({"usage":usage}));
            self.input_tokens = observed.input_tokens;
            self.cache_hit_tokens = observed.cache_hit_tokens;
            self.cache_miss_tokens = observed.cache_miss_tokens;
        }
        if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = output;
        }

        self.total_tokens = self.input_tokens.saturating_add(self.output_tokens);
        self.provider_reported = true;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestRecord {
    pub id: String,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub protocol: Protocol,
    pub requested_model: String,
    #[serde(default)]
    pub served_model: Option<String>,
    #[serde(default)]
    pub upstream_model: Option<String>,
    pub streaming: bool,
    pub status: u16,
    pub error_class: Option<FailureClass>,
    #[serde(alias = "provider")]
    pub backend: Option<String>,
    #[serde(default, alias = "provider_kind")]
    pub adapter: Option<AdapterKind>,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub route_layer: Option<String>,
    #[serde(default)]
    pub route_layer_index: Option<usize>,
    #[serde(default)]
    pub selection_reason: Option<String>,
    #[serde(default)]
    pub matched_prefix_bytes: Option<u64>,
    pub fallback: bool,
    pub translated: bool,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub first_byte_ms: Option<u64>,
    pub total_ms: u64,
    pub usage: Usage,
    pub claude_session_id: Option<String>,
    pub claude_agent_id: Option<String>,
    pub claude_parent_agent_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptRecord {
    pub id: String,
    pub request_id: Option<String>,
    pub sequence: u32,
    #[serde(alias = "provider")]
    pub backend: String,
    #[serde(default, alias = "provider_kind")]
    pub adapter: Option<AdapterKind>,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub route_layer: Option<String>,
    #[serde(default)]
    pub route_layer_index: Option<usize>,
    #[serde(default)]
    pub selection_reason: Option<String>,
    #[serde(default)]
    pub matched_prefix_bytes: Option<u64>,
    pub upstream_model: String,
    pub egress_protocol: Protocol,
    pub translated: bool,
    pub probe: bool,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub status: Option<u16>,
    pub error_class: Option<FailureClass>,
    pub retry_after_ms: Option<i64>,
    pub committed: bool,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub first_byte_ms: Option<u64>,
    pub total_ms: u64,
    pub usage: Usage,
    pub provider_cost_usd: Option<f64>,
    pub sanitized_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AlertRecord {
    pub id: String,
    #[serde(alias = "provider")]
    pub backend: String,
    #[serde(default, alias = "provider_kind")]
    pub adapter: Option<AdapterKind>,
    #[serde(default)]
    pub credential: Option<String>,
    pub class: FailureClass,
    pub active: bool,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub next_probe_at_ms: Option<i64>,
    pub request_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_reads_legacy_open_ai_spellings_but_writes_canonical_values() {
        assert_eq!(
            serde_json::from_str::<Protocol>(r#""open-ai-chat""#).unwrap(),
            Protocol::OpenAiChat
        );
        assert_eq!(
            serde_json::from_str::<Protocol>(r#""open-ai-responses""#).unwrap(),
            Protocol::OpenAiResponses
        );
        assert_eq!(
            serde_json::to_string(&Protocol::OpenAiChat).unwrap(),
            r#""openai-chat""#
        );
        assert_eq!(
            serde_json::to_string(&Protocol::OpenAiResponses).unwrap(),
            r#""openai-responses""#
        );
    }

    #[test]
    fn persisted_provider_fields_migrate_to_backend_and_adapter_names() {
        let legacy = serde_json::json!({
            "id": "alert-1",
            "provider": "go",
            "provider_kind": "opencode-go",
            "credential": "go-a",
            "class": "provider_quota",
            "active": true,
            "first_seen_ms": 1,
            "last_seen_ms": 2,
            "next_probe_at_ms": 3,
            "request_id": "request-1"
        });
        let record = serde_json::from_value::<AlertRecord>(legacy).unwrap();
        assert_eq!(record.backend, "go");
        assert_eq!(record.adapter, Some(AdapterKind::OpenCodeGo));

        let current = serde_json::to_value(record).unwrap();
        assert_eq!(current["backend"], "go");
        assert_eq!(current["adapter"], "opencode-go");
        assert!(current.get("provider").is_none());
        assert!(current.get("provider_kind").is_none());
    }

    #[test]
    fn parses_go_cache_usage() {
        let value = json!({"usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            "completion_tokens_details": {"reasoning_tokens": 12}
        }});
        let usage = Usage::from_openai(&value);
        assert_eq!(usage.cache_hit_tokens, 80);
        assert_eq!(usage.cache_miss_tokens, 20);
        assert_eq!(usage.reasoning_tokens, 12);
    }

    #[test]
    fn parses_standard_cached_tokens() {
        let value = json!({"usage": {
            "prompt_tokens": 50,
            "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 40}
        }});
        let usage = Usage::from_openai(&value);
        assert_eq!(usage.cache_hit_tokens, 40);
        assert_eq!(usage.cache_miss_tokens, 10);
    }

    #[test]
    fn provider_usage_totals_saturate_when_upstream_counts_are_malformed() {
        let openai = Usage::from_openai(&json!({"usage": {
            "prompt_tokens": u64::MAX,
            "completion_tokens": 1
        }}));
        assert_eq!(openai.total_tokens, u64::MAX);

        let responses = Usage::from_responses(&json!({"usage": {
            "input_tokens": u64::MAX,
            "output_tokens": 1
        }}));
        assert_eq!(responses.total_tokens, u64::MAX);
    }

    #[test]
    fn parses_anthropic_cache_usage_as_total_input() {
        let value = json!({"usage": {
            "input_tokens": 5,
            "cache_creation_input_tokens": 3,
            "cache_read_input_tokens": 40,
            "output_tokens": 7
        }});
        let usage = Usage::from_anthropic(&value);
        assert_eq!(usage.input_tokens, 48);
        assert_eq!(usage.cache_hit_tokens, 40);
        assert_eq!(usage.cache_miss_tokens, 8);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.total_tokens, 55);
    }

    #[test]
    fn accumulates_anthropic_stream_usage_without_clearing_on_terminal_events() {
        let mut usage = Usage::default();
        for event in [
            json!({"type":"message_start","message":{"usage":{
                "input_tokens":0,"cache_creation_input_tokens":0,
                "cache_read_input_tokens":93,"output_tokens":0
            }}}),
            json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"message_delta","usage":{
                "input_tokens":0,"cache_creation_input_tokens":0,
                "cache_read_input_tokens":93,"output_tokens":32
            }}),
            json!({"type":"message_stop"}),
        ] {
            usage.observe_anthropic_stream_event(&event);
        }
        assert_eq!(usage.input_tokens, 93);
        assert_eq!(usage.cache_hit_tokens, 93);
        assert_eq!(usage.cache_miss_tokens, 0);
        assert_eq!(usage.output_tokens, 32);
        assert_eq!(usage.total_tokens, 125);
        assert!(usage.provider_reported);
    }
}
