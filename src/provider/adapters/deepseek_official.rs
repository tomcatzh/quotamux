use reqwest::{RequestBuilder, header::HeaderMap};

use crate::{config::AdapterKind, types::Protocol};

use super::{EndpointPolicy, ProviderAdapter, anthropic_auth, bearer_auth, standard_url};

pub(super) static DEEPSEEK_OFFICIAL: DeepSeekOfficial = DeepSeekOfficial;

pub(super) struct DeepSeekOfficial;

impl ProviderAdapter for DeepSeekOfficial {
    fn kind(&self) -> AdapterKind {
        AdapterKind::DeepSeekOfficial
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::Official("https://api.deepseek.com")
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        matches!(
            protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages
        )
    }

    fn build_url(&self, endpoint: &str, protocol: Protocol) -> String {
        if protocol == Protocol::AnthropicMessages {
            format!("{}/anthropic/v1/messages", endpoint.trim_end_matches('/'))
        } else {
            standard_url(endpoint, protocol)
        }
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
}
