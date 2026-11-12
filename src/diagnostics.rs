//! Explicit, bounded, non-persistent diagnostics. Never opens the gateway store.
use crate::{
    Config,
    config::{
        AdapterKind, BackendConfig, BackendModelConfig, CredentialConfig, ServerTimeoutConfig,
    },
    provider::BackendClient,
    sse::SseDecoder,
    types::Protocol,
};
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_sts::{
    config::{BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig},
    error::ProvideErrorMetadata,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const ENV_FLAGS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ENDPOINT_URL",
    "AWS_ENDPOINT_URL_BEDROCK_RUNTIME",
    "AWS_ENDPOINT_URL_STS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];
const FIXED_PROMPT: &str = "Reply with OK.";

#[derive(Default)]
struct Redactor {
    secrets: Vec<String>,
}
impl Redactor {
    fn environment() -> Self {
        let mut value = Self::default();
        for (key, secret) in std::env::vars_os() {
            value.add_environment_value(&key.to_string_lossy(), &secret.to_string_lossy());
        }
        value
    }
    fn add_environment_value(&mut self, key: &str, value: &str) {
        let key = key.to_ascii_lowercase();
        // Profiles, regions, flags and file paths are configuration, not secret values.
        // Public short/default values must not corrupt unrelated report strings.
        let public_value = value.len() < 4
            || matches!(
                value.to_ascii_lowercase().as_str(),
                "default" | "true" | "false" | "none" | "null" | "yes" | "no"
            );
        if public_value {
            return;
        }
        if !key.ends_with("_file")
            && (key.contains("secret")
                || key.contains("token")
                || key.contains("password")
                || key.contains("key"))
        {
            self.add(value);
        } else if key.contains("proxy")
            && let Ok(url) = reqwest::Url::parse(value)
            && (!url.username().is_empty() || url.password().is_some())
        {
            self.add(value);
            if let Some(password) = url.password().filter(|password| password.len() >= 4) {
                self.add(password);
            }
        }
    }
    fn add(&mut self, value: &str) {
        if !value.is_empty() {
            self.secrets.push(value.to_owned());
            self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }
    fn clean(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                for secret in &self.secrets {
                    *text = text.replace(secret, "[REDACTED]");
                }
                *text = text.chars().take(2048).collect();
            }
            Value::Array(items) => {
                for item in items {
                    self.clean(item);
                }
            }
            Value::Object(items) => {
                for item in items.values_mut() {
                    self.clean(item);
                }
            }
            _ => {}
        }
    }
}
struct Identity {
    report: Value,
    can_probe: bool,
}
trait Checks {
    async fn identity(&self, region: &str, redactor: &mut Redactor) -> Identity;
    async fn probe(
        &self,
        backend: &BackendConfig,
        credential: &CredentialConfig,
        model: &BackendModelConfig,
        streaming: bool,
    ) -> Value;
}
struct LiveChecks;
impl Checks for LiveChecks {
    async fn identity(&self, region: &str, redactor: &mut Redactor) -> Identity {
        let credentials = tokio::time::timeout(Duration::from_secs(10), async {
            let config = crate::tls::aws_loader(
                aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(region.to_owned())),
            )
            .load()
            .await;
            let Some(provider) = config.credentials_provider() else {
                return Err("credentials_missing".to_owned());
            };
            provider
                .provide_credentials()
                .await
                .map_err(|error| credential_category(&error).to_owned())
        })
        .await;
        let credentials = match credentials {
            Ok(Ok(credentials)) => credentials,
            Ok(Err(category)) => {
                return Identity {
                    report: failure(&category),
                    can_probe: false,
                };
            }
            Err(_) => {
                return Identity {
                    report: failure("timeout"),
                    can_probe: false,
                };
            }
        };
        redactor.add(credentials.access_key_id());
        redactor.add(credentials.secret_access_key());
        if let Some(token) = credentials.session_token() {
            redactor.add(token);
        }
        let expires_at = credentials
            .expiry()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map(|v| v.to_rfc3339());
        let mut builder = aws_sdk_sts::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_owned()))
            .credentials_provider(credentials)
            .endpoint_url(format!("https://sts.{region}.amazonaws.com"))
            .retry_config(RetryConfig::disabled())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .operation_timeout(Duration::from_secs(10))
                    .build(),
            );
        #[cfg(feature = "standalone-tls")]
        {
            builder = builder.http_client(crate::tls::aws_http_client());
        }
        #[cfg(not(feature = "standalone-tls"))]
        let _ = &mut builder;
        let mut report = json!({"provider_name":"default_chain", "provider_name_exact_available":false, "expires_at":expires_at, "credentials_status":"passed"});
        let result = caller_identity(aws_sdk_sts::Client::from_conf(builder.build())).await;
        report
            .as_object_mut()
            .unwrap()
            .extend(result.as_object().unwrap().clone());
        Identity {
            report,
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
        let timeouts = probe_timeouts();
        let Ok(client) = BackendClient::new_with_timeouts(backend, credential, model, timeouts)
        else {
            return failure("backend_configuration");
        };
        let body = json!({"model":model.name,"max_tokens":256,"stream":streaming,"output_config":{"effort":"low"},"messages":[{"role":"user","content":FIXED_PROMPT}]});
        let result = tokio::time::timeout(Duration::from_secs(45), async {
            match client
                .send(
                    Protocol::AnthropicMessages,
                    &body,
                    &reqwest::header::HeaderMap::new(),
                )
                .await
            {
                Ok(response) => consume(&client, response, streaming).await,
                Err(error) => backend_failure(error),
            }
        })
        .await;
        result.unwrap_or_else(|_| failure("timeout"))
    }
}
async fn caller_identity(client: aws_sdk_sts::Client) -> Value {
    use aws_sdk_sts::operation::RequestId;
    match tokio::time::timeout(Duration::from_secs(10), client.get_caller_identity().send()).await {
        Ok(Ok(identity)) => {
            json!({"status":"passed","account":identity.account(),"role_arn":identity.arn(),"request_id":identity.request_id()})
        }
        Ok(Err(error)) => {
            let category = error
                .as_service_error()
                .and_then(|v| v.code())
                .map(safe_code)
                .unwrap_or_else(|| source_category(&error).into());
            failure(&category)
        }
        Err(_) => failure("timeout"),
    }
}
fn probe_timeouts() -> ServerTimeoutConfig {
    ServerTimeoutConfig {
        upstream_connect_ms: 5000,
        upstream_read_ms: 30000,
        upstream_stream_read_ms: 30000,
        upstream_total_ms: 45000,
        ..ServerTimeoutConfig::default()
    }
}
fn failure(category: &str) -> Value {
    json!({"status":"failed","category":category})
}
fn safe_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 128
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        code.to_owned()
    } else {
        "unknown_aws_error".into()
    }
}
fn credential_category(
    error: &aws_credential_types::provider::error::CredentialsError,
) -> &'static str {
    use aws_credential_types::provider::error::CredentialsError;
    match error {
        CredentialsError::ProviderTimedOut(_) => "timeout",
        CredentialsError::InvalidConfiguration(_) => "profile",
        _ if metadata_disabled() => "imds_disabled",
        _ => match source_category(error) {
            "transport" => "credentials_missing",
            category => category,
        },
    }
}
fn source_category(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut current = Some(error);
    while let Some(error) = current {
        // Inspect locally only. Never serialize source text, SDK Debug or raw responses.
        let text = error.to_string().to_ascii_lowercase();
        if text.contains("timed out") || text.contains("timeout") {
            return "timeout";
        }
        if text.contains("dns") || text.contains("resolve host") || text.contains("name resolution")
        {
            return "dns";
        }
        if text.contains("certificate") || text.contains("tls") {
            return "tls";
        }
        if text.contains("credential_process") || text.contains("credential process") {
            return "credentials_process";
        }
        current = error.source();
    }
    "transport"
}
fn metadata_disabled() -> bool {
    std::env::var("AWS_EC2_METADATA_DISABLED").is_ok_and(|v| v.eq_ignore_ascii_case("true"))
}
fn backend_failure(error: crate::provider::BackendError) -> Value {
    json!({"status":"failed","category":error.class.as_str(),"http_status":error.status.map(|v|v.as_u16()),"upstream_type":error.upstream_type.as_deref().map(safe_code),"upstream_code":error.upstream_code.as_deref().map(safe_code),"message":error.upstream_message.as_deref().unwrap_or(&error.safe_message),"retry_after_seconds":error.retry_after.map(|v|v.as_secs())})
}
fn usage(value: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        if let Some(count) = value.get(key).and_then(Value::as_u64) {
            result.insert(key.into(), json!(count));
        }
    }
    Value::Object(result)
}
async fn consume(client: &BackendClient, response: reqwest::Response, streaming: bool) -> Value {
    let status = response.status();
    if !streaming {
        let Ok(value) = response.json::<Value>().await else {
            return failure("invalid_json");
        };
        if let Some(error) = client.classify_semantic_error(status, &value) {
            return backend_failure(error);
        }
        if value.get("type").and_then(Value::as_str) != Some("message")
            || !value.get("content").is_some_and(Value::is_array)
        {
            return failure("invalid_message");
        }
        return json!({"status":"passed","http_status":status.as_u16(),"usage":usage(&value["usage"]),"stop_reason":value.get("stop_reason").and_then(Value::as_str).map(safe_code)});
    }
    let mut decoder = SseDecoder::new(Box::pin(response.bytes_stream()));
    let mut counts = json!({});
    let mut stop_reason = None;
    let mut started = false;
    loop {
        let event = match decoder.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => return failure("missing_message_stop"),
            Err(_) => return failure("stream_transport"),
        };
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return failure("invalid_stream_json");
        };
        if let Some(error) = client.classify_semantic_error(status, &value) {
            return backend_failure(error);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                started = true;
                counts = usage(&value["message"]["usage"]);
            }
            Some("message_delta") => {
                if let Some(c) = usage(&value["usage"]).as_object() {
                    counts.as_object_mut().unwrap().extend(c.clone());
                }
                stop_reason = value["delta"]["stop_reason"].as_str().map(safe_code);
            }
            Some("message_stop") if started => {
                return json!({"status":"passed","http_status":status.as_u16(),"message_stop":true,"usage":counts,"stop_reason":stop_reason});
            }
            Some("message_stop") => return failure("missing_message_start"),
            _ => {}
        }
    }
}

fn config_summary(config: &Config) -> Value {
    json!({"status":"passed","config_version":config.config_version,"timeouts":config.server.timeouts,"bedrock":config.backends.iter().filter(|b|b.adapter==AdapterKind::AwsBedrock).map(|b|json!({"backend":b.id,"region":b.region,"upstream_models":b.models.iter().map(|m|m.name.as_str()).collect::<Vec<_>>()})).collect::<Vec<_>>(),"served_models":config.models.iter().map(|m|json!({"name":m.name,"aliases":m.aliases,"protocols":m.protocols})).collect::<Vec<_>>()})
}
fn environment() -> Value {
    let flags = ENV_FLAGS
        .iter()
        .map(|key| ((*key).to_owned(), std::env::var_os(key).is_some()))
        .collect::<BTreeMap<_, _>>();
    json!({"set":flags,"metadata_disabled":metadata_disabled()})
}
fn gateway_base(config: &Config) -> Option<String> {
    let mut addr: SocketAddr = config.server.listen.parse().ok()?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv6() {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        });
    }
    Some(format!("http://{addr}"))
}
// Recursively retain only scalar counters and explicitly named diagnosis fields.
// Prompt/response content, arbitrary messages, headers and endpoint URLs are excluded.
fn snapshot_fields(value: &Value, depth: usize) -> Value {
    const KEYS: &[&str] = &[
        "ok",
        "status",
        "version",
        "models",
        "targets",
        "backend",
        "adapter",
        "credential",
        "model",
        "circuit",
        "state",
        "open_until_ms",
        "failures",
        "consecutive_failures",
        "cooldown_until_ms",
        "last_failure_class",
        "last_failure_ms",
        "attempts",
        "errors",
        "mode",
        "opened_at_ms",
        "next_probe_at_ms",
        "backoff_level",
        "probe_in_flight",
        "probe_deadline_at_ms",
        "generation",
        "error_class",
        "sanitized_error",
        "upstream_error_type",
        "upstream_error_code",
        "upstream_error_message",
        "completed_at_ms",
        "retry_after_ms",
        "committed",
        "translated",
        "route_layer",
        "route_layer_index",
        "served_model",
        "total_ms",
        "failure_class",
        "class",
        "http_status",
        "status_code",
        "upstream_type",
        "upstream_code",
        "count",
        "last_seen_ms",
        "first_seen_ms",
        "created_at_ms",
        "started_at_ms",
        "finished_at_ms",
        "duration_ms",
        "latency_ms",
        "success",
        "request_id",
        "attempt_id",
        "id",
        "stream",
        "streaming",
        "ingress_protocol",
        "egress_protocol",
        "outcome",
        "reason",
        "layer",
        "logical_model",
        "upstream_model",
    ];
    if depth > 8 {
        return Value::Null;
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    KEYS.contains(&key.as_str())
                        || (key.as_str() == "message"
                            && (map.contains_key("upstream_type")
                                || map.contains_key("upstream_code")
                                || map.contains_key("error_class")))
                })
                .map(|(key, value)| (key.clone(), snapshot_fields(value, depth + 1)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(|v| snapshot_fields(v, depth + 1))
                .collect(),
        ),
        // Redact full values before bounding them at the final publication boundary.
        Value::String(value) => json!(value),
        value => value.clone(),
    }
}
async fn snapshot(config: &Config) -> Value {
    let Some(base) = gateway_base(config) else {
        return json!({"status":"unavailable"});
    };
    let Ok(builder) = crate::tls::reqwest_builder() else {
        return json!({"status":"unavailable"});
    };
    let Ok(client) = builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return json!({"status":"unavailable"});
    };
    let mut result = serde_json::Map::new();
    for (name, path) in [
        ("health", "/healthz"),
        ("status", "/api/status"),
        ("attempts", "/api/attempts?limit=20"),
        ("upstream_errors", "/api/upstream-errors"),
    ] {
        let value = tokio::time::timeout(Duration::from_secs(3), async {
            let mut response = client.get(format!("{base}{path}")).send().await.ok()?;
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.ok()? {
                if bytes.len() + chunk.len() > 256 * 1024 {
                    return Some(json!({"status":"unavailable","category":"snapshot_too_large"}));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = serde_json::from_slice::<Value>(&bytes).ok()?;
            Some(
                json!({"status":"available","http_status":status,"data":snapshot_fields(&value,0)}),
            )
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({"status":"unavailable"}));
        result.insert(name.into(), value);
    }
    Value::Object(result)
}
async fn collect<C: Checks>(
    config: Result<Config, crate::config::ConfigError>,
    checks: &C,
    redactor: &mut Redactor,
    include_snapshot: bool,
) -> Value {
    use futures_util::FutureExt;
    let mut report = json!({"schema_version":1,"package":env!("CARGO_PKG_NAME"),"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"standalone_tls":cfg!(feature="standalone-tls"),"time":chrono::Utc::now().to_rfc3339(),"environment":environment(),"probe_timeouts":probe_timeouts(),"model_probes":[]});
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            let category = match error {
                crate::config::ConfigError::Read { .. } => "config_read",
                crate::config::ConfigError::Parse { .. } => "config_parse",
                crate::config::ConfigError::Invalid(_) => "config_validation",
            };
            report["config"] = failure(category);
            report["identity"] = json!({"status":"skipped","reason":"invalid_config"});
            report["gateway_snapshot"] = json!({"status":"skipped","reason":"invalid_config"});
            report["result"] = json!("failed");
            redactor.clean(&mut report);
            return report;
        }
    };
    for backend in &config.backends {
        for credential in &backend.credentials {
            redactor.add(&credential.api_key);
        }
    }
    report["config"] = config_summary(&config);
    report["gateway_snapshot"] = if include_snapshot {
        snapshot(&config).await
    } else {
        json!({"status":"skipped","reason":"offline_test"})
    };
    let backends = config
        .backends
        .iter()
        .filter(|backend| backend.adapter == AdapterKind::AwsBedrock)
        .collect::<Vec<_>>();
    let Some(region) = backends.first().and_then(|b| b.region.as_deref()) else {
        report["identity"] = json!({"status":"skipped","reason":"no_bedrock_models"});
        report["result"] = json!("failed");
        redactor.clean(&mut report);
        return report;
    };
    report["identity_scope"] = json!({"credential_chain":"default_chain","sts_region_checked":region,"bedrock_probe_regions":backends.iter().filter_map(|b|b.region.as_deref()).collect::<std::collections::BTreeSet<_>>()});
    let identity = std::panic::AssertUnwindSafe(checks.identity(region, redactor))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Identity {
            report: failure("sdk_initialization"),
            can_probe: false,
        });
    let mut passed = identity.report["status"] == "passed";
    report["identity"] = identity.report;
    let mut probes = Vec::new();
    for backend in backends {
        for model in &backend.models {
            for streaming in [false, true] {
                let started = Instant::now();
                let mut result = if identity.can_probe {
                    std::panic::AssertUnwindSafe(checks.probe(
                        backend,
                        &backend.credentials[0],
                        model,
                        streaming,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| failure("sdk_initialization"))
                } else {
                    json!({"status":"skipped","reason":"credentials_unavailable"})
                };
                if result["status"] != "passed" {
                    passed = false;
                }
                result["backend"] = json!(backend.id);
                result["model"] = json!(model.name);
                result["streaming"] = json!(streaming);
                result["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
                probes.push(result);
            }
        }
    }
    report["model_probes"] = json!(probes);
    report["result"] = json!(if passed { "passed" } else { "failed" });
    redactor.clean(&mut report);
    report
}
fn reserve_report(path: &Path) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| io::Error::other("report parent missing"))?;
    let mut cursor = PathBuf::new();
    for component in parent.components() {
        if matches!(component, Component::ParentDir) {
            return Err(io::Error::other("parent traversal is not allowed"));
        }
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::other("unsafe report directory"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&cursor)?;
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("report directory must be private (0700)"));
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(absolute)
}
/// Execute explicit diagnostics. No store, workspace or persisted circuit state is opened.
pub async fn run(config_path: &Path, report_path: Option<&Path>) -> i32 {
    let path = report_path.map(Path::to_path_buf).unwrap_or_else(|| {
        PathBuf::from(format!(
            "logs/diagnostics-{}-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            std::process::id()
        ))
    });
    let mut file = match reserve_report(&path) {
        Ok(file) => file,
        Err(_) => {
            eprintln!("无法创建私有诊断报告；请使用权限为 0700 的真实目录及尚不存在的文件路径。");
            return 1;
        }
    };
    let mut redactor = Redactor::environment();
    let report = collect(Config::load(config_path), &LiveChecks, &mut redactor, true).await;
    if serde_json::to_writer_pretty(&mut file, &report)
        .and_then(|_| file.write_all(b"\n").map_err(serde_json::Error::io))
        .is_err()
        || file.sync_all().is_err()
    {
        eprintln!("无法完整写入诊断报告。");
        return 1;
    }
    let passed = report["result"] == "passed";
    println!(
        "诊断{}；报告：{}",
        if passed { "通过" } else { "发现问题" },
        path.display()
    );
    if passed { 0 } else { 2 }
}

#[cfg(test)]
mod tests;
