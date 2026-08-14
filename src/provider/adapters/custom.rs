use reqwest::{RequestBuilder, header::HeaderMap};

use crate::{config::AdapterKind, types::Protocol};

use super::{EndpointPolicy, ProviderAdapter, anthropic_auth, bearer_auth};

pub(super) static CUSTOM_CHAT_COMPLETIONS: Custom = Custom {
    kind: AdapterKind::CustomChatCompletions,
    protocol: Protocol::OpenAiChat,
};
pub(super) static CUSTOM_RESPONSES: Custom = Custom {
    kind: AdapterKind::CustomResponses,
    protocol: Protocol::OpenAiResponses,
};
pub(super) static CUSTOM_ANTHROPIC: Custom = Custom {
    kind: AdapterKind::CustomAnthropic,
    protocol: Protocol::AnthropicMessages,
};

pub(super) struct Custom {
    kind: AdapterKind,
    protocol: Protocol,
}

impl ProviderAdapter for Custom {
    fn kind(&self) -> AdapterKind {
        self.kind
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::ConfiguredExact
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        protocol == self.protocol
    }

    fn build_url(&self, endpoint: &str, _protocol: Protocol) -> String {
        endpoint.to_owned()
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
