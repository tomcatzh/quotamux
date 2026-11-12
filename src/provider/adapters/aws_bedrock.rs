use super::{EndpointPolicy, ModelProtocolPolicy, ProviderAdapter};
use crate::provider::ErrorDetails;
use crate::{
    config::AdapterKind,
    types::{FailureClass, Protocol},
};
use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};
pub(super) static AWS_BEDROCK: AwsBedrock = AwsBedrock;
pub(super) struct AwsBedrock;
impl ProviderAdapter for AwsBedrock {
    fn kind(&self) -> AdapterKind {
        AdapterKind::AwsBedrock
    }
    fn endpoint_policy(&self) -> EndpointPolicy {
        EndpointPolicy::AwsRegional
    }
    fn supports_protocol(&self, protocol: Protocol) -> bool {
        protocol == Protocol::AnthropicMessages
    }
    fn model_protocol_policy(&self) -> ModelProtocolPolicy {
        ModelProtocolPolicy::OfficialCatalog
    }
    fn protocol_for_model(&self, model: &str) -> Option<Protocol> {
        matches!(
            model,
            "global.anthropic.claude-opus-5-5"
                | "global.anthropic.claude-sonnet-5-5"
                | "global.anthropic.claude-haiku-5-5"
                | "global.anthropic.claude-fable-5-2"
        )
        .then_some(Protocol::AnthropicMessages)
    }
    fn build_url(&self, endpoint: &str, _: Protocol) -> String {
        endpoint.to_owned()
    }
    fn apply_auth(
        &self,
        request: RequestBuilder,
        _: &str,
        _: Protocol,
        _: &HeaderMap,
    ) -> RequestBuilder {
        request
    }
    fn classify_error(&self, status: StatusCode, details: &ErrorDetails) -> FailureClass {
        classify(details.error_type.as_deref().unwrap_or_default(), status)
    }
}
pub(crate) fn classify(code: &str, status: StatusCode) -> FailureClass {
    match code.to_ascii_lowercase().as_str() {
        "validationexception" => FailureClass::ClientRequest,
        "accessdeniedexception"
        | "unrecognizedclientexception"
        | "expiredtokenexception"
        | "invalidsignatureexception"
        | "incompletesignature"
        | "credentialserror" => FailureClass::ProviderAuth,
        "resourcenotfoundexception" => FailureClass::ProviderConfiguration,
        "servicequotaexceededexception" | "throttlingexception" | "modelnotreadyexception" => {
            FailureClass::ProviderCapacity
        }
        "modeltimeoutexception"
        | "modelerrorexception"
        | "modelstreamerrorexception"
        | "serviceunavailableexception"
        | "internalserverexception" => FailureClass::ProviderTransient,
        _ => crate::provider::classify_status(status, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aws_errors_keep_request_failures_out_of_provider_circuits() {
        for (code, expected) in [
            ("ValidationException", FailureClass::ClientRequest),
            ("AccessDeniedException", FailureClass::ProviderAuth),
            ("ExpiredTokenException", FailureClass::ProviderAuth),
            (
                "ResourceNotFoundException",
                FailureClass::ProviderConfiguration,
            ),
            (
                "ServiceQuotaExceededException",
                FailureClass::ProviderCapacity,
            ),
            ("ThrottlingException", FailureClass::ProviderCapacity),
            ("ModelNotReadyException", FailureClass::ProviderCapacity),
            ("ModelStreamErrorException", FailureClass::ProviderTransient),
            ("ModelTimeoutException", FailureClass::ProviderTransient),
            ("modelStreamErrorException", FailureClass::ProviderTransient),
            ("throttlingException", FailureClass::ProviderCapacity),
            ("validationException", FailureClass::ClientRequest),
        ] {
            assert_eq!(classify(code, StatusCode::BAD_REQUEST), expected, "{code}");
        }
    }
}
