use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};

use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};

use super::{ErrorDetails, classify_status};

mod custom;
mod deepseek_official;
mod kimi_code;
mod kimi_official;
mod opencode_go;
mod zhipu_coding_plan;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointPolicy {
    Official(&'static str),
    ConfiguredExact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelProtocolPolicy {
    Listed,
    OfficialCatalog,
}

pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn endpoint_policy(&self) -> EndpointPolicy;
    fn supports_protocol(&self, protocol: Protocol) -> bool;

    fn model_protocol_policy(&self) -> ModelProtocolPolicy {
        ModelProtocolPolicy::Listed
    }

    fn protocol_for_model(&self, _model: &str) -> Option<Protocol> {
        None
    }

    fn build_url(&self, endpoint: &str, protocol: Protocol) -> String;

    fn apply_auth(
        &self,
        request: RequestBuilder,
        api_key: &str,
        protocol: Protocol,
        inbound_headers: &HeaderMap,
    ) -> RequestBuilder;

    fn classify_error(&self, status: StatusCode, details: &ErrorDetails) -> FailureClass {
        classify_status(status, &details.classification_message)
    }
}

pub fn adapter_for(kind: AdapterKind) -> Option<&'static dyn ProviderAdapter> {
    match kind {
        AdapterKind::DeepSeekOfficial => Some(&deepseek_official::DEEPSEEK_OFFICIAL),
        AdapterKind::KimiOfficial => Some(&kimi_official::KIMI_OFFICIAL),
        AdapterKind::KimiCode => Some(&kimi_code::KIMI_CODE),
        AdapterKind::OpenCodeGo => Some(&opencode_go::OPENCODE_GO),
        AdapterKind::ZhipuCodingPlan => Some(&zhipu_coding_plan::ZHIPU_CODING_PLAN),
        AdapterKind::CustomChatCompletions => Some(&custom::CUSTOM_CHAT_COMPLETIONS),
        AdapterKind::CustomResponses => Some(&custom::CUSTOM_RESPONSES),
        AdapterKind::CustomAnthropic => Some(&custom::CUSTOM_ANTHROPIC),
        AdapterKind::AliyunBailian | AdapterKind::OllamaCloud | AdapterKind::OpenCodeZen => None,
    }
}

pub(super) fn bearer_auth(request: RequestBuilder, api_key: &str) -> RequestBuilder {
    request.bearer_auth(api_key)
}

pub(super) fn anthropic_auth(
    mut request: RequestBuilder,
    api_key: &str,
    inbound_headers: &HeaderMap,
) -> RequestBuilder {
    request = request.header("x-api-key", api_key).header(
        "anthropic-version",
        inbound_headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("2023-06-01"),
    );
    if let Some(beta) = inbound_headers.get("anthropic-beta") {
        request = request.header("anthropic-beta", beta);
    }
    request
}

pub(super) fn standard_url(endpoint: &str, protocol: Protocol) -> String {
    let base = endpoint.trim_end_matches('/');
    match protocol {
        Protocol::OpenAiChat => format!("{base}/chat/completions"),
        Protocol::OpenAiResponses => format!("{base}/responses"),
        Protocol::AnthropicMessages => format!("{base}/messages"),
    }
}

pub(super) fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let haystack = haystack.to_ascii_lowercase();
    needles.iter().any(|needle| haystack.contains(needle))
}
