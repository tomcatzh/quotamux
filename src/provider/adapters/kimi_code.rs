use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};

use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};

use super::{
    EndpointPolicy, ProviderAdapter, anthropic_auth, bearer_auth, contains_any, standard_url,
};
use crate::provider::ErrorDetails;

pub(super) static KIMI_CODE: KimiCode = KimiCode;

pub(super) struct KimiCode;

impl ProviderAdapter for KimiCode {
    fn kind(&self) -> AdapterKind {
        AdapterKind::KimiCode
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::Official("https://api.kimi.com/coding/v1")
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        matches!(protocol, Protocol::OpenAiChat | Protocol::AnthropicMessages)
    }

    fn build_url(&self, endpoint: &str, protocol: Protocol) -> String {
        standard_url(endpoint, protocol)
    }

    fn apply_auth(
        &self,
        request: RequestBuilder,
        api_key: &str,
        protocol: Protocol,
        inbound_headers: &HeaderMap,
    ) -> RequestBuilder {
        if protocol == Protocol::AnthropicMessages {
            anthropic_auth(request, api_key, inbound_headers)
        } else {
            bearer_auth(request, api_key)
        }
    }

    fn classify_error(&self, status: StatusCode, details: &ErrorDetails) -> FailureClass {
        let message = details.classification_message.as_str();
        if contains_any(message, &["bot_id"]) && contains_any(message, &["does not match id_kinds"])
        {
            return FailureClass::ClientRequest;
        }
        if contains_any(
            message,
            &["未找到该账号", "该账号已被禁用", "已被暂时禁用", "已被禁言"],
        ) {
            return FailureClass::ProviderConfiguration;
        }
        match status.as_u16() {
            401 if contains_any(
                message,
                &[
                    "does not have access",
                    "supports only",
                    "model id does not exist",
                ],
            ) =>
            {
                FailureClass::ProviderConfiguration
            }
            402 => FailureClass::ProviderTransient,
            403 if contains_any(message, &["usage limit", "quota", "billing cycle"]) => {
                FailureClass::ProviderQuota
            }
            403 if contains_any(message, &["security risk", "invalid_url"]) => {
                FailureClass::ClientRequest
            }
            403 if contains_any(message, &["access terminated"]) => {
                FailureClass::ProviderConfiguration
            }
            429 if contains_any(
                message,
                &[
                    "usage limit for this period",
                    "monthly usage limit",
                    "quota will be refreshed",
                ],
            ) =>
            {
                FailureClass::ProviderQuota
            }
            429 => FailureClass::ProviderCapacity,
            _ => super::super::classify_status(status, message),
        }
    }
}
