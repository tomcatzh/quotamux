use std::time::Duration;

use reqwest::{
    Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde_json::Value;

use crate::{
    config::{CredentialConfig, ProviderConfig, ProviderKind, ProviderModelConfig},
    types::{FailureClass, Protocol},
};

const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct ProviderClient {
    provider_id: String,
    credential_id: String,
    kind: ProviderKind,
    endpoint: String,
    api_key: String,
    model: String,
    protocols: Vec<Protocol>,
    client: reqwest::Client,
}

#[derive(Debug)]
pub struct ProviderError {
    pub class: FailureClass,
    pub status: Option<StatusCode>,
    pub retry_after: Option<Duration>,
    pub safe_message: String,
}

impl ProviderClient {
    pub fn new(
        provider: &ProviderConfig,
        credential: &CredentialConfig,
        model: &ProviderModelConfig,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("quotamux/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_adaptive_window(true)
            .build()?;
        Ok(Self {
            provider_id: provider.id.clone(),
            credential_id: credential.id.clone(),
            kind: provider.kind,
            endpoint: provider
                .endpoint()
                .expect("validated provider endpoint")
                .to_string(),
            api_key: credential.api_key.clone(),
            model: model.name.clone(),
            protocols: model.protocols.clone(),
            client,
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub const fn kind(&self) -> ProviderKind {
        self.kind
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
    ) -> Result<Response, ProviderError> {
        debug_assert!(self.protocols.contains(&protocol));
        let url = self.url(protocol);
        let mut request = self.client.post(url).json(body);
        match (self.kind, protocol) {
            (ProviderKind::DeepSeekOfficial, Protocol::AnthropicMessages)
            | (ProviderKind::CustomAnthropic, Protocol::AnthropicMessages) => {
                request = request.header("x-api-key", &self.api_key).header(
                    "anthropic-version",
                    inbound_headers
                        .get("anthropic-version")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("2023-06-01"),
                );
                if let Some(beta) = inbound_headers.get("anthropic-beta") {
                    request = request.header("anthropic-beta", beta);
                }
            }
            _ => request = request.bearer_auth(&self.api_key),
        }

        let response = request.send().await.map_err(|error| ProviderError {
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
        let body = response.bytes().await.unwrap_or_default();
        let body = &body[..body.len().min(MAX_ERROR_BYTES)];
        let safe_message = extract_error_message(body);
        let class = classify_provider_status(self.kind, status, &safe_message);
        Err(ProviderError {
            class,
            status: Some(status),
            retry_after,
            safe_message,
        })
    }

    pub async fn balance(&self) -> Result<Value, ProviderError> {
        debug_assert_eq!(self.kind, ProviderKind::DeepSeekOfficial);
        let response = self
            .client
            .get(format!(
                "{}/user/balance",
                self.endpoint.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| ProviderError {
                class: classify_transport(&error),
                status: error.status(),
                retry_after: None,
                safe_message: sanitize_transport_error(&error),
            })?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            let message = extract_error_message(&bytes);
            return Err(ProviderError {
                class: classify_status(status, &message),
                status: Some(status),
                retry_after: parse_retry_after(&headers),
                safe_message: message,
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| ProviderError {
            class: FailureClass::ProviderUnknown5xxOrTransport,
            status: Some(status),
            retry_after: None,
            safe_message: "provider returned malformed balance JSON".into(),
        })
    }

    fn url(&self, protocol: Protocol) -> String {
        if self.kind.uses_exact_endpoint() {
            return self.endpoint.clone();
        }
        let base = self.endpoint.trim_end_matches('/');
        match (self.kind, protocol) {
            (_, Protocol::OpenAiChat) => format!("{base}/chat/completions"),
            (_, Protocol::OpenAiResponses) => format!("{base}/responses"),
            (ProviderKind::DeepSeekOfficial, Protocol::AnthropicMessages) => {
                format!("{base}/anthropic/v1/messages")
            }
            (_, Protocol::AnthropicMessages) => format!("{base}/messages"),
        }
    }
}

pub fn classify_status(status: StatusCode, message: &str) -> FailureClass {
    match status.as_u16() {
        400 | 422 => FailureClass::ClientRequest,
        401 => FailureClass::ProviderAuth,
        402 => FailureClass::ProviderBilling,
        403 if contains_any(message, &["china", "region", "hosted in", "configuration"]) => {
            FailureClass::ProviderConfiguration
        }
        403 => FailureClass::ProviderAuth,
        404 => FailureClass::ProviderConfiguration,
        408 | 425 | 500 | 502 | 503 | 504 => FailureClass::ProviderTransient,
        429 => FailureClass::ProviderCapacity,
        code if (400..500).contains(&code) => FailureClass::ProviderUnknown4xx,
        _ => FailureClass::ProviderUnknown5xxOrTransport,
    }
}

fn classify_provider_status(kind: ProviderKind, status: StatusCode, message: &str) -> FailureClass {
    if kind == ProviderKind::KimiCode {
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
                return FailureClass::ProviderConfiguration;
            }
            402 => return FailureClass::ProviderTransient,
            403 if contains_any(message, &["usage limit", "quota", "billing cycle"]) => {
                return FailureClass::ProviderBilling;
            }
            429 if contains_any(message, &["usage limit", "quota", "billing cycle"]) => {
                return FailureClass::ProviderBilling;
            }
            _ => {}
        }
    }
    classify_status(status, message)
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

fn extract_error_message(bytes: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        if let Some(error) = value.get("error") {
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                return truncate(message);
            }
            if let Some(message) = error.as_str() {
                return truncate(message);
            }
        }
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            return truncate(message);
        }
    }
    let text = String::from_utf8_lossy(bytes);
    if text.to_ascii_lowercase().contains("error code: 1010") {
        return "Cloudflare rejected the HTTP client signature (error 1010)".into();
    }
    if text.trim().is_empty() {
        "upstream request failed".into()
    } else {
        truncate(text.trim())
    }
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
    value.chars().take(512).collect()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let haystack = haystack.to_ascii_lowercase();
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_for(kind: ProviderKind, model: &str) -> ProviderClient {
        let provider = ProviderConfig {
            id: kind.as_str().into(),
            kind,
            endpoint: None,
            credentials: vec![CredentialConfig {
                id: "test-key".into(),
                api_key: "secret".into(),
            }],
            models: vec![ProviderModelConfig {
                name: model.into(),
                protocols: vec![Protocol::OpenAiChat],
            }],
        };
        ProviderClient::new(&provider, &provider.credentials[0], &provider.models[0]).unwrap()
    }

    #[test]
    fn kimi_k3_provider_kinds_use_their_distinct_official_chat_urls() {
        let code = client_for(ProviderKind::KimiCode, "k3");
        assert_eq!(
            code.url(Protocol::OpenAiChat),
            "https://api.kimi.com/coding/v1/chat/completions"
        );

        let official = client_for(ProviderKind::KimiOfficial, "kimi-k3");
        assert_eq!(
            official.url(Protocol::OpenAiChat),
            "https://api.moonshot.cn/v1/chat/completions"
        );

        let go = client_for(ProviderKind::OpenCodeGo, "kimi-k3");
        assert_eq!(
            go.url(Protocol::OpenAiChat),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
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
            FailureClass::ProviderConfiguration
        );
    }

    #[test]
    fn classifies_kimi_code_entitlement_and_quota_errors() {
        assert_eq!(
            classify_provider_status(
                ProviderKind::KimiCode,
                StatusCode::UNAUTHORIZED,
                "Your current plan supports only kimi-k3 up to 256K context"
            ),
            FailureClass::ProviderConfiguration
        );
        assert_eq!(
            classify_provider_status(
                ProviderKind::KimiCode,
                StatusCode::FORBIDDEN,
                "You've reached your usage limit for this billing cycle"
            ),
            FailureClass::ProviderBilling
        );
        assert_eq!(
            classify_provider_status(
                ProviderKind::KimiCode,
                StatusCode::PAYMENT_REQUIRED,
                "unable to verify membership benefits"
            ),
            FailureClass::ProviderTransient
        );
        assert_eq!(
            classify_provider_status(
                ProviderKind::KimiCode,
                StatusCode::TOO_MANY_REQUESTS,
                "the engine is currently overloaded"
            ),
            FailureClass::ProviderCapacity
        );
    }
}
