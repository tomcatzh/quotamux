use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};

use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};

use super::{
    EndpointPolicy, ModelProtocolPolicy, ProviderAdapter, bearer_auth, contains_any, standard_url,
};
use crate::provider::ErrorDetails;

pub(super) static OPENCODE_GO: OpenCodeGo = OpenCodeGo;

pub(super) struct OpenCodeGo;

impl ProviderAdapter for OpenCodeGo {
    fn kind(&self) -> AdapterKind {
        AdapterKind::OpenCodeGo
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::Official("https://opencode.ai/zen/go/v1")
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        matches!(
            protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages
        )
    }

    fn model_protocol_policy(&self) -> ModelProtocolPolicy {
        ModelProtocolPolicy::OfficialCatalog
    }

    fn protocol_for_model(&self, model: &str) -> Option<Protocol> {
        match model {
            "grok-4.5" | "gpt-5.6-luna" => Some(Protocol::OpenAiResponses),
            "glm-5.3" | "glm-5.2" | "glm-5.1" | "kimi-k3" | "kimi-k2.7-code" | "kimi-k2.6"
            | "deepseek-v4-pro" | "deepseek-v4-flash" | "mimo-v2.5" | "mimo-v2.5-pro" | "hy3" => {
                Some(Protocol::OpenAiChat)
            }
            "minimax-m3" | "minimax-m2.7" | "minimax-m2.5" | "qwen3.8-max" | "qwen3.7-max"
            | "qwen3.7-plus" | "qwen3.6-plus" => Some(Protocol::AnthropicMessages),
            _ => None,
        }
    }

    fn build_url(&self, endpoint: &str, protocol: Protocol) -> String {
        standard_url(endpoint, protocol)
    }

    fn apply_auth(
        &self,
        request: RequestBuilder,
        api_key: &str,
        _protocol: Protocol,
        _inbound_headers: &HeaderMap,
    ) -> RequestBuilder {
        bearer_auth(request, api_key)
    }

    fn classify_error(&self, status: StatusCode, details: &ErrorDetails) -> FailureClass {
        let message = details.classification_message.as_str();
        if details
            .error_type
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("GoUsageLimitError"))
            || contains_any(message, &["subscription quota exceeded"])
        {
            return FailureClass::ProviderQuota;
        }
        if contains_any(
            message,
            &[
                "does not have access",
                "model is not available",
                "model not found",
                "unsupported model",
            ],
        ) {
            return FailureClass::ProviderConfiguration;
        }
        super::super::classify_status(status, message)
    }
}
