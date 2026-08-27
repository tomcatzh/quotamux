use std::time::Duration;

use reqwest::{
    Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde_json::Value;
use thiserror::Error;

use crate::{
    config::{
        AdapterKind, BackendConfig, BackendModelConfig, CredentialConfig, ModelPricingConfig,
        ServerTimeoutConfig,
    },
    types::{FailureClass, Protocol},
};

mod adapters;

use adapters::ProviderAdapter;
pub(crate) use adapters::{EndpointPolicy, ModelProtocolPolicy, adapter_for};

const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub(super) struct ErrorDetails {
    pub(super) classification_message: String,
    pub(super) safe_message: String,
    pub(super) error_type: Option<String>,
}

#[derive(Clone)]
pub struct BackendClient {
    backend_id: String,
    credential_id: String,
    adapter_kind: AdapterKind,
    adapter: &'static dyn ProviderAdapter,
    endpoint: String,
    api_key: String,
    model: String,
    protocols: Vec<Protocol>,
    pricing: Option<ModelPricingConfig>,
    client: reqwest::Client,
    stream_client: reqwest::Client,
}

#[derive(Debug)]
pub struct BackendError {
    pub class: FailureClass,
    pub status: Option<StatusCode>,
    pub retry_after: Option<Duration>,
    pub safe_message: String,
}

#[derive(Debug, Error)]
pub enum BackendBuildError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("adapter {0} is not registered")]
    UnsupportedAdapter(&'static str),
    #[error("custom backend {0} has no validated endpoint")]
    MissingEndpoint(String),
    #[error("custom backend {0} endpoint is not an absolute HTTP(S) URL")]
    InvalidEndpoint(String),
    #[error("backend {backend} has no protocol contract for model {model}")]
    UnknownModel { backend: String, model: String },
    #[error("backend {backend} model {model} has no validated protocols")]
    MissingProtocols { backend: String, model: String },
    #[error("backend {backend} model {model} does not support protocol {protocol}")]
    UnsupportedProtocol {
        backend: String,
        model: String,
        protocol: &'static str,
    },
}

impl BackendClient {
    pub fn new(
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
    ) -> Result<Self, BackendBuildError> {
        Self::build(
            backend,
            credential,
            model,
            None,
            ServerTimeoutConfig::default(),
        )
    }

    #[cfg(not(feature = "test-support"))]
    pub(crate) fn new_with_timeouts(
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
        timeouts: ServerTimeoutConfig,
    ) -> Result<Self, BackendBuildError> {
        Self::build(backend, credential, model, None, timeouts)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn new_with_endpoint_override(
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
        endpoint_override: Option<&str>,
        timeouts: ServerTimeoutConfig,
    ) -> Result<Self, BackendBuildError> {
        Self::build(backend, credential, model, endpoint_override, timeouts)
    }

    fn build(
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
        endpoint_override: Option<&str>,
        timeouts: ServerTimeoutConfig,
    ) -> Result<Self, BackendBuildError> {
        let client = build_http_client(&timeouts, timeouts.upstream_read_ms)?;
        let stream_client = build_http_client(&timeouts, timeouts.upstream_stream_read_ms)?;
        let adapter = adapter_for(backend.adapter).ok_or(BackendBuildError::UnsupportedAdapter(
            backend.adapter.as_str(),
        ))?;
        let endpoint_policy = adapter.endpoint_policy();
        let endpoint = endpoint_override.unwrap_or_else(|| match endpoint_policy {
            EndpointPolicy::Official(endpoint) => endpoint,
            EndpointPolicy::ConfiguredExact => backend.endpoint.as_deref().unwrap_or_default(),
        });
        if endpoint.is_empty() {
            return Err(BackendBuildError::MissingEndpoint(backend.id.clone()));
        }
        if endpoint_override.is_none() && endpoint_policy == EndpointPolicy::ConfiguredExact {
            let valid = reqwest::Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            });
            if !valid {
                return Err(BackendBuildError::InvalidEndpoint(backend.id.clone()));
            }
        }
        let protocols = match adapter.model_protocol_policy() {
            ModelProtocolPolicy::Listed => model.protocols.clone(),
            ModelProtocolPolicy::OfficialCatalog => {
                vec![adapter.protocol_for_model(&model.name).ok_or_else(|| {
                    BackendBuildError::UnknownModel {
                        backend: backend.id.clone(),
                        model: model.name.clone(),
                    }
                })?]
            }
        };
        if protocols.is_empty() {
            return Err(BackendBuildError::MissingProtocols {
                backend: backend.id.clone(),
                model: model.name.clone(),
            });
        }
        if let Some(protocol) = protocols
            .iter()
            .find(|protocol| !adapter.supports_protocol(**protocol))
        {
            return Err(BackendBuildError::UnsupportedProtocol {
                backend: backend.id.clone(),
                model: model.name.clone(),
                protocol: protocol.as_str(),
            });
        }
        Ok(Self {
            backend_id: backend.id.clone(),
            credential_id: credential.id.clone(),
            adapter_kind: backend.adapter,
            adapter,
            endpoint: endpoint.to_string(),
            api_key: credential.api_key.clone(),
            model: model.name.clone(),
            protocols,
            pricing: model.pricing,
            client,
            stream_client,
        })
    }

    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub const fn adapter_kind(&self) -> AdapterKind {
        self.adapter_kind
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn protocols(&self) -> &[Protocol] {
        &self.protocols
    }
    pub const fn pricing(&self) -> Option<&ModelPricingConfig> {
        self.pricing.as_ref()
    }
    pub fn protocol_for(&self, ingress: Protocol) -> Protocol {
        if self.protocols.contains(&ingress) {
            ingress
        } else if self.protocols.contains(&Protocol::OpenAiChat) {
            Protocol::OpenAiChat
        } else {
            self.protocols[0]
        }
    }

    pub async fn send(
        &self,
        protocol: Protocol,
        body: &Value,
        inbound_headers: &HeaderMap,
    ) -> Result<Response, BackendError> {
        debug_assert!(self.protocols.contains(&protocol));
        let url = self.request_url(protocol);
        let client = if body.get("stream").and_then(Value::as_bool) == Some(true) {
            &self.stream_client
        } else {
            &self.client
        };
        let mut request = client.post(url).json(body);
        request = self
            .adapter
            .apply_auth(request, &self.api_key, protocol, inbound_headers);

        let response = request.send().await.map_err(|error| BackendError {
            class: classify_transport(&error),
            status: error.status(),
            retry_after: None,
            safe_message: sanitize_transport_error(&error),
        })?;
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let body = read_error_body(response).await;
        let details = extract_error_details(&body);
        let class = self.adapter.classify_error(status, &details);
        Err(BackendError {
            class,
            status: Some(status),
            retry_after,
            safe_message: details.safe_message,
        })
    }

    pub fn request_url(&self, protocol: Protocol) -> String {
        self.adapter.build_url(&self.endpoint, protocol)
    }

    pub fn classify_semantic_error(
        &self,
        status: StatusCode,
        value: &Value,
    ) -> Option<BackendError> {
        let is_error = value.get("error").is_some_and(|error| !error.is_null())
            || matches!(
                value.get("type").and_then(Value::as_str),
                Some("error" | "response.failed")
            );
        if !is_error {
            return None;
        }
        let details = extract_error_details_from_value(value).unwrap_or_else(|| ErrorDetails {
            classification_message: String::new(),
            safe_message: "upstream returned an error envelope".into(),
            error_type: None,
        });
        Some(BackendError {
            class: self.adapter.classify_error(status, &details),
            status: Some(status),
            retry_after: None,
            safe_message: details.safe_message,
        })
    }
}

fn build_http_client(
    timeouts: &ServerTimeoutConfig,
    read_timeout_ms: u64,
) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!("quotamux/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_millis(timeouts.upstream_connect_ms))
        .read_timeout(Duration::from_millis(read_timeout_ms))
        .timeout(Duration::from_millis(timeouts.upstream_total_ms))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .http2_adaptive_window(true)
        .build()
}

async fn read_error_body(mut response: Response) -> Vec<u8> {
    let mut body = Vec::with_capacity(MAX_ERROR_BYTES);
    while body.len() < MAX_ERROR_BYTES {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let remaining = MAX_ERROR_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

pub fn classify_status(status: StatusCode, _message: &str) -> FailureClass {
    match status.as_u16() {
        400 | 409 | 413 | 414 | 422 | 431 => FailureClass::ClientRequest,
        401 => FailureClass::ProviderAuth,
        402 => FailureClass::ProviderBilling,
        403 => FailureClass::ProviderAuth,
        404 | 405 | 415 => FailureClass::ProviderConfiguration,
        408 | 425 => FailureClass::ProviderTransient,
        429 => FailureClass::ProviderCapacity,
        499 => FailureClass::ClientCancelled,
        code if (400..500).contains(&code) => FailureClass::ProviderUnknown4xx,
        code if (500..600).contains(&code) => FailureClass::ProviderTransient,
        _ => FailureClass::ProviderUnknown5xxOrTransport,
    }
}

fn classify_transport(error: &reqwest::Error) -> FailureClass {
    if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() {
        FailureClass::ProviderTransient
    } else {
        FailureClass::ProviderUnknown5xxOrTransport
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    when.duration_since(std::time::SystemTime::now()).ok()
}

fn extract_error_details(bytes: &[u8]) -> ErrorDetails {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes)
        && let Some(details) = extract_error_details_from_value(&value)
    {
        return details;
    }
    let text = String::from_utf8_lossy(bytes);
    if text.to_ascii_lowercase().contains("error code: 1010") {
        return ErrorDetails {
            classification_message: truncate(text.trim()),
            safe_message: "Cloudflare rejected the HTTP client signature (error 1010)".into(),
            error_type: None,
        };
    }
    ErrorDetails {
        classification_message: truncate(text.trim()),
        safe_message: "upstream request failed".into(),
        error_type: None,
    }
}

fn error_discriminator(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|code| code.to_string()))
        .or_else(|| value.as_u64().map(|code| code.to_string()))
}

fn extract_error_details_from_value(value: &Value) -> Option<ErrorDetails> {
    let (message, error_type) = if let Some(error) = value.get("error") {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            (
                message,
                error
                    .get("type")
                    .or_else(|| error.get("code"))
                    .and_then(error_discriminator),
            )
        } else {
            let message = error.as_str()?;
            (
                message,
                value
                    .get("type")
                    .or_else(|| value.get("code"))
                    .and_then(error_discriminator),
            )
        }
    } else {
        let message = value.get("message").and_then(Value::as_str)?;
        (
            message,
            value
                .get("type")
                .or_else(|| value.get("code"))
                .and_then(error_discriminator),
        )
    };
    Some(ErrorDetails {
        classification_message: truncate(message),
        safe_message: "upstream request failed".into(),
        error_type: error_type.as_deref().map(truncate),
    })
}

fn sanitize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "upstream request timed out".into()
    } else if error.is_connect() {
        "could not connect to upstream".into()
    } else if error.is_body() {
        "upstream response body failed".into()
    } else {
        "upstream transport failure".into()
    }
}

fn truncate(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| if word.contains("://") { "[url]" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(
        kind: AdapterKind,
        status: StatusCode,
        message: &str,
        error_type: Option<&str>,
    ) -> FailureClass {
        adapter_for(kind).unwrap().classify_error(
            status,
            &ErrorDetails {
                classification_message: message.into(),
                safe_message: message.into(),
                error_type: error_type.map(str::to_string),
            },
        )
    }

    fn client_for(kind: AdapterKind, model: &str) -> BackendClient {
        let backend = BackendConfig {
            id: kind.as_str().into(),
            adapter: kind,
            endpoint: None,
            credentials: vec![CredentialConfig {
                id: "test-key".into(),
                api_key: "secret".into(),
            }],
            models: vec![BackendModelConfig {
                name: model.into(),
                protocols: if kind == AdapterKind::OpenCodeGo {
                    Vec::new()
                } else {
                    vec![Protocol::OpenAiChat]
                },
                pricing: None,
            }],
        };
        BackendClient::new(&backend, &backend.credentials[0], &backend.models[0]).unwrap()
    }

    #[test]
    fn kimi_k3_adapters_use_their_distinct_official_urls() {
        let mut code = client_for(AdapterKind::KimiCode, "k3");
        code.protocols.push(Protocol::AnthropicMessages);
        assert_eq!(
            code.request_url(Protocol::OpenAiChat),
            "https://api.kimi.com/coding/v1/chat/completions"
        );
        assert_eq!(
            code.request_url(Protocol::AnthropicMessages),
            "https://api.kimi.com/coding/v1/messages"
        );
        assert_eq!(
            code.protocol_for(Protocol::AnthropicMessages),
            Protocol::AnthropicMessages
        );

        let official = client_for(AdapterKind::KimiOfficial, "kimi-k3");
        assert_eq!(
            official.request_url(Protocol::OpenAiChat),
            "https://api.moonshot.cn/v1/chat/completions"
        );

        let go = client_for(AdapterKind::OpenCodeGo, "kimi-k3");
        assert_eq!(
            go.request_url(Protocol::OpenAiChat),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );

        let mut deepseek = client_for(AdapterKind::DeepSeekOfficial, "deepseek-v4-pro");
        deepseek.protocols = vec![
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ];
        assert_eq!(
            deepseek.request_url(Protocol::OpenAiChat),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            deepseek.request_url(Protocol::OpenAiResponses),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            deepseek.request_url(Protocol::AnthropicMessages),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            assert_eq!(deepseek.protocol_for(protocol), protocol);
        }

        let mut zhipu = client_for(AdapterKind::ZhipuCodingPlan, "glm-5.3-flash");
        zhipu.protocols = vec![
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ];
        assert_eq!(
            zhipu.request_url(Protocol::OpenAiChat),
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(
            zhipu.request_url(Protocol::OpenAiResponses),
            "https://open.bigmodel.cn/api/v1/responses"
        );
        assert_eq!(
            zhipu.request_url(Protocol::AnthropicMessages),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            assert_eq!(zhipu.protocol_for(protocol), protocol);
        }
    }

    #[test]
    fn opencode_go_uses_each_models_explicit_official_protocol() {
        let adapter = adapter_for(AdapterKind::OpenCodeGo).unwrap();
        for (protocol, models) in [
            (Protocol::OpenAiResponses, &["grok-4.5", "gpt-5.6-luna"][..]),
            (
                Protocol::OpenAiChat,
                &[
                    "glm-5.3-flash",
                    "glm-5.3",
                    "glm-5.2",
                    "glm-5.1",
                    "kimi-k3",
                    "kimi-k2.7-code",
                    "kimi-k2.6",
                    "deepseek-v4-pro",
                    "deepseek-v4-flash",
                    "deepseek-v4-flash-vision-exp",
                    "mimo-v2.5",
                    "mimo-v2.5-pro",
                    "hy3",
                ][..],
            ),
            (
                Protocol::AnthropicMessages,
                &[
                    "minimax-m3",
                    "minimax-m2.7",
                    "minimax-m2.5",
                    "qwen3.8-max",
                    "qwen3.7-max",
                    "qwen3.7-plus",
                    "qwen3.6-plus",
                ][..],
            ),
        ] {
            for model in models {
                assert_eq!(adapter.protocol_for_model(model), Some(protocol), "{model}");
                let client = client_for(AdapterKind::OpenCodeGo, model);
                assert_eq!(client.protocol_for(protocol), protocol, "{model}");
            }
        }
        assert_eq!(
            adapter.protocol_for_model("not-in-the-official-catalog"),
            None
        );
    }

    #[test]
    fn custom_endpoint_is_exact_and_never_uses_official_error_classification() {
        let backend = BackendConfig {
            id: "customer".into(),
            adapter: AdapterKind::CustomChatCompletions,
            endpoint: Some("https://customer.example/private/inference".into()),
            credentials: vec![CredentialConfig {
                id: "customer-key".into(),
                api_key: "secret".into(),
            }],
            models: vec![BackendModelConfig {
                name: "customer-model".into(),
                protocols: vec![Protocol::OpenAiChat],
                pricing: None,
            }],
        };
        let client =
            BackendClient::new(&backend, &backend.credentials[0], &backend.models[0]).unwrap();
        assert_eq!(
            client.request_url(Protocol::OpenAiChat),
            "https://customer.example/private/inference"
        );
        assert_eq!(
            classify(
                AdapterKind::CustomChatCompletions,
                StatusCode::TOO_MANY_REQUESTS,
                "Subscription quota exceeded",
                Some("GoUsageLimitError"),
            ),
            FailureClass::ProviderCapacity
        );
    }

    #[test]
    fn public_client_constructor_rejects_unvalidated_backend_shapes_without_panicking() {
        let mut backend = BackendConfig {
            id: "bad".into(),
            adapter: AdapterKind::OllamaCloud,
            endpoint: None,
            credentials: vec![CredentialConfig {
                id: "key".into(),
                api_key: "secret".into(),
            }],
            models: vec![BackendModelConfig {
                name: "model".into(),
                protocols: vec![Protocol::OpenAiChat],
                pricing: None,
            }],
        };
        assert!(matches!(
            BackendClient::new(&backend, &backend.credentials[0], &backend.models[0]),
            Err(BackendBuildError::UnsupportedAdapter("ollama-cloud"))
        ));

        backend.adapter = AdapterKind::CustomChatCompletions;
        backend.endpoint = Some("relative/path".into());
        assert!(matches!(
            BackendClient::new(&backend, &backend.credentials[0], &backend.models[0]),
            Err(BackendBuildError::InvalidEndpoint(_))
        ));

        backend.adapter = AdapterKind::KimiOfficial;
        backend.endpoint = None;
        backend.models[0].protocols = vec![Protocol::AnthropicMessages];
        assert!(matches!(
            BackendClient::new(&backend, &backend.credentials[0], &backend.models[0]),
            Err(BackendBuildError::UnsupportedProtocol { .. })
        ));
    }

    #[test]
    fn classifies_known_errors() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, ""),
            FailureClass::ProviderAuth
        );
        assert_eq!(
            classify_status(StatusCode::PAYMENT_REQUIRED, ""),
            FailureClass::ProviderBilling
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, ""),
            FailureClass::ProviderCapacity
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, ""),
            FailureClass::ProviderTransient
        );
        assert_eq!(
            classify_status(
                StatusCode::FORBIDDEN,
                "model is hosted in China; enable region"
            ),
            FailureClass::ProviderAuth
        );
    }

    #[test]
    fn classifies_kimi_code_entitlement_and_quota_errors() {
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::UNAUTHORIZED,
                "Your current plan supports only kimi-k3 up to 256K context",
                None,
            ),
            FailureClass::ProviderConfiguration
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid_argument: bot_id value does not match id_kinds: [uuid_v4]",
                None,
            ),
            FailureClass::ClientRequest
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::INTERNAL_SERVER_ERROR,
                "unauthenticated: failed_precondition: 该账号已被禁用。",
                None,
            ),
            FailureClass::ProviderConfiguration
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::FORBIDDEN,
                "You've reached your usage limit for this billing cycle",
                Some("access_terminated_error"),
            ),
            FailureClass::ProviderQuota
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::PAYMENT_REQUIRED,
                "unable to verify membership benefits",
                None,
            ),
            FailureClass::ProviderTransient
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::TOO_MANY_REQUESTS,
                "the engine is currently overloaded",
                None,
            ),
            FailureClass::ProviderCapacity
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::TOO_MANY_REQUESTS,
                "You've reached your usage limit for this period. Your quota will be refreshed in the next period.",
                None,
            ),
            FailureClass::ProviderQuota
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::TOO_MANY_REQUESTS,
                "You've reached kimi monthly usage limit for this billing cycle.",
                None,
            ),
            FailureClass::ProviderQuota
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::FORBIDDEN,
                "Access terminated.",
                None,
            ),
            FailureClass::ProviderConfiguration
        );
        assert_eq!(
            classify(
                AdapterKind::KimiCode,
                StatusCode::FORBIDDEN,
                "temporary membership edge rejection",
                None,
            ),
            FailureClass::ProviderAmbiguousRejection
        );
    }

    #[test]
    fn classifies_opencode_go_usage_limit_from_official_error_shape() {
        let details = extract_error_details(
            br#"{"type":"error","error":{"type":"GoUsageLimitError","message":"Subscription quota exceeded. You can continue using free models."},"metadata":{"limitName":"5 hour"}}"#,
        );
        assert_eq!(details.error_type.as_deref(), Some("GoUsageLimitError"));
        assert_eq!(
            adapter_for(AdapterKind::OpenCodeGo)
                .unwrap()
                .classify_error(StatusCode::TOO_MANY_REQUESTS, &details),
            FailureClass::ProviderQuota
        );
        assert_eq!(
            classify(
                AdapterKind::OpenCodeGo,
                StatusCode::TOO_MANY_REQUESTS,
                "5-hour usage limit reached. Resets in 4hr 57min.",
                Some("GoUsageLimitError"),
            ),
            FailureClass::ProviderQuota
        );
        assert_eq!(
            classify(
                AdapterKind::OpenCodeGo,
                StatusCode::TOO_MANY_REQUESTS,
                "the upstream is overloaded",
                None,
            ),
            FailureClass::ProviderCapacity
        );
    }

    #[test]
    fn classifies_zhipu_business_error_codes() {
        for (code, expected) in [
            ("1002", FailureClass::ProviderAuth),
            ("1113", FailureClass::ProviderBilling),
            ("1211", FailureClass::ProviderConfiguration),
            ("1261", FailureClass::ClientRequest),
            ("1302", FailureClass::ProviderCapacity),
            ("1308", FailureClass::ProviderQuota),
            ("1317", FailureClass::ProviderQuota),
        ] {
            assert_eq!(
                classify(
                    AdapterKind::ZhipuCodingPlan,
                    StatusCode::TOO_MANY_REQUESTS,
                    "zhipu error",
                    Some(code),
                ),
                expected,
                "business code {code}"
            );
        }

        let details = extract_error_details_from_value(&serde_json::json!({
            "error":{"code":1308,"message":"usage limit reached"}
        }))
        .unwrap();
        assert_eq!(details.error_type.as_deref(), Some("1308"));
    }

    #[test]
    fn classifies_kimi_open_platform_structured_error_types() {
        for (error_type, expected) in [
            ("engine_overloaded_error", FailureClass::ProviderCapacity),
            ("rate_limit_reached_error", FailureClass::ProviderCapacity),
            (
                "exceeded_current_quota_error",
                FailureClass::ProviderBilling,
            ),
            ("incorrect_api_key_error", FailureClass::ProviderAuth),
            ("invalid_request_error", FailureClass::ClientRequest),
            ("server_unavailable", FailureClass::ProviderTransient),
        ] {
            assert_eq!(
                classify(
                    AdapterKind::KimiOfficial,
                    StatusCode::TOO_MANY_REQUESTS,
                    "documented error",
                    Some(error_type),
                ),
                expected,
                "{error_type}"
            );
        }
    }

    #[test]
    fn generic_request_specific_4xx_does_not_poison_a_target() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::CONFLICT,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::URI_TOO_LONG,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        ] {
            assert_eq!(classify_status(status, ""), FailureClass::ClientRequest);
        }
        assert_eq!(
            classify_status(StatusCode::METHOD_NOT_ALLOWED, ""),
            FailureClass::ProviderConfiguration
        );
    }

    #[test]
    fn retry_after_supports_standard_seconds_and_rejects_unknown_formats() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));

        headers.insert(RETRY_AFTER, "not-a-delay".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);

        headers.remove(RETRY_AFTER);
        headers.insert("retry-after-ms", "250".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn stored_upstream_errors_never_copy_untrusted_upstream_text() {
        let details = extract_error_details(
            br#"{"error":{"message":"Incorrect API key provided: sk-live-secret-value; manage it at https://example.com/workspace/private-id/settings"}}"#,
        );
        assert_eq!(details.safe_message, "upstream request failed");
        assert!(!details.safe_message.contains("sk-live-secret-value"));
        assert!(!details.safe_message.contains("private-id"));
        assert!(details.classification_message.contains("Incorrect API key"));
    }

    #[test]
    fn plain_text_upstream_errors_are_also_replaced_by_a_controlled_summary() {
        let details = extract_error_details(
            b"Authorization: Bearer secret-token cookie=session-secret user@example.com",
        );
        assert_eq!(details.safe_message, "upstream request failed");
        assert!(!details.safe_message.contains("secret"));
        assert!(!details.safe_message.contains('@'));
    }

    #[test]
    fn successful_http_status_with_error_envelope_is_not_accepted_as_success() {
        let client = client_for(AdapterKind::OpenCodeGo, "kimi-k3");
        let error = client
            .classify_semantic_error(
                StatusCode::OK,
                &serde_json::json!({
                    "type":"error",
                    "error":{
                        "type":"GoUsageLimitError",
                        "message":"Subscription quota exceeded"
                    }
                }),
            )
            .expect("semantic error");
        assert_eq!(error.class, FailureClass::ProviderQuota);
        assert_eq!(error.safe_message, "upstream request failed");
    }

    #[tokio::test]
    async fn error_response_reader_has_a_hard_memory_bound() {
        let app = axum::Router::new().route(
            "/error",
            axum::routing::get(|| async { vec![b'x'; MAX_ERROR_BYTES * 4] }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let response = reqwest::get(format!("http://{address}/error"))
            .await
            .unwrap();
        let body = read_error_body(response).await;
        server.abort();
        assert_eq!(body.len(), MAX_ERROR_BYTES);
    }
}
