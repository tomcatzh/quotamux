use std::{
    env,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::Router;
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{
        AdapterKind, RouteLayerConfig, RouteStrategy, RouteTargetConfig, ServedModelConfig,
        ServerConfig,
    },
    provider::BackendClient,
    types::{Protocol, Usage},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{sync::Mutex, task::JoinHandle};

const PROBE_MODEL: &str = "quotamux-subscription-quota-probe";
const LARGE_PROMPT_TOKENS: usize = 140_000;
static PORT_8081: OnceLock<Mutex<()>> = OnceLock::new();

#[tokio::test]
#[ignore = "set QUOTAMUX_CONFIRM_EXHAUST_GO_5H=1; deliberately exhausts real OpenCode Go quota"]
async fn exhaust_opencode_go_five_hour_limit_on_8081() {
    require_confirmation("QUOTAMUX_CONFIRM_EXHAUST_GO_5H");
    run_quota_probe(AdapterKind::OpenCodeGo, 100).await;
}

#[tokio::test]
#[ignore = "set QUOTAMUX_CONFIRM_EXHAUST_KIMI_QUOTA=1; deliberately exhausts a real Kimi Code subscription quota"]
async fn exhaust_kimi_code_subscription_limit_on_8081() {
    require_confirmation("QUOTAMUX_CONFIRM_EXHAUST_KIMI_QUOTA");
    run_quota_probe(AdapterKind::KimiCode, 200).await;
}

fn require_confirmation(name: &str) {
    assert_eq!(
        env::var(name).as_deref(),
        Ok("1"),
        "this destructive real-quota test requires {name}=1"
    );
}

async fn run_quota_probe(kind: AdapterKind, max_attempts: usize) {
    let _port_guard = PORT_8081.get_or_init(|| Mutex::new(())).lock().await;
    let config_path = env::var_os("QUOTAMUX_REAL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quotamux.toml"));
    let mut config = Config::load(config_path).expect("load private quota-probe configuration");
    let direct = DirectTarget::from_config(&config, kind);
    let route = direct.route.clone();
    let data_dir = TempDir::new().expect("isolated quota-probe data directory");
    config.server = ServerConfig {
        listen: "127.0.0.1:8081".into(),
        data_dir: data_dir.path().to_path_buf(),
    };
    config.models = vec![ServedModelConfig {
        name: PROBE_MODEL.into(),
        aliases: Vec::new(),
        protocols: vec![Protocol::OpenAiChat],
        layers: vec![RouteLayerConfig {
            name: "real-subscription-only".into(),
            strategy: RouteStrategy::Random,
            targets: vec![route],
        }],
    }];
    config
        .validate()
        .expect("validate isolated quota-probe config");

    let state = Arc::new(AppState::new(config).await.expect("quota-probe AppState"));
    let _gateway = RunningGateway::start(build_app(state)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .expect("quota-probe HTTP client");
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_millis();
    let large_context = " token".repeat(LARGE_PROMPT_TOKENS);
    let mut successful_requests = 0_u64;
    let mut input_tokens = 0_u64;
    let mut cache_hit_tokens = 0_u64;

    for attempt in 1..=max_attempts {
        let response = client
            .post("http://127.0.0.1:8081/v1/chat/completions")
            .header("x-relay-include-metadata", "1")
            .json(&json!({
                "model":PROBE_MODEL,
                "reasoning_effort":"low",
                "max_tokens":8,
                "messages":[{
                    "role":"user",
                    "content":format!(
                        "QuotaMux authorized five-hour quota probe {run_id}/{attempt}. Treat this prefix as unique.\n{large_context}\nReply with OK."
                    )
                }]
            }))
            .send()
            .await
            .unwrap_or_else(|error| panic!("{} attempt {attempt} failed: {error}", kind.as_str()));
        let status = response.status();
        let failure = response
            .headers()
            .get("x-relay-fallback-reason")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .await
            .expect("read quota-probe gateway response");

        if status.is_success() {
            let value = serde_json::from_slice::<Value>(&body)
                .expect("successful quota-probe response JSON");
            let usage = Usage::from_openai(&value);
            assert!(
                usage.provider_reported,
                "{} did not report usage on attempt {attempt}",
                kind.as_str()
            );
            successful_requests += 1;
            input_tokens = input_tokens.saturating_add(usage.input_tokens);
            cache_hit_tokens = cache_hit_tokens.saturating_add(usage.cache_hit_tokens);
            if attempt == 1 || attempt % 5 == 0 {
                eprintln!(
                    "REAL_QUOTA_PROGRESS {}",
                    json!({
                        "adapter":kind.as_str(),
                        "successful_requests":successful_requests,
                        "input_tokens":input_tokens,
                        "cache_hit_tokens":cache_hit_tokens
                    })
                );
            }
            continue;
        }

        match failure.as_deref() {
            Some("provider_quota") => {
                let capture = direct.capture_quota(&client).await;
                let quota_scope = capture.assert_subscription_quota(kind);
                assert_capped_quota_circuit(&client, &direct).await;
                eprintln!(
                    "REAL_QUOTA_EVIDENCE {}",
                    json!({
                        "listen":"127.0.0.1:8081",
                        "adapter":kind.as_str(),
                        "exhausted_during_this_invocation":successful_requests > 0,
                        "successful_requests":successful_requests,
                        "input_tokens":input_tokens,
                        "cache_hit_tokens":cache_hit_tokens,
                        "terminal_status":status.as_u16(),
                        "quota_scope":quota_scope,
                        "error_type":capture.controlled_error_type(),
                        "error_message_kind":capture.message_kind(),
                        "limit_name":capture.controlled_limit_name(),
                        "retry_after":capture.retry_after
                    })
                );
                return;
            }
            Some("provider_capacity")
            | Some("provider_transient")
            | Some("stream_failure")
            | Some("provider_unknown_5xx_or_transport") => {
                eprintln!(
                    "REAL_QUOTA_RETRY {}",
                    json!({
                        "adapter":kind.as_str(),
                        "attempt":attempt,
                        "status":status.as_u16(),
                        "failure_class":failure,
                        "body":"[upstream body redacted]"
                    })
                );
                wait_for_next_probe(&client, &direct).await;
            }
            other => {
                panic!(
                    "{} attempt {attempt} stopped with status {status}, class {other:?}: {}",
                    kind.as_str(),
                    "[upstream body redacted]"
                );
            }
        }
    }

    panic!(
        "{} did not reach its five-hour quota after {max_attempts} attempts ({} successes, {input_tokens} input tokens, {cache_hit_tokens} cached)",
        kind.as_str(),
        successful_requests
    );
}

struct RunningGateway {
    task: JoinHandle<()>,
}

impl RunningGateway {
    async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:8081")
            .await
            .expect("bind isolated quota probe to 127.0.0.1:8081");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { task }
    }
}

impl Drop for RunningGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct DirectTarget {
    route: RouteTargetConfig,
    endpoint: String,
    api_key: String,
}

impl DirectTarget {
    fn from_config(config: &Config, kind: AdapterKind) -> Self {
        let provider = config
            .backends
            .iter()
            .find(|provider| provider.adapter == kind)
            .unwrap_or_else(|| panic!("private config has no {} provider", kind.as_str()));
        let credential = provider
            .credentials
            .first()
            .expect("subscription provider credential");
        let preferred_model = match kind {
            AdapterKind::OpenCodeGo => "kimi-k3",
            AdapterKind::KimiCode => "k3",
            _ => unreachable!("quota probe supports subscription providers only"),
        };
        let model = provider
            .models
            .iter()
            .find(|model| model.name == preferred_model)
            .unwrap_or_else(|| panic!("{} has no {preferred_model} Chat model", kind.as_str()));
        let client = BackendClient::new(provider, credential, model)
            .expect("build subscription provider client");
        assert!(client.protocols().contains(&Protocol::OpenAiChat));
        Self {
            route: RouteTargetConfig {
                backend: provider.id.clone(),
                credential: credential.id.clone(),
                model: model.name.clone(),
            },
            endpoint: client.request_url(Protocol::OpenAiChat),
            api_key: credential.api_key.clone(),
        }
    }

    async fn capture_quota(&self, client: &reqwest::Client) -> QuotaCapture {
        let response = client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model":self.route.model,
                "max_tokens":8,
                "messages":[{"role":"user","content":"Reply with OK."}]
            }))
            .send()
            .await
            .expect("direct quota evidence response");
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let value = response
            .json::<Value>()
            .await
            .expect("direct quota evidence JSON");
        QuotaCapture {
            status,
            error_type: value
                .pointer("/error/type")
                .or_else(|| value.pointer("/error/code"))
                .or_else(|| value.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            message: value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            limit_name: value
                .pointer("/metadata/limitName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            retry_after,
        }
    }
}

struct QuotaCapture {
    status: u16,
    error_type: Option<String>,
    message: Option<String>,
    limit_name: Option<String>,
    retry_after: Option<u64>,
}

impl QuotaCapture {
    fn controlled_error_type(&self) -> Option<&str> {
        self.error_type.as_deref().map(|value| match value {
            "GoUsageLimitError" | "access_terminated_error" => value,
            _ => "[unrecognized]",
        })
    }

    fn controlled_limit_name(&self) -> Option<&str> {
        self.limit_name.as_deref().map(|value| match value {
            "5 hour" => value,
            _ => "[unrecognized]",
        })
    }

    fn message_kind(&self) -> &'static str {
        let message = self
            .message
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if message.contains("usage limit") || message.contains("quota exceeded") {
            "subscription_quota"
        } else {
            "[unrecognized]"
        }
    }

    fn assert_subscription_quota(&self, kind: AdapterKind) -> &'static str {
        let message = self
            .message
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match kind {
            AdapterKind::OpenCodeGo => {
                assert_eq!(self.status, 429, "unexpected OpenCode Go quota status");
                assert_eq!(self.error_type.as_deref(), Some("GoUsageLimitError"));
                assert_eq!(self.limit_name.as_deref(), Some("5 hour"));
                "five-hour"
            }
            AdapterKind::KimiCode => {
                if self.status == 403
                    && message.contains("usage limit")
                    && message.contains("billing cycle")
                {
                    return "weekly-billing-cycle";
                }
                if self.status == 429 && message.contains("monthly usage limit") {
                    return "monthly";
                }
                if self.status == 429 && message.contains("usage limit for this period") {
                    return "five-hour";
                }
                panic!(
                    "Kimi Code did not report a documented subscription quota (status {})",
                    self.status
                );
            }
            _ => unreachable!(),
        }
    }
}

async fn wait_for_next_probe(client: &reqwest::Client, target: &DirectTarget) {
    let circuit = circuit_status(client, target).await;
    let now = chrono::Utc::now().timestamp_millis();
    let deadline = circuit["next_probe_at_ms"]
        .as_i64()
        .expect("retryable circuit next-probe timestamp");
    let wait_ms = deadline.saturating_sub(now).max(0) as u64 + 100;
    assert!(
        wait_ms <= 300_100,
        "retryable circuit wait exceeded five minutes"
    );
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
}

async fn assert_capped_quota_circuit(client: &reqwest::Client, target: &DirectTarget) {
    let circuit = circuit_status(client, target).await;
    assert_eq!(circuit["mode"], "open");
    assert_eq!(circuit["reason"], "provider_quota");
    let opened = circuit["opened_at_ms"]
        .as_i64()
        .expect("quota circuit opened timestamp");
    let next = circuit["next_probe_at_ms"]
        .as_i64()
        .expect("quota circuit next-probe timestamp");
    assert!(next >= opened);
    assert!(next - opened <= Duration::from_secs(15 * 60).as_millis() as i64);
}

async fn circuit_status(client: &reqwest::Client, target: &DirectTarget) -> Value {
    let status = client
        .get("http://127.0.0.1:8081/api/status")
        .send()
        .await
        .expect("quota-probe status response")
        .json::<Value>()
        .await
        .expect("quota-probe status JSON");
    status["targets"]
        .as_array()
        .expect("status targets")
        .iter()
        .find(|candidate| {
            candidate["backend"] == target.route.backend
                && candidate["credential"] == target.route.credential
                && candidate["model"] == target.route.model
        })
        .unwrap_or_else(|| panic!("missing {} circuit status", target.route.backend))["circuit"]
        .clone()
}
