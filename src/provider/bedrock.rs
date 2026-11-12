//! IAM-authenticated Bedrock Invoke transport. The SDK owns signing and EventStream decoding.
use super::{BackendError, extract_error_details_from_value};
use crate::{config::ServerTimeoutConfig, types::FailureClass};
use aws_sdk_bedrockruntime::{
    Client,
    config::{
        BehaviorVersion, Region, StalledStreamProtectionConfig, retry::RetryConfig,
        timeout::TimeoutConfig,
    },
    error::{ProvideErrorMetadata, SdkError},
    primitives::Blob,
    types::{ResponseStream, error::ResponseStreamError},
};
use reqwest::{Response, StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{sync::OnceCell, time::Instant};

#[derive(Clone)]
pub(super) struct BedrockTransport {
    region: String,
    endpoint_override: Option<String>,
    timeouts: ServerTimeoutConfig,
    client: Arc<OnceCell<Client>>,
}
impl BedrockTransport {
    pub(super) fn new(
        region: String,
        endpoint_override: Option<String>,
        timeouts: ServerTimeoutConfig,
    ) -> Self {
        Self {
            region,
            endpoint_override,
            timeouts,
            client: Arc::new(OnceCell::new()),
        }
    }
    async fn client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let region = Region::new(self.region.clone());
                #[cfg(feature = "test-support")]
                if let Some(endpoint) = &self.endpoint_override {
                    return Client::from_conf(
                        crate::tls::aws_client(aws_sdk_bedrockruntime::config::Builder::new())
                            .behavior_version(BehaviorVersion::latest())
                            .credentials_provider(aws_sdk_bedrockruntime::config::Credentials::new(
                                "quotamux-test-access",
                                "quotamux-test-secret",
                                Some("quotamux-test-session".into()),
                                None,
                                "quotamux-test",
                            ))
                            .region(region)
                            .endpoint_url(endpoint)
                            .retry_config(RetryConfig::disabled())
                            .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
                            .timeout_config(
                                TimeoutConfig::builder()
                                    .connect_timeout(Duration::from_millis(
                                        self.timeouts.upstream_connect_ms,
                                    ))
                                    .operation_timeout(Duration::from_millis(
                                        self.timeouts.upstream_total_ms,
                                    ))
                                    .build(),
                            )
                            .build(),
                    );
                }
                let shared = crate::tls::aws_loader(
                    aws_config::defaults(BehaviorVersion::latest()).region(region.clone()),
                )
                .load()
                .await;
                let builder =
                    crate::tls::aws_client(aws_sdk_bedrockruntime::config::Builder::new())
                        .behavior_version(BehaviorVersion::latest())
                        .credentials_provider(
                            shared
                                .credentials_provider()
                                .expect("default credential chain"),
                        )
                        .region(region)
                        .endpoint_url(self.endpoint_override.clone().unwrap_or_else(|| {
                            format!("https://bedrock-runtime.{}.amazonaws.com", self.region)
                        }))
                        .retry_config(RetryConfig::disabled())
                        .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
                        .timeout_config(
                            TimeoutConfig::builder()
                                .connect_timeout(Duration::from_millis(
                                    self.timeouts.upstream_connect_ms,
                                ))
                                .operation_timeout(Duration::from_millis(
                                    self.timeouts.upstream_total_ms,
                                ))
                                .build(),
                        );
                Client::from_conf(builder.build())
            })
            .await
    }
    pub(super) async fn send(
        &self,
        model: &str,
        body: &Value,
        headers: &HeaderMap,
    ) -> Result<Response, BackendError> {
        let deadline = Instant::now() + Duration::from_millis(self.timeouts.upstream_total_ms);
        let streaming = body.get("stream").and_then(Value::as_bool) == Some(true);
        let payload = payload(body, headers)?;
        let client = tokio::time::timeout_at(deadline, self.client())
            .await
            .map_err(|_| timeout_error())?;
        if !streaming {
            let output = tokio::time::timeout_at(
                deadline
                    .min(Instant::now() + Duration::from_millis(self.timeouts.upstream_read_ms)),
                client
                    .invoke_model()
                    .model_id(model)
                    .content_type("application/json")
                    .accept("application/json")
                    .body(Blob::new(payload))
                    .send(),
            )
            .await
            .map_err(|_| timeout_error())?
            .map_err(sdk_error)?;
            return Ok(Response::from(
                axum::http::Response::builder()
                    .header("content-type", "application/json")
                    .body(reqwest::Body::from(output.body.into_inner()))
                    .expect("static response headers"),
            ));
        }
        let mut output = tokio::time::timeout_at(
            deadline
                .min(Instant::now() + Duration::from_millis(self.timeouts.upstream_stream_read_ms)),
            client
                .invoke_model_with_response_stream()
                .model_id(model)
                .content_type("application/json")
                .accept("application/json")
                .body(Blob::new(payload))
                .send(),
        )
        .await
        .map_err(|_| timeout_error())?
        .map_err(sdk_error)?;
        let read_timeout = Duration::from_millis(self.timeouts.upstream_stream_read_ms);
        let stream = async_stream::stream! {
            loop {
                let next_deadline = deadline.min(Instant::now() + read_timeout);
                match tokio::time::timeout_at(next_deadline, output.body.recv()).await {
                    Ok(Ok(Some(ResponseStream::Chunk(part)))) => {
                        let Some(bytes) = part.bytes else { yield Err(stream_error()); break; };
                        match event(bytes.as_ref()) {
                            Ok(event) => yield Ok(event),
                            Err(_) => { yield Err(stream_error()); break; }
                        }
                    }
                    Ok(Err(error)) => {
                        let failure = stream_sdk_error(error);
                        let code = failure.upstream_type.as_deref().unwrap_or("ModelStreamErrorException");
                        let value = json!({"type":"error","error":{"type":code,"message":"Bedrock response stream failed"}});
                        yield Ok(event(&serde_json::to_vec(&value).expect("known error shape")).expect("known event shape"));
                        break;
                    }
                    Ok(Ok(None)) => break,
                    _ => { yield Err(stream_error()); break; }
                }
            }
        };
        Ok(Response::from(
            axum::http::Response::builder()
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(stream))
                .expect("static response headers"),
        ))
    }
}
fn timeout_error() -> BackendError {
    BackendError::local(
        FailureClass::ProviderTransient,
        None,
        None,
        "upstream request timed out",
    )
}
fn stream_error() -> std::io::Error {
    std::io::Error::other("Bedrock response stream failed")
}
// Operation responses retain their real HTTP envelope, including unknown AWS errors.
fn sdk_error<E>(error: SdkError<E>) -> BackendError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let raw = error.raw_response();
    let status = raw.and_then(|response| StatusCode::from_u16(response.status().as_u16()).ok());
    let retry_after = raw.and_then(|response| {
        let value = response.headers().get("retry-after")?;
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, value.parse().ok()?);
        super::parse_retry_after(&headers)
    });
    if let Some(service) = error.as_service_error() {
        service_error(service, status, retry_after)
    } else {
        non_service_error(&error, status, retry_after)
    }
}
// EventStream exception frames have no HTTP failure status of their own.
fn stream_sdk_error<R>(error: SdkError<ResponseStreamError, R>) -> BackendError
where
    R: std::fmt::Debug + Send + Sync + 'static,
{
    if let Some(service) = error.as_service_error() {
        let fallback = match service {
            ResponseStreamError::InternalServerException(_) => "internalServerException",
            ResponseStreamError::ModelStreamErrorException(_) => "modelStreamErrorException",
            ResponseStreamError::ValidationException(_) => "validationException",
            ResponseStreamError::ThrottlingException(_) => "throttlingException",
            ResponseStreamError::ModelTimeoutException(_) => "modelTimeoutException",
            ResponseStreamError::ServiceUnavailableException(_) => "serviceUnavailableException",
            _ => "UnknownBedrockError",
        };
        let metadata = aws_sdk_bedrockruntime::error::ErrorMetadata::builder()
            .code(service.code().unwrap_or(fallback))
            .message(
                service
                    .message()
                    .unwrap_or("Bedrock response stream failed"),
            )
            .build();
        service_error(&metadata, None, None)
    } else {
        non_service_error(&error, None, None)
    }
}
fn service_error(
    service: &impl ProvideErrorMetadata,
    status: Option<StatusCode>,
    retry_after: Option<Duration>,
) -> BackendError {
    let code = service.code().unwrap_or("UnknownBedrockError");
    let status = status
        .filter(|status| !status.is_success())
        .unwrap_or_else(|| match code.to_ascii_lowercase().as_str() {
            "validationexception" => StatusCode::BAD_REQUEST,
            "accessdeniedexception"
            | "unrecognizedclientexception"
            | "expiredtokenexception"
            | "invalidsignatureexception" => StatusCode::FORBIDDEN,
            "resourcenotfoundexception" => StatusCode::NOT_FOUND,
            "throttlingexception" | "servicequotaexceededexception" | "modelnotreadyexception" => {
                StatusCode::TOO_MANY_REQUESTS
            }
            _ => StatusCode::BAD_GATEWAY,
        });
    let details = extract_error_details_from_value(
        &json!({"type":code,"message":service.message().unwrap_or("Bedrock request failed")}),
    )
    .expect("known error shape");
    BackendError::from_details(
        super::adapters::aws_bedrock::classify(code, status),
        Some(status),
        retry_after,
        &details,
    )
}
fn non_service_error(
    error: &(dyn std::error::Error + 'static),
    status: Option<StatusCode>,
    retry_after: Option<Duration>,
) -> BackendError {
    let mut source = Some(error);
    let mut credentials_failure = false;
    while let Some(error) = source {
        if error.is::<aws_credential_types::provider::error::CredentialsError>() {
            credentials_failure = true;
            break;
        }
        source = error.source();
    }
    let class = if credentials_failure {
        FailureClass::ProviderAuth
    } else if let Some(status) = status.filter(|status| !status.is_success()) {
        super::classify_status(status, "")
    } else {
        FailureClass::ProviderTransient
    };
    BackendError::local(
        class,
        status,
        retry_after,
        "Bedrock credentials or transport failed",
    )
}
fn payload(body: &Value, headers: &HeaderMap) -> Result<Vec<u8>, BackendError> {
    let mut body = body.clone();
    let object = body.as_object_mut().ok_or_else(|| {
        BackendError::local(
            FailureClass::ClientRequest,
            Some(StatusCode::BAD_REQUEST),
            None,
            "Messages body must be an object",
        )
    })?;
    object.remove("model");
    object.remove("stream");
    object.insert("anthropic_version".into(), json!("bedrock-2023-05-31"));
    let beta = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if !beta.is_empty() {
        let existing = object.entry("anthropic_beta").or_insert_with(|| json!([]));
        let array = existing.as_array_mut().ok_or_else(|| {
            BackendError::local(
                FailureClass::ClientRequest,
                Some(StatusCode::BAD_REQUEST),
                None,
                "anthropic_beta must be an array",
            )
        })?;
        for value in beta {
            if !array.iter().any(|v| v.as_str() == Some(value)) {
                array.push(json!(value));
            }
        }
    }
    serde_json::to_vec(&body).map_err(|_| {
        BackendError::local(
            FailureClass::ClientRequest,
            Some(StatusCode::BAD_REQUEST),
            None,
            "Messages body could not be serialized",
        )
    })
}
fn event(bytes: &[u8]) -> Result<bytes::Bytes, serde_json::Error> {
    let value: Value = serde_json::from_slice(bytes)?;
    let name = value
        .get("type")
        .and_then(Value::as_str)
        .filter(|name| name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or("message");
    Ok(bytes::Bytes::from(format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&value)?
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct MissingCredentials;
    impl aws_sdk_bedrockruntime::config::ProvideCredentials for MissingCredentials {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            aws_credential_types::provider::future::ProvideCredentials::ready(Err(
                aws_credential_types::provider::error::CredentialsError::not_loaded(
                    "synthetic missing role",
                ),
            ))
        }
    }
    #[tokio::test]
    async fn credential_loading_failure_is_auth_without_network_or_default_chain() {
        let client = Client::from_conf(
            aws_sdk_bedrockruntime::config::Builder::new()
                .behavior_version(BehaviorVersion::latest())
                .credentials_provider(MissingCredentials)
                .region(Region::new("us-east-1"))
                .endpoint_url("http://127.0.0.1:1")
                .retry_config(RetryConfig::disabled())
                .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
                .build(),
        );
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client
                .invoke_model()
                .model_id("global.anthropic.claude-sonnet-5-5")
                .body(Blob::new(b"{}".to_vec()))
                .send(),
        )
        .await
        .unwrap()
        .unwrap_err();
        let result = sdk_error(error);
        assert_eq!(result.class, FailureClass::ProviderAuth);
        assert_eq!(result.status, None);
        assert!(!result.safe_message.contains("synthetic"));
    }
    fn metadata(code: &str) -> aws_sdk_bedrockruntime::error::ErrorMetadata {
        aws_sdk_bedrockruntime::error::ErrorMetadata::builder()
            .code(code)
            .message("synthetic upstream error")
            .build()
    }
    #[test]
    fn unknown_service_errors_keep_http_status_and_retry_after() {
        for (status, expected) in [
            (StatusCode::BAD_REQUEST, FailureClass::ClientRequest),
            (StatusCode::FORBIDDEN, FailureClass::ProviderAuth),
            (
                StatusCode::TOO_MANY_REQUESTS,
                FailureClass::ProviderCapacity,
            ),
        ] {
            let result = service_error(
                &metadata("FutureBedrockException"),
                Some(status),
                Some(Duration::from_secs(37)),
            );
            assert_eq!(result.status, Some(status));
            assert_eq!(result.class, expected);
            assert_eq!(result.retry_after, Some(Duration::from_secs(37)));
        }
    }
    #[test]
    fn eventstream_service_exceptions_use_exception_status_instead_of_http_ok() {
        let result = service_error(&metadata("throttlingException"), Some(StatusCode::OK), None);
        assert_eq!(result.status, Some(StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(result.class, FailureClass::ProviderCapacity);
    }
    #[test]
    fn stream_variants_without_metadata_keep_their_failure_class() {
        use aws_sdk_bedrockruntime::types::error::*;
        let cases = [
            (
                ResponseStreamError::InternalServerException(
                    InternalServerException::builder()
                        .message("synthetic")
                        .build(),
                ),
                "internalServerException",
                FailureClass::ProviderTransient,
            ),
            (
                ResponseStreamError::ModelStreamErrorException(
                    ModelStreamErrorException::builder()
                        .message("synthetic")
                        .build(),
                ),
                "modelStreamErrorException",
                FailureClass::ProviderTransient,
            ),
            (
                ResponseStreamError::ValidationException(
                    ValidationException::builder().message("synthetic").build(),
                ),
                "validationException",
                FailureClass::ClientRequest,
            ),
            (
                ResponseStreamError::ThrottlingException(
                    ThrottlingException::builder().message("synthetic").build(),
                ),
                "throttlingException",
                FailureClass::ProviderCapacity,
            ),
            (
                ResponseStreamError::ModelTimeoutException(
                    ModelTimeoutException::builder()
                        .message("synthetic")
                        .build(),
                ),
                "modelTimeoutException",
                FailureClass::ProviderTransient,
            ),
            (
                ResponseStreamError::ServiceUnavailableException(
                    ServiceUnavailableException::builder()
                        .message("synthetic")
                        .build(),
                ),
                "serviceUnavailableException",
                FailureClass::ProviderTransient,
            ),
        ];
        for (service, code, class) in cases {
            assert_eq!(service.code(), None);
            let error = SdkError::service_error(service, ());
            let result = stream_sdk_error(error);
            assert_eq!(result.class, class, "{code}");
            assert_eq!(result.upstream_type.as_deref(), Some(code));
        }
    }
    #[test]
    fn invoke_payload_preserves_native_semantics_and_merges_beta() {
        let body = json!({"model":"public", "stream":true, "anthropic_beta":["existing"], "thinking":{"type":"adaptive"}, "messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"opaque","signature":"unchanged"}]}], "unknown_future":{"v":1}});
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", "existing, new-beta".parse().unwrap());
        let result: Value = serde_json::from_slice(&payload(&body, &headers).unwrap()).unwrap();
        assert!(result.get("model").is_none());
        assert!(result.get("stream").is_none());
        assert_eq!(result["messages"], body["messages"]);
        assert_eq!(result["thinking"], body["thinking"]);
        assert_eq!(result["unknown_future"], body["unknown_future"]);
        assert_eq!(result["anthropic_beta"], json!(["existing", "new-beta"]));
        assert_eq!(result["anthropic_version"], "bedrock-2023-05-31");
    }
    #[test]
    fn stream_event_is_one_sse_record_with_opaque_fields() {
        let input = json!({"type":"content_block_delta","delta":{"type":"signature_delta","signature":"opaque\nsignature"},"future":42});
        let result = event(&serde_json::to_vec(&input).unwrap()).unwrap();
        let text = std::str::from_utf8(&result).unwrap();
        assert!(text.starts_with("event: content_block_delta\ndata: "));
        assert!(text.ends_with("\n\n"));
        assert_eq!(
            serde_json::from_str::<Value>(
                text.lines().nth(1).unwrap().strip_prefix("data: ").unwrap()
            )
            .unwrap(),
            input
        );
    }
}
