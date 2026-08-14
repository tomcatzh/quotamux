use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};

use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};

use super::{EndpointPolicy, ProviderAdapter, bearer_auth, standard_url};
use crate::provider::ErrorDetails;

pub(super) static KIMI_OFFICIAL: KimiOfficial = KimiOfficial;

pub(super) struct KimiOfficial;

impl ProviderAdapter for KimiOfficial {
    fn kind(&self) -> AdapterKind {
        AdapterKind::KimiOfficial
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::Official("https://api.moonshot.cn/v1")
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        protocol == Protocol::OpenAiChat
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
        match details
            .error_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "engine_overloaded_error" | "rate_limit_reached_error" => {
                FailureClass::ProviderCapacity
            }
            "exceeded_current_quota_error" => FailureClass::ProviderBilling,
            "invalid_authentication_error" | "incorrect_api_key_error" => {
                FailureClass::ProviderAuth
            }
            "permission_denied_error" | "resource_not_found_error" => {
                FailureClass::ProviderConfiguration
            }
            "content_filter" | "invalid_request_error" => FailureClass::ClientRequest,
            "server_error" | "unexpected_output" | "server_unavailable" => {
                FailureClass::ProviderTransient
            }
            _ => super::super::classify_status(status, &details.classification_message),
        }
    }
}
