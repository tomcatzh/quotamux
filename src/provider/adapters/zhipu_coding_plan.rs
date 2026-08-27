use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};

use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};

use super::{EndpointPolicy, ProviderAdapter, anthropic_auth, bearer_auth};
use crate::provider::ErrorDetails;

pub(super) static ZHIPU_CODING_PLAN: ZhipuCodingPlan = ZhipuCodingPlan;

pub(super) struct ZhipuCodingPlan;

impl ProviderAdapter for ZhipuCodingPlan {
    fn kind(&self) -> AdapterKind {
        AdapterKind::ZhipuCodingPlan
    }

    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::Official("https://open.bigmodel.cn/api")
    }

    fn supports_protocol(&self, protocol: Protocol) -> bool {
        matches!(
            protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages
        )
    }

    fn build_url(&self, endpoint: &str, protocol: Protocol) -> String {
        let base = endpoint.trim_end_matches('/');
        match protocol {
            Protocol::OpenAiChat => format!("{base}/coding/paas/v4/chat/completions"),
            Protocol::AnthropicMessages => format!("{base}/anthropic/v1/messages"),
            Protocol::OpenAiResponses => format!("{base}/v1/responses"),
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

    fn classify_error(&self, status: StatusCode, details: &ErrorDetails) -> FailureClass {
        match details.error_type.as_deref().unwrap_or_default() {
            "1000" | "1001" | "1002" | "1003" | "1004" | "1005" => FailureClass::ProviderAuth,
            "1113" | "1309" | "1314" => FailureClass::ProviderBilling,
            "1110" | "1111" | "1112" | "1121" | "1211" | "1212" | "1220" | "1221" | "1222"
            | "1311" | "1315" => FailureClass::ProviderConfiguration,
            "1120" | "1200" | "1230" | "1234" => FailureClass::ProviderTransient,
            "1210" | "1213" | "1214" | "1215" | "1261" | "1301" => FailureClass::ClientRequest,
            "1302" | "1305" => FailureClass::ProviderCapacity,
            "1304" | "1308" | "1310" | "1313" | "1316" | "1317" | "1318" | "1319" | "1320"
            | "1321" => FailureClass::ProviderQuota,
            _ => super::super::classify_status(status, &details.classification_message),
        }
    }
}
