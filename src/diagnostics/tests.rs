use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Fake {
    identity_ok: bool,
    calls: AtomicUsize,
    partial: bool,
}
impl Checks for Fake {
    async fn identity(&self, _: &str, redactor: &mut Redactor) -> Identity {
        redactor.add("secret-synthetic-access");
        redactor.add("secret-synthetic-token");
        Identity {
            report: if self.identity_ok {
                json!({"status":"passed","account":"123456789012","role_arn":"arn:aws:sts::123456789012:assumed-role/test/session"})
            } else {
                failure("credentials_missing")
            },
            can_probe: self.identity_ok,
        }
    }
    async fn probe(
        &self,
        _: &BackendConfig,
        _: &CredentialConfig,
        model: &BackendModelConfig,
        stream: bool,
    ) -> Value {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.partial && model.name.contains("sonnet") && stream {
            json!({"status":"failed","category":"synthetic","message":"secret-synthetic-access secret-synthetic-token"})
        } else {
            json!({"status":"passed","message_stop":stream,"usage":{"input_tokens":10,"output_tokens":2}})
        }
    }
}
fn config() -> Config {
    let mut config: Config =
        toml::from_str(include_str!("../../quotamux.bedrock.example.toml")).unwrap();
    config.upgrade().unwrap();
    config
}
#[tokio::test]
async fn all_four_models_and_modes_complete_without_store() {
    let fake = Fake {
        identity_ok: true,
        calls: AtomicUsize::new(0),
        partial: false,
    };
    let report = collect(Ok(config()), &fake, &mut Redactor::default(), false).await;
    assert_eq!(report["result"], "passed");
    assert_eq!(report["model_probes"].as_array().unwrap().len(), 8);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 8);
    assert!(report["config"].get("data_dir").is_none());
    assert!(!report.to_string().contains("api_key"));
}
struct StsFailure(Fake);
impl Checks for StsFailure {
    async fn identity(&self, _: &str, _: &mut Redactor) -> Identity {
        Identity {
            report: json!({"status":"failed","category":"transport","credentials_status":"passed"}),
            can_probe: true,
        }
    }
    async fn probe(
        &self,
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
        streaming: bool,
    ) -> Value {
        self.0.probe(backend, credential, model, streaming).await
    }
}
#[tokio::test]
async fn sts_failure_does_not_block_independent_bedrock_probes() {
    let fake = StsFailure(Fake {
        identity_ok: true,
        calls: AtomicUsize::new(0),
        partial: false,
    });
    let report = collect(Ok(config()), &fake, &mut Redactor::default(), false).await;
    assert_eq!(report["identity"]["credentials_status"], "passed");
    assert_eq!(fake.0.calls.load(Ordering::SeqCst), 8);
    assert!(
        report["model_probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["status"] == "passed")
    );
}
#[tokio::test]
async fn identity_failure_skips_all_eight_without_repeating_provider_resolution() {
    let fake = Fake {
        identity_ok: false,
        calls: AtomicUsize::new(0),
        partial: false,
    };
    let report = collect(Ok(config()), &fake, &mut Redactor::default(), false).await;
    assert_eq!(report["result"], "failed");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert!(
        report["model_probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["status"] == "skipped")
    );
}
#[tokio::test]
async fn partial_failure_keeps_later_probes_and_redacts_resolved_secrets() {
    let fake = Fake {
        identity_ok: true,
        calls: AtomicUsize::new(0),
        partial: true,
    };
    let report = collect(Ok(config()), &fake, &mut Redactor::default(), false).await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 8);
    assert_eq!(report["result"], "failed");
    assert_eq!(
        report["model_probes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["status"] == "failed")
            .count(),
        1
    );
    assert!(!report.to_string().contains("secret-synthetic"));
}
#[tokio::test]
async fn bad_config_still_generates_private_report_and_does_not_open_data() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let report = root.join("private/report.json");
    assert_eq!(run(&root.join("absent-config"), Some(&report)).await, 2);
    let value: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["config"]["category"], "config_read");
    assert_eq!(run(&root.join("absent-config"), Some(&report)).await, 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&report).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(report.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
#[test]
fn rejects_symlink_report_and_parent() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("target");
        fs::write(&target, "unchanged").unwrap();
        let link = root.join("link");
        symlink(&target, &link).unwrap();
        assert!(reserve_report(&link).is_err());
        let dir = root.join("linked");
        symlink(&root, &dir).unwrap();
        assert!(reserve_report(&dir.join("new.json")).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "unchanged");
    }
}
#[test]
fn snapshot_projection_never_copies_body_headers_or_messages() {
    let input = json!({"attempts":[{"model":"test","http_status":403,"message":"secret","upstream_message":"secret","headers":{"Authorization":"secret"},"body":{"messages":["secret"]}}],"targets":[{"circuit":{"state":"open"},"endpoint":"http://secret:secret@host"}]});
    let output = snapshot_fields(&input, 0);
    let text = output.to_string();
    assert!(!text.contains("secret"));
    assert_eq!(output["attempts"][0]["http_status"], 403);
    assert_eq!(output["targets"][0]["circuit"]["state"], "open");
}
#[tokio::test]
async fn parses_native_message_without_retaining_generated_text() {
    let config = config();
    let backend = &config.backends[0];
    let client = BackendClient::new_with_timeouts(
        backend,
        &backend.credentials[0],
        &backend.models[0],
        probe_timeouts(),
    )
    .unwrap();
    let value = json!({"type":"message","content":[{"type":"thinking","thinking":"must-not-copy","signature":"must-not-copy"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":4,"future":"must-not-copy"}});
    let response = reqwest::Response::from(axum::http::Response::new(reqwest::Body::from(
        value.to_string(),
    )));
    let result = consume(&client, response, false).await;
    assert_eq!(result["status"], "passed");
    assert!(!result.to_string().contains("must-not-copy"));
}
#[tokio::test]
async fn stream_must_finish_and_errors_are_not_success() {
    let config = config();
    let backend = &config.backends[0];
    let client = BackendClient::new_with_timeouts(
        backend,
        &backend.credentials[0],
        &backend.models[0],
        probe_timeouts(),
    )
    .unwrap();
    for (suffix, expected) in [
        (
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "passed",
        ),
        ("", "failed"),
        (
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"throttlingException\",\"message\":\"do-not-copy-secret\"}}\n\n",
            "failed",
        ),
    ] {
        let response = reqwest::Response::from(axum::http::Response::new(reqwest::Body::from(
            format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":1}}}}}}\n\n{suffix}"
            ),
        )));
        let mut result = consume(&client, response, true).await;
        let mut redactor = Redactor::default();
        redactor.add("do-not-copy-secret");
        redactor.clean(&mut result);
        assert_eq!(result["status"], expected);
        assert!(!result.to_string().contains("do-not-copy-secret"));
    }
}
#[tokio::test]
async fn localhost_gateway_snapshot_is_whitelisted_and_optional() {
    let app=axum::Router::new().fallback(||async{axum::Json(json!({"attempts":[{"http_status":403,"message":"secret-key","body":"secret-token"}],"targets":[{"circuit":{"state":"open"}}]}))});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = config();
    config.server.listen = addr.to_string();
    let result = snapshot(&config).await;
    assert_eq!(result["attempts"]["status"], "available");
    assert!(!result.to_string().contains("secret-"));
    task.abort();
    let result = snapshot(&config).await;
    assert_eq!(result["health"]["status"], "unavailable");
}

#[tokio::test]
async fn fake_sts_identity_keeps_only_account_role_and_request_id() {
    let xml = "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>arn:aws:sts::123456789012:assumed-role/test/session</Arn><UserId>do-not-store-user-id</UserId><Account>123456789012</Account></GetCallerIdentityResult><ResponseMetadata><RequestId>fake-request-id</RequestId></ResponseMetadata></GetCallerIdentityResponse>";
    let app = axum::Router::new().fallback(move || async move {
        (
            [
                (axum::http::header::CONTENT_TYPE, "text/xml"),
                (
                    axum::http::header::HeaderName::from_static("x-amzn-requestid"),
                    "fake-request-id",
                ),
            ],
            xml,
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut builder = aws_sdk_sts::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(aws_credential_types::Credentials::new(
            "fake-access-secret",
            "fake-key-secret",
            Some("fake-token-secret".into()),
            None,
            "fake-sts",
        ))
        .endpoint_url(format!("http://{addr}"))
        .retry_config(RetryConfig::disabled());
    #[cfg(feature = "standalone-tls")]
    {
        builder = builder.http_client(crate::tls::aws_http_client());
    }
    #[cfg(not(feature = "standalone-tls"))]
    let _ = &mut builder;
    let report = caller_identity(aws_sdk_sts::Client::from_conf(builder.build())).await;
    assert_eq!(report["status"], "passed");
    assert_eq!(report["account"], "123456789012");
    assert_eq!(report["request_id"], "fake-request-id");
    assert!(!report.to_string().contains("fake-access"));
    assert!(!report.to_string().contains("do-not-store"));
    task.abort();
}

#[test]
fn actual_circuit_attempt_and_error_bucket_schemas_preserve_root_cause_and_redact_secrets() {
    use crate::{
        circuit::{CircuitMode, CircuitSnapshot},
        types::{AttemptRecord, FailureClass, UpstreamErrorBucket},
    };
    let cause = "User arn:aws:sts::123456789012:assumed-role/BedrockRole/session is not authorized to perform bedrock:InvokeModel on arn:aws:bedrock:::foundation-model/anthropic.claude-opus-5-5 because of an explicit deny in a service control policy; key=synthetic-private-value";
    let circuit = CircuitSnapshot {
        mode: CircuitMode::Suspended,
        reason: Some(FailureClass::ProviderAuth),
        opened_at_ms: Some(100),
        next_probe_at_ms: Some(200),
        consecutive_failures: 2,
        backoff_level: 1,
        probe_in_flight: true,
        probe_deadline_at_ms: Some(300),
        generation: 4,
    };
    let attempt:AttemptRecord=serde_json::from_value(json!({"id":"attempt","request_id":"request","sequence":1,"backend":"aws-bedrock","adapter":"aws-bedrock","credential":"instance-role","route_layer":"global","upstream_model":"global.anthropic.claude-opus-5-5","served_model":"claude-opus-5-5","egress_protocol":"anthropic-messages","translated":false,"probe":false,"started_at_ms":100,"completed_at_ms":110,"status":403,"error_class":"provider_auth","retry_after_ms":10,"committed":false,"request_bytes":1,"response_bytes":2,"first_byte_ms":3,"total_ms":10,"usage":crate::types::Usage::default(),"provider_cost_usd":null,"sanitized_error":"upstream request failed","upstream_error_type":"AccessDeniedException","upstream_error_code":"Denied","upstream_error_message":cause})).unwrap();
    let bucket = UpstreamErrorBucket {
        upstream_type: Some("AccessDeniedException".into()),
        upstream_code: None,
        error_class: Some(FailureClass::ProviderAuth),
        message: Some(cause.into()),
        backend: Some("aws-bedrock".into()),
        count: 1,
        last_seen_ms: 110,
    };
    let fixture = json!({"targets":[{"circuit":circuit}],"attempts":[attempt],"errors":[bucket],"body":{"content":"must-not-copy"},"headers":{"authorization":"must-not-copy"}});
    let mut report = snapshot_fields(&fixture, 0);
    let mut redactor = Redactor::default();
    redactor.add("synthetic-private-value");
    redactor.clean(&mut report);
    assert_eq!(
        report["targets"][0]["circuit"],
        serde_json::to_value(circuit).unwrap()
    );
    for field in [
        "error_class",
        "sanitized_error",
        "upstream_error_type",
        "upstream_error_code",
        "started_at_ms",
        "completed_at_ms",
        "retry_after_ms",
        "committed",
        "translated",
    ] {
        assert_eq!(
            report["attempts"][0][field], fixture["attempts"][0][field],
            "{field}"
        );
    }
    for message in [
        &report["attempts"][0]["upstream_error_message"],
        &report["errors"][0]["message"],
    ] {
        let message = message.as_str().unwrap();
        assert!(message.contains("BedrockRole"));
        assert!(message.contains("bedrock:InvokeModel"));
        assert!(message.contains("service control policy"));
        assert!(!message.contains("synthetic-private-value"));
    }
    assert!(!report.to_string().contains("must-not-copy"));
}
#[test]
fn fresh_probe_retains_access_denied_explanation_and_safe_fallback() {
    let mut error = crate::provider::BackendError::local(
        crate::types::FailureClass::ProviderAuth,
        Some(reqwest::StatusCode::FORBIDDEN),
        None,
        "safe fallback",
    );
    error.upstream_message = Some(
        "Role BedrockRole cannot perform bedrock:InvokeModel because of SCP; synthetic-token-value"
            .into(),
    );
    let mut result = backend_failure(error);
    let mut redactor = Redactor::default();
    redactor.add("synthetic-token-value");
    redactor.clean(&mut result);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("because of SCP")
    );
    assert!(!result.to_string().contains("synthetic-token-value"));
    let result = backend_failure(crate::provider::BackendError::local(
        crate::types::FailureClass::ProviderTransient,
        None,
        None,
        "safe fallback",
    ));
    assert_eq!(result["message"], "safe fallback");
}
#[test]
fn environment_redaction_ignores_public_flags_profiles_and_unauthenticated_proxies() {
    let mut redactor = Redactor::default();
    for (key, value) in [
        ("AWS_PROFILE", "default"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
        ("AWS_REGION", "us-east-1"),
        ("AWS_CONFIG_FILE", "/home/ubuntu/.aws/config"),
        ("HTTP_PROXY", "http://proxy.internal:8080"),
        ("SOME_KEY_FLAG", "1"),
        ("TOKEN_ENABLED", "true"),
    ] {
        redactor.add_environment_value(key, value);
    }
    assert!(redactor.secrets.is_empty());
    redactor.add_environment_value("AWS_SESSION_TOKEN", "synthetic-session-token");
    redactor.add_environment_value(
        "HTTPS_PROXY",
        "http://proxyuser:proxy-password@proxy.internal:8080",
    );
    let mut result = json!({"message":"default true 1 us-east-1 http://proxy.internal:8080 synthetic-session-token http://proxyuser:proxy-password@proxy.internal:8080"});
    redactor.clean(&mut result);
    let text = result["message"].as_str().unwrap();
    assert!(text.contains("default true 1 us-east-1 http://proxy.internal:8080"));
    assert!(!text.contains("synthetic-session-token"));
    assert!(!text.contains("proxy-password"));
}
#[test]
fn secret_spanning_old_snapshot_truncation_boundary_is_redacted_before_bounding() {
    let secret = "synthetic-long-private-secret";
    let message = format!("{}{secret}", "x".repeat(250));
    let mut value = snapshot_fields(
        &json!({"errors":[{"upstream_type":"AccessDeniedException","message":message}]}),
        0,
    );
    let mut redactor = Redactor::default();
    redactor.add(secret);
    redactor.clean(&mut value);
    let text = value.to_string();
    assert!(!text.contains("synthetic-long"));
    assert!(text.contains("REDACTED"));
}
