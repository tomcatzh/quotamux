use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    OpenCodeGo,
    DeepSeek,
}

impl Provider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenCodeGo => "opencode-go",
            Self::DeepSeek => "deepseek",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
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
                .unwrap_or(prompt + output),
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
                .unwrap_or(input + output),
            provider_reported: true,
        }
    }

    pub fn from_anthropic(value: &Value) -> Self {
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
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Self {
            input_tokens: input,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: input.saturating_sub(cache_hit),
            reasoning_tokens: 0,
            output_tokens: output,
            total_tokens: input + output,
            provider_reported: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestRecord {
    pub id: String,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub protocol: Protocol,
    pub requested_model: String,
    pub streaming: bool,
    pub status: u16,
    pub error_class: Option<FailureClass>,
    pub provider: Option<Provider>,
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
    pub provider: Provider,
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
    pub provider: Provider,
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
}
