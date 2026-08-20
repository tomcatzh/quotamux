use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{
        AdapterKind, BackendConfig, BackendModelConfig, CredentialConfig, LOGICAL_MODEL,
        ModelPricingConfig, RouteLayerConfig, RouteStrategy, RouteTargetConfig, ServedModelConfig,
        ServerConfig, UPSTREAM_MODEL,
    },
    types::Protocol,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{sync::Mutex, task::JoinHandle};

enum MockBody {
    Json(Value),
    Raw(String),
    HangingSse(String),
    PausedSse {
        first: String,
        rest: String,
        pause: Duration,
    },
}

struct MockReply {
    status: StatusCode,
    body: MockBody,
    headers: Vec<(String, String)>,
    delay: Option<Duration>,
}

impl MockReply {
    fn json(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body: MockBody::Json(body),
            headers: Vec::new(),
            delay: None,
        }
    }

    fn sse(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: MockBody::Raw(body.into()),
            headers: vec![(CONTENT_TYPE.as_str().into(), "text/event-stream".into())],
            delay: None,
        }
    }

    fn hanging_sse(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: MockBody::HangingSse(body.into()),
            headers: vec![(CONTENT_TYPE.as_str().into(), "text/event-stream".into())],
            delay: None,
        }
    }

    fn paused_sse(first: impl Into<String>, rest: impl Into<String>, pause: Duration) -> Self {
        Self {
            status: StatusCode::OK,
            body: MockBody::PausedSse {
                first: first.into(),
                rest: rest.into(),
                pause,
            },
            headers: vec![(CONTENT_TYPE.as_str().into(), "text/event-stream".into())],
            delay: None,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn into_response(self) -> Response {
        let mut response = match self.body {
            MockBody::Json(value) => Json(value).into_response(),
            MockBody::Raw(body) => Response::new(Body::from(body)),
            MockBody::HangingSse(body) => {
                let stream = async_stream::stream! {
                    yield Ok::<Bytes, Infallible>(Bytes::from(body));
                    std::future::pending::<()>().await;
                };
                Response::new(Body::from_stream(stream))
            }
            MockBody::PausedSse { first, rest, pause } => {
                let stream = async_stream::stream! {
                    yield Ok::<Bytes, Infallible>(Bytes::from(first));
                    tokio::time::sleep(pause).await;
                    yield Ok::<Bytes, Infallible>(Bytes::from(rest));
                };
                Response::new(Body::from_stream(stream))
            }
        };
        *response.status_mut() = self.status;
        for (name, value) in self.headers {
            response.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("valid mock header name"),
                HeaderValue::from_str(&value).expect("valid mock header value"),
            );
        }
        response
    }
}

struct MockProviderState {
    replies: Mutex<VecDeque<MockReply>>,
    calls: AtomicUsize,
    request_bodies: Mutex<Vec<Value>>,
    request_headers: Mutex<Vec<HeaderMap>>,
    request_paths: Mutex<Vec<String>>,
}

async fn mock_provider_handler(
    State(state): State<Arc<MockProviderState>>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.request_bodies.lock().await.push(body);
    state.request_headers.lock().await.push(headers);
    state.request_paths.lock().await.push(uri.path().into());
    let reply = state.replies.lock().await.pop_front().unwrap_or_else(|| {
        MockReply::json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":{"message":"unexpected mock provider call"}}),
        )
    });
    if let Some(delay) = reply.delay {
        tokio::time::sleep(delay).await;
    }
    reply.into_response()
}

struct MockProvider {
    address: SocketAddr,
    state: Arc<MockProviderState>,
    task: JoinHandle<()>,
}

impl MockProvider {
    async fn start(replies: Vec<MockReply>) -> Self {
        let state = Arc::new(MockProviderState {
            replies: Mutex::new(replies.into_iter().collect()),
            calls: AtomicUsize::new(0),
            request_bodies: Mutex::new(Vec::new()),
            request_headers: Mutex::new(Vec::new()),
            request_paths: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .fallback(mock_provider_handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let address = listener.local_addr().expect("mock provider address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    async fn request_bodies(&self) -> Vec<Value> {
        self.state.request_bodies.lock().await.clone()
    }

    async fn request_headers(&self) -> Vec<HeaderMap> {
        self.state.request_headers.lock().await.clone()
    }

    async fn request_paths(&self) -> Vec<String> {
        self.state.request_paths.lock().await.clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Gateway {
    address: SocketAddr,
    _data_dir: TempDir,
    task: JoinHandle<()>,
}

impl Gateway {
    async fn start(primary: &MockProvider, fallback: &MockProvider) -> Self {
        let config = Config {
            config_version: 3,
            server: ServerConfig {
                listen: "127.0.0.1:0".into(),
                data_dir: "unused-test-data".into(),
                timeouts: Default::default(),
            },
            affinity: Default::default(),
            backends: vec![
                BackendConfig {
                    id: "opencode-go".into(),
                    adapter: AdapterKind::OpenCodeGo,
                    endpoint: Some(primary.endpoint()),
                    credentials: vec![CredentialConfig {
                        id: "go-plan".into(),
                        api_key: "test-opencode-key".into(),
                    }],
                    models: vec![BackendModelConfig {
                        name: UPSTREAM_MODEL.into(),
                        protocols: Vec::new(),
                        pricing: None,
                    }],
                },
                BackendConfig {
                    id: "deepseek".into(),
                    adapter: AdapterKind::DeepSeekOfficial,
                    endpoint: Some(fallback.endpoint()),
                    credentials: vec![CredentialConfig {
                        id: "deepseek-payg".into(),
                        api_key: "test-deepseek-key".into(),
                    }],
                    models: vec![BackendModelConfig {
                        name: UPSTREAM_MODEL.into(),
                        protocols: vec![
                            Protocol::OpenAiChat,
                            Protocol::OpenAiResponses,
                            Protocol::AnthropicMessages,
                        ],
                        pricing: None,
                    }],
                },
            ],
            models: vec![ServedModelConfig {
                name: LOGICAL_MODEL.into(),
                aliases: vec![UPSTREAM_MODEL.into()],
                protocols: vec![
                    Protocol::OpenAiChat,
                    Protocol::OpenAiResponses,
                    Protocol::AnthropicMessages,
                ],
                layers: vec![
                    RouteLayerConfig {
                        name: "plan".into(),
                        strategy: RouteStrategy::Random,
                        targets: vec![RouteTargetConfig {
                            backend: "opencode-go".into(),
                            credential: "go-plan".into(),
                            model: UPSTREAM_MODEL.into(),
                        }],
                    },
                    RouteLayerConfig {
                        name: "payg".into(),
                        strategy: RouteStrategy::Random,
                        targets: vec![RouteTargetConfig {
                            backend: "deepseek".into(),
                            credential: "deepseek-payg".into(),
                            model: UPSTREAM_MODEL.into(),
                        }],
                    },
                ],
            }],
        };
        Self::start_config(config, 0x5eed).await
    }

    async fn start_config(mut config: Config, seed: u64) -> Self {
        let mut endpoint_overrides = HashMap::new();
        for backend in &mut config.backends {
            if matches!(
                backend.adapter,
                AdapterKind::DeepSeekOfficial
                    | AdapterKind::KimiOfficial
                    | AdapterKind::KimiCode
                    | AdapterKind::OpenCodeGo
            ) && let Some(endpoint) = backend.endpoint.take()
            {
                endpoint_overrides.insert(backend.id.clone(), endpoint);
            }
        }
        let data_dir = tempfile::tempdir().expect("create gateway data directory");
        config.server.data_dir = data_dir.path().to_path_buf();
        let state = Arc::new(
            AppState::new_with_random_seed_and_test_endpoints(config, seed, endpoint_overrides)
                .await
                .expect("create gateway state"),
        );
        let app = build_app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway");
        let address = listener.local_addr().expect("gateway address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            address,
            _data_dir: data_dir,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn chat_completion(reasoning: &str, content: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": UPSTREAM_MODEL,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning_content": reasoning,
                "content": content
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "total_tokens": 18,
            "prompt_cache_hit_tokens": 3,
            "prompt_cache_miss_tokens": 8,
            "completion_tokens_details": {"reasoning_tokens": 4}
        },
        "system_fingerprint": "fp-upstream-test"
    })
}

fn chat_request() -> Value {
    chat_request_with_content("hello")
}

fn chat_request_with_content(content: &str) -> Value {
    json!({
        "model": LOGICAL_MODEL,
        "messages": [{"role":"user","content":content}]
    })
}

fn test_backend(id: &str, credential: &str, upstream: &MockProvider) -> BackendConfig {
    test_backend_adapter(
        id,
        credential,
        upstream,
        AdapterKind::OpenCodeGo,
        Protocol::OpenAiChat,
    )
}

fn test_backend_protocol(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    protocol: Protocol,
) -> BackendConfig {
    let kind = match protocol {
        Protocol::OpenAiChat => AdapterKind::CustomChatCompletions,
        Protocol::OpenAiResponses => AdapterKind::CustomResponses,
        Protocol::AnthropicMessages => AdapterKind::CustomAnthropic,
    };
    test_backend_adapter(id, credential, upstream, kind, protocol)
}

fn test_backend_adapter(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    kind: AdapterKind,
    protocol: Protocol,
) -> BackendConfig {
    test_backend_adapter_model(id, credential, upstream, kind, protocol, UPSTREAM_MODEL)
}

fn test_backend_adapter_model(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    kind: AdapterKind,
    protocol: Protocol,
    model: &str,
) -> BackendConfig {
    BackendConfig {
        id: id.into(),
        adapter: kind,
        endpoint: Some(upstream.endpoint()),
        credentials: vec![CredentialConfig {
            id: credential.into(),
            api_key: format!("test-key-{credential}"),
        }],
        models: vec![BackendModelConfig {
            name: model.into(),
            protocols: if kind == AdapterKind::OpenCodeGo {
                Vec::new()
            } else {
                vec![protocol]
            },
            pricing: None,
        }],
    }
}

fn target(backend: &str, credential: &str) -> RouteTargetConfig {
    target_model(backend, credential, UPSTREAM_MODEL)
}

fn target_model(backend: &str, credential: &str, model: &str) -> RouteTargetConfig {
    RouteTargetConfig {
        backend: backend.into(),
        credential: credential.into(),
        model: model.into(),
    }
}

fn test_config(
    backends: Vec<BackendConfig>,
    layers: Vec<(&str, Vec<RouteTargetConfig>)>,
) -> Config {
    Config {
        config_version: 3,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
            timeouts: Default::default(),
        },
        affinity: Default::default(),
        backends,
        models: vec![ServedModelConfig {
            name: LOGICAL_MODEL.into(),
            aliases: vec![UPSTREAM_MODEL.into()],
            protocols: vec![
                Protocol::OpenAiChat,
                Protocol::OpenAiResponses,
                Protocol::AnthropicMessages,
            ],
            layers: layers
                .into_iter()
                .map(|(name, targets)| RouteLayerConfig {
                    name: name.into(),
                    strategy: RouteStrategy::Random,
                    targets,
                })
                .collect(),
        }],
    }
}

fn chat_stream(reasoning: &str, content: &str, done: bool) -> String {
    let mut stream = format!(
        "data: {}\n\n",
        json!({
            "id":"chatcmpl-stream",
            "choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":reasoning,"content":content}}]
        })
    );
    if done {
        stream.push_str("data: [DONE]\n\n");
    }
    stream
}

fn header_value(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing response header {name}"))
        .to_str()
        .expect("valid response header")
        .to_owned()
}

fn attempts_for_request<'a>(body: &'a Value, request_id: &str) -> Vec<&'a Value> {
    body["attempts"]
        .as_array()
        .expect("attempts array")
        .iter()
        .filter(|attempt| attempt["request_id"].as_str() == Some(request_id))
        .collect()
}

fn status_target<'a>(body: &'a Value, backend: &str) -> &'a Value {
    body["targets"]
        .as_array()
        .expect("status targets array")
        .iter()
        .find(|target| target["backend"].as_str() == Some(backend))
        .unwrap_or_else(|| panic!("missing status target {backend}"))
}

#[tokio::test]
async fn openai_chat_success_exposes_reasoning_and_backend_metadata() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("primary reasoning", "primary answer"),
    )])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("chat response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "opencode-go");
    assert!(response.headers().get("x-relay-provider").is_none());
    assert_eq!(header_value(&response, "x-relay-fallback"), "0");
    let body = response.json::<Value>().await.expect("chat JSON");
    assert_eq!(
        body["choices"][0]["message"]["reasoning_content"],
        "primary reasoning"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "primary answer");
    assert_eq!(body["system_fingerprint"], "fp-upstream-test");
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("backend status response")
        .json::<Value>()
        .await
        .expect("backend status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["adapter"], "opencode-go");
    assert!(target.get("provider").is_none());
    assert!(target.get("provider_kind").is_none());

    let stats = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("backend stats response")
        .json::<Value>()
        .await
        .expect("backend stats JSON");
    assert!(stats.get("providers").is_none());
    assert_eq!(stats["backends"]["deepseek"]["attempts"], 0);
    assert_eq!(
        stats["backends"]["deepseek"]["models"],
        json!([UPSTREAM_MODEL])
    );
}

#[tokio::test]
async fn configured_model_pricing_drives_cost_for_any_adapter() {
    let upstream = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("priced reasoning", "priced answer"),
    )])
    .await;
    let mut config = test_config(
        vec![test_backend_adapter(
            "priced-custom",
            "priced-key",
            &upstream,
            AdapterKind::CustomChatCompletions,
            Protocol::OpenAiChat,
        )],
        vec![("priced", vec![target("priced-custom", "priced-key")])],
    );
    config.backends[0].models[0].pricing = Some(ModelPricingConfig {
        cache_hit_input_usd_per_million: 1.0,
        cache_miss_input_usd_per_million: 2.0,
        output_usd_per_million: 3.0,
    });
    let gateway = Gateway::start_config(config, 0x0c057).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .expect("priced response");
    assert_eq!(response.status(), StatusCode::OK);

    let stats = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("priced stats response")
        .json::<Value>()
        .await
        .expect("priced stats JSON");
    let expected = (3.0 * 1.0 + 8.0 * 2.0 + 7.0 * 3.0) / 1_000_000.0;
    let actual = stats["backends"]["priced-custom"]["cost_usd"]
        .as_f64()
        .expect("configured cost");
    assert!((actual - expected).abs() < 1e-12);
}

#[tokio::test]
async fn custom_chat_completions_uses_exact_endpoint_and_generic_429_classification() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::TOO_MANY_REQUESTS,
        json!({
            "type":"error",
            "error":{
                "type":"GoUsageLimitError",
                "message":"Subscription quota exceeded. You can continue using free models."
            },
            "metadata":{"limitName":"5 hour"}
        }),
    )])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let mut custom_primary = test_backend_adapter(
        "custom-primary",
        "custom-key",
        &primary,
        AdapterKind::CustomChatCompletions,
        Protocol::OpenAiChat,
    );
    custom_primary.endpoint = Some(format!("{}/custom/gateway", primary.endpoint()));
    let custom_fallback = test_backend_adapter(
        "custom-fallback",
        "fallback-key",
        &fallback,
        AdapterKind::CustomChatCompletions,
        Protocol::OpenAiChat,
    );
    let config = test_config(
        vec![custom_primary, custom_fallback],
        vec![
            ("custom", vec![target("custom-primary", "custom-key")]),
            ("fallback", vec![target("custom-fallback", "fallback-key")]),
        ],
    );
    let gateway = Gateway::start_config(config, 0x0c05_7429).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("custom endpoint fallback response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, "x-relay-backend"),
        "custom-fallback"
    );
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "provider_capacity"
    );
    let body = response
        .json::<Value>()
        .await
        .expect("custom fallback JSON");
    assert_eq!(body["choices"][0]["message"]["content"], "fallback answer");

    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
    assert_eq!(primary.request_paths().await, vec!["/custom/gateway"]);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("custom endpoint attempts response")
        .json::<Value>()
        .await
        .expect("custom endpoint attempts JSON");
    let primary_attempt = attempts["attempts"]
        .as_array()
        .expect("custom endpoint attempts array")
        .iter()
        .find(|attempt| attempt["backend"] == "custom-primary")
        .expect("custom primary attempt");
    assert_eq!(primary_attempt["error_class"], "provider_capacity");
    assert_ne!(primary_attempt["error_class"], "provider_quota");

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("custom endpoint status response")
        .json::<Value>()
        .await
        .expect("custom endpoint status JSON");
    let target = status_target(&status, "custom-primary");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "provider_capacity");
}

#[tokio::test]
async fn named_tool_choice_reaches_upstream_and_400_does_not_fallback_or_open_circuit() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"named tool choice is unsupported"}}),
        ),
        MockReply::json(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"named tool choice is unsupported"}}),
        ),
    ])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let config = Config {
        config_version: 3,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
            timeouts: Default::default(),
        },
        affinity: Default::default(),
        backends: vec![
            test_backend_adapter_model(
                "kimi-primary",
                "primary-key",
                &primary,
                AdapterKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_backend_adapter_model(
                "kimi-fallback",
                "fallback-key",
                &fallback,
                AdapterKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
        ],
        models: vec![ServedModelConfig {
            name: "kimi-k3".into(),
            aliases: Vec::new(),
            protocols: vec![Protocol::OpenAiChat],
            layers: vec![
                RouteLayerConfig {
                    name: "primary".into(),
                    strategy: RouteStrategy::Random,
                    targets: vec![target_model("kimi-primary", "primary-key", "kimi-k3")],
                },
                RouteLayerConfig {
                    name: "fallback".into(),
                    strategy: RouteStrategy::Random,
                    targets: vec![target_model("kimi-fallback", "fallback-key", "kimi-k3")],
                },
            ],
        }],
    };
    let gateway = Gateway::start_config(config, 0x400).await;
    let client = reqwest::Client::new();
    let request = json!({
        "model":"kimi-k3",
        "reasoning_effort":"high",
        "messages":[{"role":"user","content":"check the weather"}],
        "tools":[{"type":"function","function":{
            "name":"weather","parameters":{"type":"object"}
        }}],
        "tool_choice":{"type":"function","function":{"name":"weather"}}
    });

    for streaming in [false, true] {
        let mut request = request.clone();
        if streaming {
            request["stream"] = json!(true);
        }
        let response = client
            .post(gateway.url("/v1/chat/completions"))
            .json(&request)
            .send()
            .await
            .expect("named tool choice response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.json::<Value>().await.expect("client error JSON");
        assert_eq!(body["error"]["type"], "client_request");
        assert_eq!(body["error"]["message"], "upstream request failed");
    }

    assert_eq!(primary.calls().await, 2);
    assert_eq!(fallback.calls().await, 0);
    let upstream_requests = primary.request_bodies().await;
    assert_eq!(upstream_requests.len(), 2);
    assert_eq!(upstream_requests[0]["reasoning_effort"], "high");
    assert_eq!(
        upstream_requests[0]["tool_choice"]["function"]["name"],
        "weather"
    );
    assert_eq!(upstream_requests[1]["stream"], true);
    assert_eq!(
        upstream_requests[1]["stream_options"]["include_usage"],
        true
    );
}

#[tokio::test]
async fn claude_tool_history_without_thinking_reaches_upstream_without_fallback() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::BAD_REQUEST,
        json!({"error":{"message":"reasoning_content is required"}}),
    )])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let config = Config {
        config_version: 3,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
            timeouts: Default::default(),
        },
        affinity: Default::default(),
        backends: vec![
            test_backend_adapter_model(
                "kimi-primary",
                "primary-key",
                &primary,
                AdapterKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_backend_adapter_model(
                "kimi-fallback",
                "fallback-key",
                &fallback,
                AdapterKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
        ],
        models: vec![ServedModelConfig {
            name: "kimi-k3".into(),
            aliases: vec!["kimi-k3[1m]".into()],
            protocols: vec![Protocol::AnthropicMessages],
            layers: vec![
                RouteLayerConfig {
                    name: "primary".into(),
                    strategy: RouteStrategy::Random,
                    targets: vec![target_model("kimi-primary", "primary-key", "kimi-k3")],
                },
                RouteLayerConfig {
                    name: "fallback".into(),
                    strategy: RouteStrategy::Random,
                    targets: vec![target_model("kimi-fallback", "fallback-key", "kimi-k3")],
                },
            ],
        }],
    };
    let gateway = Gateway::start_config(config, 0xc1a0de).await;
    let client = reqwest::Client::new();
    let request = json!({
        "model":"kimi-k3[1m]",
        "max_tokens":16_000,
        "thinking":{"type":"enabled","budget_tokens":8_000},
        "messages":[
            {"role":"user","content":"check the weather"},
            {"role":"assistant","content":[{
                "type":"tool_use","id":"tool-1","name":"weather","input":{"city":"Shanghai"}
            }]},
            {"role":"user","content":[{
                "type":"tool_result","tool_use_id":"tool-1","content":"sunny"
            },{
                "type":"text","text":"continue after all tool results"
            }]}
        ],
        "tools":[{
            "name":"weather",
            "input_schema":{"type":"object","properties":{"city":{"type":"string"}}}
        }],
        "tool_choice":{"type":"tool","name":"weather"}
    });

    let response = client
        .post(gateway.url("/v1/messages"))
        .json(&request)
        .send()
        .await
        .expect("Claude tool history response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.json::<Value>().await.expect("client error JSON");
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(body["error"]["message"], "upstream request failed");

    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
    let upstream_requests = primary.request_bodies().await;
    assert_eq!(upstream_requests.len(), 1);
    let upstream = &upstream_requests[0];
    assert_eq!(upstream["model"], "kimi-k3");
    assert!(upstream.get("reasoning_effort").is_none());
    assert!(upstream.get("thinking").is_none());
    assert_eq!(upstream["tool_choice"]["function"]["name"], "weather");
    assert_eq!(upstream["messages"][1]["tool_calls"][0]["id"], "tool-1");
    assert!(upstream["messages"][1].get("reasoning_content").is_none());
    assert_eq!(upstream["messages"][2]["tool_call_id"], "tool-1");
    assert_eq!(upstream["messages"][3]["role"], "user");
    assert_eq!(
        upstream["messages"][3]["content"],
        "continue after all tool results"
    );
}

#[tokio::test]
async fn kimi_code_anthropic_ingress_uses_native_messages_protocol() {
    let upstream = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        json!({
            "id":"msg-native",
            "type":"message",
            "role":"assistant",
            "model":"k3",
            "content":[{"type":"text","text":"native ok"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":20,"output_tokens":3}
        }),
    )])
    .await;
    let mut config = test_config(
        vec![test_backend_adapter_model(
            "kimi-code",
            "allegretto",
            &upstream,
            AdapterKind::KimiCode,
            Protocol::AnthropicMessages,
            "k3",
        )],
        vec![(
            "native",
            vec![target_model("kimi-code", "allegretto", "k3")],
        )],
    );
    config.models[0].name = "kimi-k3".into();
    config.models[0].aliases = vec!["kimi-k3[1m]".into()];
    config.models[0].protocols = vec![Protocol::AnthropicMessages];
    let gateway = Gateway::start_config(config, 0xa117_0001).await;
    let client = reqwest::Client::new();
    let request = json!({
        "model":"kimi-k3[1m]",
        "max_tokens":16_000,
        "thinking":{"type":"adaptive"},
        "output_config":{"effort":"high"},
        "messages":[
            {"role":"assistant","content":[{
                "type":"tool_use","id":"Skill:3","name":"Skill","input":{"skill":"verify"}
            }]},
            {"role":"user","content":[{
                "type":"tool_result","tool_use_id":"Skill:3","content":"loaded"
            },{
                "type":"text","text":"continue after the skill result"
            }]}
        ]
    });

    let response = client
        .post(gateway.url("/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-relay-include-metadata", "1")
        .json(&request)
        .send()
        .await
        .expect("native Kimi Messages response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "kimi-code");
    assert_eq!(
        header_value(&response, "x-relay-egress-protocol"),
        "anthropic-messages"
    );
    assert_eq!(header_value(&response, "x-relay-translated"), "0");
    assert_eq!(
        response.json::<Value>().await.unwrap()["content"][0]["text"],
        "native ok"
    );

    let bodies = upstream.request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "k3");
    assert_eq!(bodies[0]["thinking"], request["thinking"]);
    assert_eq!(bodies[0]["output_config"], request["output_config"]);
    assert_eq!(bodies[0]["messages"], request["messages"]);
    assert_eq!(upstream.request_paths().await, vec!["/messages"]);
    let headers = upstream.request_headers().await;
    assert_eq!(headers[0]["x-api-key"], "test-key-allegretto");
    assert_eq!(headers[0]["anthropic-version"], "2023-06-01");
    assert!(headers[0].get("authorization").is_none());
}

#[tokio::test]
async fn native_anthropic_stream_persists_accumulated_usage() {
    let upstream_stream = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-native-stream\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"k3\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":20,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":12,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"native stream ok\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let upstream = MockProvider::start(vec![MockReply::sse(upstream_stream)]).await;
    let mut config = test_config(
        vec![test_backend_adapter_model(
            "kimi-code",
            "allegretto",
            &upstream,
            AdapterKind::KimiCode,
            Protocol::AnthropicMessages,
            "k3",
        )],
        vec![(
            "native",
            vec![target_model("kimi-code", "allegretto", "k3")],
        )],
    );
    config.models[0].name = "kimi-k3".into();
    config.models[0].protocols = vec![Protocol::AnthropicMessages];
    let gateway = Gateway::start_config(config, 0xa117_0002).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model":"kimi-k3",
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"hello"}]
        }))
        .send()
        .await
        .expect("native Anthropic stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("native Anthropic stream body");
    assert!(stream.contains("native stream ok"));
    assert!(stream.contains("event: message_stop"));

    let requests = client
        .get(gateway.url("/api/requests?limit=10"))
        .send()
        .await
        .expect("native Anthropic request records")
        .json::<Value>()
        .await
        .expect("native Anthropic request records JSON");
    let request = &requests["requests"][0];
    assert_eq!(request["streaming"], true);
    assert_eq!(request["translated"], false);
    assert_eq!(request["usage"]["input_tokens"], 35);
    assert_eq!(request["usage"]["cache_hit_tokens"], 12);
    assert_eq!(request["usage"]["cache_miss_tokens"], 23);
    assert_eq!(request["usage"]["output_tokens"], 7);
    assert_eq!(request["usage"]["total_tokens"], 42);
    assert_eq!(request["usage"]["provider_reported"], true);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=10"))
        .send()
        .await
        .expect("native Anthropic attempt records")
        .json::<Value>()
        .await
        .expect("native Anthropic attempt records JSON");
    assert_eq!(attempts["attempts"][0]["usage"], request["usage"]);

    let stats = client
        .get(gateway.url("/api/routing/stats?model=kimi-k3&window=all"))
        .send()
        .await
        .expect("native Anthropic routing stats")
        .json::<Value>()
        .await
        .expect("native Anthropic routing stats JSON");
    assert_eq!(stats["totals"]["calls"], 1);
    assert_eq!(stats["totals"]["input_tokens"], 35);
    assert_eq!(stats["totals"]["output_tokens"], 7);
    assert_eq!(stats["totals"]["total_tokens"], 42);
}

#[tokio::test]
async fn direct_validation_is_deferred_and_unknown_translation_extensions_are_ignored() {
    let primary = MockProvider::start(
        (0..3)
            .map(|_| {
                MockReply::json(
                    StatusCode::BAD_REQUEST,
                    json!({"error":{"message":"upstream semantic rejection"}}),
                )
            })
            .collect(),
    )
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let cases = [
        (
            "/v1/chat/completions",
            json!({
                "model":LOGICAL_MODEL,
                "messages":"not-an-array",
                "temperature":"not-a-number"
            }),
        ),
        (
            "/v1/responses",
            json!({
                "model":LOGICAL_MODEL,
                "input":[{"type":"future_item","payload":{"x":1}}],
                "reasoning":{"effort":"future_effort"},
                "tools":[{"type":"future_tool","name":"future"}],
                "tool_choice":{"type":"future_choice","name":"future"}
            }),
        ),
        (
            "/v1/messages",
            json!({
                "model":LOGICAL_MODEL,
                "max_tokens":128,
                "messages":[{"role":"user","content":[{
                    "type":"future_block","payload":{"x":1}
                }]}],
                "thinking":{"type":"future_thinking"},
                "output_config":{
                    "effort":"future_effort",
                    "format":{"type":"future_format"}
                },
                "tool_choice":{"type":"future_choice","name":"future"}
            }),
        ),
    ];

    for (path, body) in cases {
        let response = client
            .post(gateway.url(path))
            .json(&body)
            .send()
            .await
            .expect("semantic pass-through response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    assert_eq!(primary.calls().await, 3);
    assert_eq!(fallback.calls().await, 0);
    let upstream = primary.request_bodies().await;
    assert_eq!(upstream[0]["messages"], "not-an-array");
    assert!(upstream[1]["messages"].as_array().unwrap().is_empty());
    assert!(upstream[1].get("reasoning_effort").is_none());
    assert!(upstream[1].get("tools").is_none());
    assert!(upstream[2]["messages"].as_array().unwrap().is_empty());
    assert!(upstream[2].get("thinking").is_none());
    assert!(upstream[2].get("reasoning_effort").is_none());
    assert!(upstream[2].get("response_format").is_none());
}

#[tokio::test]
async fn requests_api_paginates_with_exclusive_cursors() {
    let primary = MockProvider::start(
        (0..5)
            .map(|index| {
                MockReply::json(
                    StatusCode::OK,
                    chat_completion("reasoning", &format!("answer-{index}")),
                )
            })
            .collect(),
    )
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    for _ in 0..5 {
        let response = client
            .post(gateway.url("/v1/chat/completions"))
            .json(&chat_request())
            .send()
            .await
            .expect("chat response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let first = client
        .get(gateway.url("/api/requests?limit=2"))
        .send()
        .await
        .expect("first request page")
        .json::<Value>()
        .await
        .expect("first request page JSON");
    assert_eq!(first["limit"], 2);
    let first_rows = first["requests"].as_array().expect("first request rows");
    assert_eq!(first_rows.len(), 2);
    let first_cursor = first["next_cursor"]
        .as_str()
        .expect("first page next cursor");
    assert_eq!(first_cursor, first_rows[1]["id"].as_str().unwrap());

    let second = client
        .get(gateway.url(&format!("/api/requests?limit=2&before={first_cursor}")))
        .send()
        .await
        .expect("second request page")
        .json::<Value>()
        .await
        .expect("second request page JSON");
    let second_rows = second["requests"].as_array().expect("second request rows");
    assert_eq!(second_rows.len(), 2);
    let second_cursor = second["next_cursor"]
        .as_str()
        .expect("second page next cursor");
    assert_eq!(second_cursor, second_rows[1]["id"].as_str().unwrap());

    let last = client
        .get(gateway.url(&format!("/api/requests?limit=2&before={second_cursor}")))
        .send()
        .await
        .expect("last request page")
        .json::<Value>()
        .await
        .expect("last request page JSON");
    let last_rows = last["requests"].as_array().expect("last request rows");
    assert_eq!(last_rows.len(), 1);
    assert!(last["next_cursor"].is_null());

    let mut ids = first_rows
        .iter()
        .chain(second_rows)
        .chain(last_rows)
        .map(|row| row["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5);
}

#[tokio::test]
async fn random_strategy_distributes_requests_within_one_layer() {
    const REQUESTS: usize = 200;
    let worker_a = MockProvider::start(
        (0..REQUESTS)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("a", "worker-a")))
            .collect(),
    )
    .await;
    let worker_b = MockProvider::start(
        (0..REQUESTS)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("b", "worker-b")))
            .collect(),
    )
    .await;
    let config = test_config(
        vec![
            test_backend("worker-a", "key-a", &worker_a),
            test_backend("worker-b", "key-b", &worker_b),
        ],
        vec![(
            "plan",
            vec![target("worker-a", "key-a"), target("worker-b", "key-b")],
        )],
    );
    let gateway = Gateway::start_config(config, 0x1234_5678).await;
    let client = reqwest::Client::new();

    for _ in 0..REQUESTS {
        let response = client
            .post(gateway.url("/v1/chat/completions"))
            .json(&chat_request())
            .send()
            .await
            .expect("random layer response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let count_a = worker_a.calls().await;
    let count_b = worker_b.calls().await;
    assert_eq!(count_a + count_b, REQUESTS);
    assert!((70..=130).contains(&count_a), "worker-a count={count_a}");
    assert!((70..=130).contains(&count_b), "worker-b count={count_b}");

    let attempts = client
        .get(gateway.url("/api/attempts?limit=500"))
        .send()
        .await
        .expect("random attempts response")
        .json::<Value>()
        .await
        .expect("random attempts JSON");
    let attempts = attempts["attempts"].as_array().expect("attempt rows");
    assert_eq!(attempts.len(), REQUESTS);
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt["route_layer"] == "plan")
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt["route_layer_index"] == 0)
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt["selection_reason"] == "random")
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["backend"] == "worker-a")
            .count(),
        count_a
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["backend"] == "worker-b")
            .count(),
        count_b
    );
    eprintln!(
        "RANDOM_LAYER_EVIDENCE {}",
        json!({"requests":REQUESTS,"worker_a":count_a,"worker_b":count_b,"persisted_attempts":attempts.len()})
    );
}

#[tokio::test]
async fn prompt_prefix_affinity_routes_a_divergent_branch_across_mixed_adapters() {
    let worker_a = MockProvider::start(
        (0..4)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("a", "worker-a")))
            .collect(),
    )
    .await;
    let worker_b = MockProvider::start(
        (0..4)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("b", "worker-b")))
            .collect(),
    )
    .await;
    let mut config = test_config(
        vec![
            test_backend("worker-a", "key-a", &worker_a),
            test_backend_adapter(
                "worker-b",
                "key-b",
                &worker_b,
                AdapterKind::DeepSeekOfficial,
                Protocol::OpenAiChat,
            ),
        ],
        vec![(
            "mixed-plan-payg",
            vec![target("worker-a", "key-a"), target("worker-b", "key-b")],
        )],
    );
    config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;
    config.affinity.checkpoint_bytes = 128;
    config.affinity.success_ttl_ms = 200;
    let gateway = Gateway::start_config(config, 0xa11f_1a17).await;
    let client = reqwest::Client::new();
    let common = "a".repeat(3_000);
    let long = format!("{common}{}", "b".repeat(4_000));
    let branch = format!("{common}{}", "c".repeat(3_000));

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request_with_content(&long))
        .send()
        .await
        .expect("cold affinity request");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(header_value(&first, "x-relay-selection-reason"), "random");
    let warm_provider = header_value(&first, "x-relay-backend");
    let _ = first.json::<Value>().await.expect("cold response JSON");

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request_with_content(&branch))
        .send()
        .await
        .expect("warm branch request");
    let second_request_id = header_value(&second, "x-relay-request-id");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-backend"), warm_provider);
    assert_eq!(
        header_value(&second, "x-relay-selection-reason"),
        "prompt-prefix-affinity"
    );
    let matched = header_value(&second, "x-relay-matched-prefix-bytes")
        .parse::<u64>()
        .expect("matched prefix byte count");
    assert!(
        ((common.len() - 128) as u64..=(common.len() + 128) as u64).contains(&matched),
        "matched={matched}"
    );
    let _ = second.json::<Value>().await.expect("warm response JSON");

    let unrelated = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request_with_content("completely unrelated prompt"))
        .send()
        .await
        .expect("unrelated affinity request");
    assert_eq!(unrelated.status(), StatusCode::OK);
    assert_eq!(
        header_value(&unrelated, "x-relay-selection-reason"),
        "random"
    );
    assert!(
        unrelated
            .headers()
            .get("x-relay-matched-prefix-bytes")
            .is_none()
    );
    let _ = unrelated.json::<Value>().await.expect("unrelated JSON");

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let expired = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request_with_content(&branch))
        .send()
        .await
        .expect("expired affinity request");
    assert_eq!(expired.status(), StatusCode::OK);
    assert_eq!(header_value(&expired, "x-relay-selection-reason"), "random");
    let _ = expired
        .json::<Value>()
        .await
        .expect("expired response JSON");

    assert_eq!(worker_a.calls().await + worker_b.calls().await, 4);
    let warm_calls = if warm_provider == "worker-a" {
        worker_a.calls().await
    } else {
        worker_b.calls().await
    };
    assert!(
        warm_calls >= 2,
        "warm_provider={warm_provider} calls={warm_calls}"
    );

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("affinity attempts response")
        .json::<Value>()
        .await
        .expect("affinity attempts JSON");
    let warm_attempt = attempts_for_request(&attempts, &second_request_id);
    assert_eq!(warm_attempt.len(), 1);
    assert_eq!(warm_attempt[0]["backend"], warm_provider);
    assert_eq!(
        warm_attempt[0]["selection_reason"],
        "prompt-prefix-affinity"
    );
    assert_eq!(warm_attempt[0]["matched_prefix_bytes"], matched);
    assert_eq!(warm_attempt[0]["usage"]["cache_hit_tokens"], 3);
    eprintln!(
        "MOCK_AFFINITY_EVIDENCE {}",
        json!({"warm_provider":warm_provider,"matched_prefix_bytes":matched,"warm_provider_calls":warm_calls,"recorded_cache_hit_tokens":3})
    );
}

#[tokio::test]
async fn single_target_affinity_layer_skips_hashing_and_bookkeeping() {
    let worker = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("single", "single-worker"),
    )])
    .await;
    let unused = MockProvider::start(vec![]).await;
    let mut config = test_config(
        vec![
            test_backend("single-worker", "single-key", &worker),
            test_backend("unused-worker", "unused-key", &unused),
        ],
        vec![("plan", vec![target("single-worker", "single-key")])],
    );
    config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;
    let gateway = Gateway::start_config(config, 17).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("single-target affinity response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, "x-relay-selection-reason"),
        "single-target"
    );
    assert!(
        response
            .headers()
            .get("x-relay-matched-prefix-bytes")
            .is_none()
    );
    let _ = response.json::<Value>().await.expect("single-target JSON");
    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("single-target status")
        .json::<Value>()
        .await
        .expect("single-target status JSON");
    assert_eq!(status["affinity"]["storage"], "memory-only");
    assert_eq!(status["affinity"]["leases"], 0);
    assert_eq!(status["models"][0], LOGICAL_MODEL);
    assert!(status.get("active_provider").is_none());
    assert!(status.get("circuit").is_none());
    assert!(status.get("deepseek_balance").is_none());
    let stats = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("single-target stats")
        .json::<Value>()
        .await
        .expect("single-target stats JSON");
    assert_eq!(stats["backends"]["single-worker"]["attempts"], 1);
    assert_eq!(stats["backends"]["unused-worker"]["attempts"], 0);
    eprintln!(
        "SINGLE_TARGET_EVIDENCE {}",
        json!({"selection_reason":"single-target","affinity_leases":0,"unused_provider_attempts":0})
    );
}

#[tokio::test]
async fn completed_stream_warms_prefix_affinity_for_the_next_request() {
    let worker_a = MockProvider::start(
        (0..2)
            .map(|_| MockReply::sse(chat_stream("a", "worker-a", true)))
            .collect(),
    )
    .await;
    let worker_b = MockProvider::start(
        (0..2)
            .map(|_| MockReply::sse(chat_stream("b", "worker-b", true)))
            .collect(),
    )
    .await;
    let mut config = test_config(
        vec![
            test_backend("worker-a", "key-a", &worker_a),
            test_backend("worker-b", "key-b", &worker_b),
        ],
        vec![(
            "plan",
            vec![target("worker-a", "key-a"), target("worker-b", "key-b")],
        )],
    );
    config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;
    let gateway = Gateway::start_config(config, 0x57ea_0a11).await;
    let client = reqwest::Client::new();
    let common = "stream-prefix".repeat(300);

    let mut first_body = chat_request_with_content(&format!("{common}-first"));
    first_body["stream"] = json!(true);
    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&first_body)
        .send()
        .await
        .expect("first streaming affinity response");
    assert_eq!(first.status(), StatusCode::OK);
    let warm_provider = header_value(&first, "x-relay-backend");
    assert_eq!(header_value(&first, "x-relay-selection-reason"), "random");
    let first_stream = first.text().await.expect("first stream body");
    assert!(first_stream.contains("data: [DONE]"));

    let mut second_body = chat_request_with_content(&format!("{common}-branch"));
    second_body["stream"] = json!(true);
    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&second_body)
        .send()
        .await
        .expect("second streaming affinity response");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-backend"), warm_provider);
    assert_eq!(
        header_value(&second, "x-relay-selection-reason"),
        "prompt-prefix-affinity"
    );
    assert!(
        header_value(&second, "x-relay-matched-prefix-bytes")
            .parse::<u64>()
            .expect("stream matched prefix bytes")
            > 3_000
    );
    let second_stream = second.text().await.expect("second stream body");
    assert!(second_stream.contains("data: [DONE]"));
    assert_eq!(worker_a.calls().await + worker_b.calls().await, 2);
    eprintln!(
        "STREAM_AFFINITY_EVIDENCE {}",
        json!({"warm_provider":warm_provider,"requests":2,"completed_done_events":2})
    );
}

#[tokio::test]
async fn exhausts_every_target_in_a_layer_before_later_layer_fallback() {
    let plan_a = MockProvider::start(vec![MockReply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":{"message":"plan-a unavailable"}}),
    )])
    .await;
    let plan_b = MockProvider::start(vec![MockReply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":{"message":"plan-b unavailable"}}),
    )])
    .await;
    let payg = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("payg reasoning", "payg answer"),
    )])
    .await;
    let config = test_config(
        vec![
            test_backend("plan-a", "key-a", &plan_a),
            test_backend("plan-b", "key-b", &plan_b),
            test_backend("payg", "key-payg", &payg),
        ],
        vec![
            (
                "plan",
                vec![target("plan-a", "key-a"), target("plan-b", "key-b")],
            ),
            ("payg", vec![target("payg", "key-payg")]),
        ],
    );
    let gateway = Gateway::start_config(config, 0xfeed_beef).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("layer fallback response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "payg");
    assert_eq!(header_value(&response, "x-relay-route-layer"), "payg");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(plan_a.calls().await, 1);
    assert_eq!(plan_b.calls().await, 1);
    assert_eq!(payg.calls().await, 1);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("layer attempts response")
        .json::<Value>()
        .await
        .expect("layer attempts JSON");
    let mut attempts = attempts_for_request(&attempts, &request_id);
    attempts.sort_by_key(|attempt| attempt["sequence"].as_u64().unwrap());
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[0]["route_layer_index"], 0);
    assert_eq!(attempts[1]["route_layer_index"], 0);
    assert_eq!(attempts[2]["route_layer_index"], 1);
    assert_eq!(attempts[0]["error_class"], "provider_transient");
    assert_eq!(attempts[1]["error_class"], "provider_transient");
    assert_eq!(attempts[2]["error_class"], Value::Null);
    assert_eq!(attempts[2]["selection_reason"], "single-target");
}

#[tokio::test]
async fn kimi_k3_two_layer_route_uses_exact_provider_model_ids_before_payg_fallback() {
    let go = MockProvider::start(vec![MockReply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":{"message":"go capacity exhausted"}}),
    )])
    .await;
    let code = MockProvider::start(vec![MockReply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":{"message":"code capacity exhausted"}}),
    )])
    .await;
    let official = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("official reasoning", "official answer"),
    )])
    .await;

    let mut config = test_config(
        vec![
            test_backend_adapter_model(
                "opencode-go-kimi",
                "go-plan",
                &go,
                AdapterKind::OpenCodeGo,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_backend_adapter_model(
                "kimi-code",
                "allegretto",
                &code,
                AdapterKind::KimiCode,
                Protocol::OpenAiChat,
                "k3",
            ),
            test_backend_adapter_model(
                "kimi-official",
                "official-payg",
                &official,
                AdapterKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
        ],
        vec![
            (
                "subscriptions",
                vec![
                    target_model("opencode-go-kimi", "go-plan", "kimi-k3"),
                    target_model("kimi-code", "allegretto", "k3"),
                ],
            ),
            (
                "payg",
                vec![target_model("kimi-official", "official-payg", "kimi-k3")],
            ),
        ],
    );
    config.models[0].name = "kimi-k3".into();
    config.models[0].aliases = vec!["kimi-k3-1m".into()];
    config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;

    let gateway = Gateway::start_config(config, 0x31_000_000).await;
    let client = reqwest::Client::new();
    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&json!({
            "model":"kimi-k3-1m",
            "reasoning_effort":"low",
            "messages":[{"role":"user","content":"Reply with OK."}]
        }))
        .send()
        .await
        .expect("Kimi layered response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "kimi-official");
    assert_eq!(header_value(&response, "x-relay-route-layer"), "payg");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&response, "x-relay-selection-reason"),
        "single-target"
    );
    let body = response.json::<Value>().await.expect("Kimi response JSON");
    assert_eq!(body["choices"][0]["message"]["content"], "official answer");

    for (provider, expected_model) in [(&go, "kimi-k3"), (&code, "k3"), (&official, "kimi-k3")] {
        let bodies = provider.request_bodies().await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["model"], expected_model);
        assert_eq!(bodies[0]["reasoning_effort"], "low");
    }

    let attempts = client
        .get(gateway.url("/api/attempts?limit=10"))
        .send()
        .await
        .expect("Kimi attempts response")
        .json::<Value>()
        .await
        .expect("Kimi attempts JSON");
    let attempts = attempts_for_request(&attempts, &request_id);
    assert_eq!(attempts.len(), 3);
    assert!(attempts.iter().all(|attempt| {
        attempt.get("backend").is_some()
            && attempt.get("adapter").is_some()
            && attempt.get("provider").is_none()
            && attempt.get("provider_kind").is_none()
    }));
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["route_layer"] == "subscriptions")
            .count(),
        2
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["route_layer"] == "payg")
            .count(),
        1
    );

    let stats = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("Kimi stats response")
        .json::<Value>()
        .await
        .expect("Kimi stats JSON");
    assert_eq!(
        stats["backends"]["opencode-go-kimi"]["models"][0],
        "kimi-k3"
    );
    assert_eq!(stats["backends"]["kimi-code"]["models"][0], "k3");
    assert_eq!(stats["backends"]["kimi-official"]["models"][0], "kimi-k3");
    let routes = stats["routes"].as_array().expect("Kimi stats routes");
    for (provider, upstream_model) in [
        ("opencode-go-kimi", "kimi-k3"),
        ("kimi-code", "k3"),
        ("kimi-official", "kimi-k3"),
    ] {
        let route = routes
            .iter()
            .find(|route| {
                route["backend"].as_str() == Some(provider)
                    && route["upstream_model"].as_str() == Some(upstream_model)
            })
            .unwrap_or_else(|| panic!("missing Kimi route {provider}/{upstream_model}"));
        assert_eq!(route["served_models"][0], "kimi-k3");
    }

    let requests = client
        .get(gateway.url("/api/requests?limit=10"))
        .send()
        .await
        .expect("Kimi requests response")
        .json::<Value>()
        .await
        .expect("Kimi requests JSON");
    let request = requests["requests"]
        .as_array()
        .expect("Kimi requests array")
        .iter()
        .find(|request| request["id"].as_str() == Some(&request_id))
        .expect("Kimi request record");
    assert_eq!(request["requested_model"], "kimi-k3-1m");
    assert_eq!(request["served_model"], "kimi-k3");
    assert_eq!(request["upstream_model"], "kimi-k3");
    assert_eq!(request["backend"], "kimi-official");
    assert_eq!(request["adapter"], "kimi-official");
    assert!(request.get("provider").is_none());
    assert!(request.get("provider_kind").is_none());
    eprintln!(
        "KIMI_LAYER_EVIDENCE {}",
        json!({"plan_attempts":2,"payg_attempts":1,"models":{"opencode-go":"kimi-k3","kimi-code":"k3","kimi-official":"kimi-k3"}})
    );
}

#[tokio::test]
async fn kimi_code_and_opencode_go_compete_by_prefix_with_isolated_model_namespaces() {
    let go = MockProvider::start(
        (0..2)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("go", "go")))
            .collect(),
    )
    .await;
    let code = MockProvider::start(
        (0..2)
            .map(|_| MockReply::json(StatusCode::OK, chat_completion("code", "code")))
            .collect(),
    )
    .await;
    let mut config = test_config(
        vec![
            test_backend_adapter_model(
                "opencode-go-kimi",
                "go-plan",
                &go,
                AdapterKind::OpenCodeGo,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_backend_adapter_model(
                "kimi-code",
                "allegretto",
                &code,
                AdapterKind::KimiCode,
                Protocol::OpenAiChat,
                "k3",
            ),
        ],
        vec![(
            "subscriptions",
            vec![
                target_model("opencode-go-kimi", "go-plan", "kimi-k3"),
                target_model("kimi-code", "allegretto", "k3"),
            ],
        )],
    );
    config.models[0].name = "kimi-k3".into();
    config.models[0].aliases = vec!["kimi-k3-1m".into()];
    config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;
    config.affinity.checkpoint_bytes = 128;

    let gateway = Gateway::start_config(config, 0x31_aff1).await;
    let client = reqwest::Client::new();
    let common = "Kimi K3 isolated affinity prefix. ".repeat(150);
    let request = |suffix: &str| {
        json!({
            "model":"kimi-k3",
            "messages":[{"role":"user","content":format!("{common}{suffix}")}]
        })
    };

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&request("cold branch"))
        .send()
        .await
        .expect("cold mixed Kimi response");
    assert_eq!(first.status(), StatusCode::OK);
    let warm_provider = header_value(&first, "x-relay-backend");
    assert_eq!(header_value(&first, "x-relay-selection-reason"), "random");
    let _ = first.json::<Value>().await.expect("cold mixed Kimi JSON");

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&request("divergent branch"))
        .send()
        .await
        .expect("warm mixed Kimi response");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-backend"), warm_provider);
    assert_eq!(
        header_value(&second, "x-relay-selection-reason"),
        "prompt-prefix-affinity"
    );
    assert!(
        header_value(&second, "x-relay-matched-prefix-bytes")
            .parse::<u64>()
            .expect("Kimi matched prefix bytes")
            > 4_000
    );
    let _ = second.json::<Value>().await.expect("warm mixed Kimi JSON");

    for body in go.request_bodies().await {
        assert_eq!(body["model"], "kimi-k3");
    }
    for body in code.request_bodies().await {
        assert_eq!(body["model"], "k3");
    }
    eprintln!(
        "KIMI_AFFINITY_EVIDENCE {}",
        json!({"warm_provider":warm_provider,"requests":2,"namespaces":["opencode-go/kimi-k3","kimi-code/k3"]})
    );
}

#[tokio::test]
async fn chat_client_uses_responses_only_provider_with_bidirectional_translation() {
    let upstream = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        json!({
            "id":"resp-upstream",
            "object":"response",
            "model":UPSTREAM_MODEL,
            "status":"completed",
            "output":[
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"provider thought"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"provider answer"}]}
            ],
            "usage":{
                "input_tokens":21,
                "input_tokens_details":{"cached_tokens":13},
                "output_tokens":8,
                "output_tokens_details":{"reasoning_tokens":3},
                "total_tokens":29
            }
        }),
    )])
    .await;
    let config = test_config(
        vec![test_backend_protocol(
            "responses-worker",
            "responses-key",
            &upstream,
            Protocol::OpenAiResponses,
        )],
        vec![("plan", vec![target("responses-worker", "responses-key")])],
    );
    let gateway = Gateway::start_config(config, 7).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("responses-provider response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, "x-relay-egress-protocol"),
        "openai-responses"
    );
    assert_eq!(header_value(&response, "x-relay-translated"), "1");
    let body = response
        .json::<Value>()
        .await
        .expect("translated chat JSON");
    assert_eq!(body["model"], LOGICAL_MODEL);
    assert_eq!(
        body["choices"][0]["message"]["reasoning_content"],
        "provider thought"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "provider answer");
    assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 13);
    let upstream_bodies = upstream.request_bodies().await;
    assert_eq!(upstream_bodies.len(), 1);
    assert!(upstream_bodies[0].get("input").is_some());
    assert!(upstream_bodies[0].get("messages").is_none());
}

#[tokio::test]
async fn responses_client_uses_anthropic_only_provider_with_bidirectional_translation() {
    let upstream = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        json!({
            "id":"msg-upstream",
            "type":"message",
            "role":"assistant",
            "model":UPSTREAM_MODEL,
            "content":[
                {"type":"thinking","thinking":"anthropic thought","signature":"sig"},
                {"type":"text","text":"anthropic answer"}
            ],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":17,"output_tokens":6,"cache_read_input_tokens":9}
        }),
    )])
    .await;
    let config = test_config(
        vec![test_backend_protocol(
            "anthropic-worker",
            "anthropic-key",
            &upstream,
            Protocol::AnthropicMessages,
        )],
        vec![("plan", vec![target("anthropic-worker", "anthropic-key")])],
    );
    let gateway = Gateway::start_config(config, 8).await;
    let client = reqwest::Client::new();
    let body = json!({"model":LOGICAL_MODEL,"input":"hello"});

    let response = client
        .post(gateway.url("/v1/responses"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("anthropic-provider response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, "x-relay-egress-protocol"),
        "anthropic-messages"
    );
    let body = response
        .json::<Value>()
        .await
        .expect("translated Responses JSON");
    assert_eq!(body["model"], LOGICAL_MODEL);
    assert_eq!(body["output"][0]["type"], "reasoning");
    assert_eq!(body["output"][1]["type"], "message");
    assert_eq!(body["output"][1]["content"][0]["text"], "anthropic answer");
    let upstream_bodies = upstream.request_bodies().await;
    assert_eq!(upstream_bodies[0]["messages"][0]["role"], "user");
    assert!(upstream_bodies[0].get("max_tokens").is_some());
    assert!(upstream_bodies[0].get("input").is_none());
}

#[tokio::test]
async fn responses_provider_stream_is_translated_to_chat_stream() {
    let upstream_stream = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-stream\",\"model\":\"deepseek-v4-flash\"}}\n\n",
        "event: response.reasoning_text.delta\ndata: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"stream thought\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"stream answer\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-stream\",\"model\":\"deepseek-v4-flash\",\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":7},\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":2},\"total_tokens\":17}}}\n\n"
    );
    let upstream = MockProvider::start(vec![MockReply::sse(upstream_stream)]).await;
    let config = test_config(
        vec![test_backend_protocol(
            "responses-worker",
            "responses-key",
            &upstream,
            Protocol::OpenAiResponses,
        )],
        vec![("plan", vec![target("responses-worker", "responses-key")])],
    );
    let gateway = Gateway::start_config(config, 9).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("translated Responses stream");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("translated stream body");
    assert!(stream.contains("stream thought"));
    assert!(stream.contains("stream answer"));
    assert!(stream.contains(&format!("\"model\":\"{LOGICAL_MODEL}\"")));
    assert!(stream.contains("\"cached_tokens\":7"));
    assert!(stream.contains("data: [DONE]"));
    assert_eq!(upstream.calls().await, 1);
    assert_eq!(upstream.request_bodies().await[0]["stream"], true);
}

#[tokio::test]
async fn anthropic_provider_stream_is_translated_to_responses_stream() {
    let upstream_stream = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-stream\",\"model\":\"deepseek-v4-flash\",\"content\":[],\"usage\":{\"input_tokens\":14,\"cache_read_input_tokens\":9}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"anthropic stream thought\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"anthropic stream answer\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let upstream = MockProvider::start(vec![MockReply::sse(upstream_stream)]).await;
    let config = test_config(
        vec![test_backend_protocol(
            "anthropic-worker",
            "anthropic-key",
            &upstream,
            Protocol::AnthropicMessages,
        )],
        vec![("plan", vec![target("anthropic-worker", "anthropic-key")])],
    );
    let gateway = Gateway::start_config(config, 10).await;
    let client = reqwest::Client::new();
    let body = json!({"model":LOGICAL_MODEL,"input":"hello","stream":true});

    let response = client
        .post(gateway.url("/v1/responses"))
        .json(&body)
        .send()
        .await
        .expect("translated Anthropic stream");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("translated Responses stream");
    assert!(stream.contains("event: response.created"));
    assert!(stream.contains(&format!("\"model\":\"{LOGICAL_MODEL}\"")));
    assert!(stream.contains("event: response.reasoning_text.delta"));
    assert!(stream.contains("anthropic stream thought"));
    assert!(stream.contains("event: response.output_text.delta"));
    assert!(stream.contains("anthropic stream answer"));
    assert!(stream.contains("event: response.completed"));
    assert!(
        stream.contains("\"cached_tokens\":9"),
        "translated stream missing cache usage: {stream}"
    );
    let upstream_bodies = upstream.request_bodies().await;
    assert_eq!(upstream_bodies[0]["stream"], true);
    assert_eq!(upstream_bodies[0]["messages"][0]["role"], "user");
    assert!(upstream_bodies[0].get("input").is_none());
}

#[tokio::test]
async fn transient_primary_failure_falls_back_before_commit_and_persists_attempts() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"primary unavailable"}}),
        )
        .with_header("retry-after", "0"),
    ])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("fallback response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "provider_transient"
    );
    let body = response.json::<Value>().await.expect("fallback JSON");
    assert_eq!(body["choices"][0]["message"]["content"], "fallback answer");
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);

    let attempts_response = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("attempts response");
    let attempts_body = attempts_response
        .json::<Value>()
        .await
        .expect("attempts JSON");
    let attempts = attempts_for_request(&attempts_body, &request_id);
    assert_eq!(attempts.len(), 2);
    let primary_attempt = attempts
        .iter()
        .find(|attempt| attempt["sequence"] == 1)
        .expect("primary attempt");
    let fallback_attempt = attempts
        .iter()
        .find(|attempt| attempt["sequence"] == 2)
        .expect("fallback attempt");
    assert_eq!(primary_attempt["committed"], false);
    assert_eq!(primary_attempt["error_class"], "provider_transient");
    assert_eq!(fallback_attempt["sequence"], 2);
    assert_eq!(fallback_attempt["committed"], true);
    assert_eq!(fallback_attempt["error_class"], Value::Null);

    let routing_stats = client
        .get(gateway.url(&format!(
            "/api/routing/stats?model={LOGICAL_MODEL}&window=all"
        )))
        .send()
        .await
        .expect("fallback routing statistics")
        .json::<Value>()
        .await
        .expect("fallback routing statistics JSON");
    assert_eq!(routing_stats["totals"]["calls"], 1);
    assert_eq!(routing_stats["layers"][0]["totals"]["calls"], 0);
    assert_eq!(routing_stats["layers"][1]["totals"]["calls"], 1);
    assert_eq!(
        routing_stats["layers"][1]["targets"][0]["credential"],
        "deepseek-payg"
    );
    assert_eq!(
        routing_stats["layers"][1]["targets"][0]["totals"]["calls"],
        1
    );
    let overview = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("fallback overview statistics")
        .json::<Value>()
        .await
        .expect("fallback overview statistics JSON");
    assert_eq!(overview["backends"]["opencode-go"]["attempts"], 1);
    assert_eq!(overview["backends"]["deepseek"]["attempts"], 1);

    let recent = client
        .get(gateway.url("/api/requests?limit=1"))
        .send()
        .await
        .expect("fallback request record")
        .json::<Value>()
        .await
        .expect("fallback request record JSON");
    assert_eq!(recent["requests"][0]["fallback"], true);
    assert_eq!(recent["requests"][0]["fallback_exhausted"], false);
}

#[tokio::test]
async fn both_providers_failing_returns_terminal_error() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"primary unavailable"}}),
        )
        .with_header("retry-after", "0"),
    ])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":{"message":"fallback unavailable"}}),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("terminal response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    let body = response.json::<Value>().await.expect("terminal JSON");
    assert_eq!(body["error"]["type"], "fallback_unavailable");
    assert_eq!(body["error"]["message"], "upstream request failed");
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn all_skipped_targets_record_fallback_exhausted_without_false_selection() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"primary unavailable"}}),
        )
        .with_header("retry-after", "60"),
    ])
    .await;
    let fallback = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"fallback unavailable"}}),
        )
        .with_header("retry-after", "60"),
    ])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .expect("open both circuits");
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = first.json::<Value>().await.expect("first terminal error");

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .expect("all targets skipped");
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = second
        .json::<Value>()
        .await
        .expect("skipped terminal error");
    assert_eq!(body["error"]["type"], "fallback_unavailable");
    assert_eq!(
        body["error"]["message"],
        "no route target is currently available"
    );
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);

    let recent = client
        .get(gateway.url("/api/requests?limit=2"))
        .send()
        .await
        .expect("request records")
        .json::<Value>()
        .await
        .expect("request records JSON");
    let requests = recent["requests"].as_array().expect("request array");
    assert_eq!(requests.len(), 2);

    let skipped = &requests[0];
    assert_eq!(skipped["error_class"], "fallback_unavailable");
    assert_eq!(skipped["fallback"], Value::Null);
    assert_eq!(skipped["fallback_exhausted"], true);
    assert_eq!(skipped["backend"], Value::Null);
    assert_eq!(skipped["route_layer"], Value::Null);
    assert_eq!(skipped["selection_reason"], Value::Null);

    let attempted_and_exhausted = &requests[1];
    assert_eq!(
        attempted_and_exhausted["error_class"],
        "fallback_unavailable"
    );
    assert_eq!(attempted_and_exhausted["fallback"], true);
    assert_eq!(attempted_and_exhausted["fallback_exhausted"], true);
    assert_eq!(attempted_and_exhausted["backend"], "deepseek");

    let attempts = client
        .get(gateway.url("/api/attempts?limit=10"))
        .send()
        .await
        .expect("attempt records")
        .json::<Value>()
        .await
        .expect("attempt records JSON");
    assert_eq!(attempts["attempts"].as_array().unwrap().len(), 2);

    let overview = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("overview statistics")
        .json::<Value>()
        .await
        .expect("overview statistics JSON");
    assert_eq!(overview["requests"]["total"], 2);
    assert_eq!(overview["requests"]["errors"], 2);
    assert_eq!(overview["requests"]["fallbacks"], 1);
}

#[tokio::test]
async fn http_200_error_envelope_does_not_close_or_commit_the_primary_route() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        json!({
            "type":"error",
            "error":{
                "type":"GoUsageLimitError",
                "message":"Subscription quota exceeded"
            }
        }),
    )])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("semantic fallback response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "provider_quota"
    );
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn first_stream_error_uses_provider_classification_before_fallback() {
    let primary = MockProvider::start(vec![MockReply::sse(concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"GoUsageLimitError\",\"message\":\"Subscription quota exceeded\"}}\n\n"
    ))])
    .await;
    let fallback = MockProvider::start(vec![MockReply::sse(chat_stream(
        "fallback thinking",
        "fallback answer",
        true,
    ))])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("classified stream fallback response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "provider_quota"
    );
    let stream = response.text().await.expect("classified fallback stream");
    assert!(stream.contains("fallback answer"));
    assert!(!stream.contains("GoUsageLimitError"));

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("classified stream status")
        .json::<Value>()
        .await
        .expect("classified stream status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["reason"], "provider_quota");
}

#[tokio::test]
async fn imagined_selector_fields_and_headers_do_not_select_the_route() {
    let primary = MockProvider::start(vec![
        MockReply::json(StatusCode::OK, chat_completion("one", "primary")),
        MockReply::json(StatusCode::OK, chat_completion("two", "primary")),
        MockReply::json(StatusCode::OK, chat_completion("three", "primary")),
        MockReply::json(StatusCode::OK, chat_completion("four", "primary")),
        MockReply::json(StatusCode::OK, chat_completion("five", "primary")),
        MockReply::json(StatusCode::OK, chat_completion("six", "primary")),
    ])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let mut field_body = chat_request();
    field_body["provider"] = json!("fallback");
    field_body["credential"] = json!("fallback-key");
    field_body["route_layer"] = json!("fallback-layer");
    let field_response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&field_body)
        .send()
        .await
        .expect("provider field response");
    assert_eq!(field_response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&field_response, "x-relay-backend"),
        "opencode-go"
    );

    for (name, value) in [
        ("x-relay-backend", "fallback"),
        ("x-relay-provider", "fallback"),
        ("x-provider", "fallback"),
        ("x-relay-credential", "fallback-key"),
        ("x-relay-route-layer", "fallback-layer"),
    ] {
        let response = client
            .post(gateway.url("/v1/chat/completions"))
            .header("x-relay-include-metadata", "1")
            .header(name, value)
            .json(&chat_request())
            .send()
            .await
            .expect("ignored provider header response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_value(&response, "x-relay-backend"), "opencode-go");
    }
    assert_eq!(primary.calls().await, 6);
    assert_eq!(fallback.calls().await, 0);
    let upstream_bodies = primary.request_bodies().await;
    assert_eq!(upstream_bodies[0]["provider"], "fallback");
    assert_eq!(upstream_bodies[0]["credential"], "fallback-key");
    assert_eq!(upstream_bodies[0]["route_layer"], "fallback-layer");
}

#[tokio::test]
async fn streaming_chat_passes_reasoning_content_usage_and_done() {
    let upstream = concat!(
        "data: {\"id\":\"chatcmpl-stream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"think\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-stream\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":3,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n"
    );
    let primary = MockProvider::start(vec![MockReply::sse(upstream)]).await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let mut body = chat_request();
    body["stream"] = json!(true);
    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let stream = response.text().await.expect("stream body");
    assert!(stream.contains("reasoning_content"));
    assert!(stream.contains("think"));
    assert!(stream.contains("\"content\":\"answer\""));
    assert!(stream.contains("\"prompt_tokens\":4"));
    assert!(stream.contains("data: [DONE]"));
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
}

#[tokio::test]
async fn streaming_pause_uses_stream_timeout_and_emits_downstream_heartbeats() {
    let primary = MockProvider::start(vec![MockReply::paused_sse(
        chat_stream("long thinking", "partial answer", false),
        "data: [DONE]\n\n",
        Duration::from_millis(90),
    )])
    .await;
    let mut config = test_config(
        vec![test_backend("opencode-go", "go-plan", &primary)],
        vec![("plan", vec![target("opencode-go", "go-plan")])],
    );
    config.server.timeouts.upstream_connect_ms = 100;
    config.server.timeouts.upstream_read_ms = 20;
    config.server.timeouts.upstream_stream_read_ms = 250;
    config.server.timeouts.upstream_total_ms = 1_000;
    config.server.timeouts.downstream_sse_heartbeat_ms = 20;
    let gateway = Gateway::start_config(config, 0x5eed).await;
    let client = reqwest::Client::new();

    let mut body = chat_request();
    body["stream"] = json!(true);
    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("paused stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("paused stream body");
    assert!(stream.contains("partial answer"));
    assert!(stream.contains("data: [DONE]"));
    assert!(
        stream.matches(": keep-alive\n\n").count() >= 2,
        "expected periodic downstream heartbeats: {stream}"
    );

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("timeout status response")
        .json::<Value>()
        .await
        .expect("timeout status JSON");
    assert_eq!(status["timeouts"]["upstream_read_ms"], 20);
    assert_eq!(status["timeouts"]["upstream_stream_read_ms"], 250);
    assert_eq!(status["timeouts"]["downstream_sse_heartbeat_ms"], 20);
}

#[tokio::test]
async fn streaming_read_timeout_records_precise_sanitized_error_after_commit() {
    let primary = MockProvider::start(vec![MockReply::hanging_sse(chat_stream(
        "stalled thinking",
        "partial answer",
        false,
    ))])
    .await;
    let mut config = test_config(
        vec![test_backend("opencode-go", "go-plan", &primary)],
        vec![("plan", vec![target("opencode-go", "go-plan")])],
    );
    config.server.timeouts.upstream_connect_ms = 100;
    config.server.timeouts.upstream_read_ms = 70;
    config.server.timeouts.upstream_stream_read_ms = 70;
    config.server.timeouts.upstream_total_ms = 1_000;
    config.server.timeouts.downstream_sse_heartbeat_ms = 15;
    let gateway = Gateway::start_config(config, 0x5eed).await;
    let client = reqwest::Client::new();

    let mut body = chat_request();
    body["stream"] = json!(true);
    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("stalled stream response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("stalled stream body");
    assert!(stream.contains("partial answer"));
    assert!(stream.contains(": keep-alive\n\n"));
    assert!(!stream.contains("data: [DONE]"));

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("stalled stream attempts response")
        .json::<Value>()
        .await
        .expect("stalled stream attempts JSON");
    let request_attempts = attempts_for_request(&attempts, &request_id);
    assert_eq!(request_attempts.len(), 1);
    assert_eq!(request_attempts[0]["error_class"], "stream_failure");
    assert_eq!(
        request_attempts[0]["sanitized_error"],
        "upstream SSE read timed out"
    );
}

#[tokio::test]
async fn empty_primary_stream_falls_back_before_first_semantic_event() {
    let primary = MockProvider::start(vec![MockReply::sse(String::new())]).await;
    let fallback = MockProvider::start(vec![MockReply::sse(chat_stream(
        "fallback thinking",
        "fallback answer",
        true,
    ))])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("fallback stream response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "stream_failure"
    );
    let stream = response.text().await.expect("fallback stream body");
    assert!(stream.contains("fallback thinking"));
    assert!(stream.contains("fallback answer"));
    assert!(stream.contains("data: [DONE]"));
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn malformed_first_sse_event_falls_back_before_response_commitment() {
    let primary = MockProvider::start(vec![MockReply::sse("data: not-json\n\n")]).await;
    let fallback = MockProvider::start(vec![MockReply::sse(chat_stream(
        "fallback thinking",
        "fallback after malformed SSE",
        true,
    ))])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("malformed SSE fallback response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "stream_failure"
    );
    let stream = response.text().await.expect("fallback stream body");
    assert!(stream.contains("fallback after malformed SSE"));
    assert!(!stream.contains("not-json"));
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn semantic_stream_eof_records_failure_without_fallback_and_opens_circuit() {
    let primary = MockProvider::start(vec![MockReply::sse(chat_stream(
        "",
        "partial answer",
        false,
    ))])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("partial stream response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "opencode-go");
    assert_eq!(header_value(&response, "x-relay-fallback"), "0");
    let stream = response.text().await.expect("partial stream body");
    assert!(stream.contains("partial answer"));
    assert!(!stream.contains("data: [DONE]"));
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("partial stream attempts response")
        .json::<Value>()
        .await
        .expect("partial stream attempts JSON");
    let request_attempts = attempts_for_request(&attempts, &request_id);
    assert_eq!(request_attempts.len(), 1);
    assert_eq!(request_attempts[0]["error_class"], "stream_failure");

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("partial stream status response")
        .json::<Value>()
        .await
        .expect("partial stream status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "stream_failure");
}

#[tokio::test]
async fn native_semantic_stream_error_after_commit_opens_circuit() {
    let upstream = concat!(
        "data: {\"id\":\"chatcmpl-stream\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "event: error\n",
        "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n",
        "data: [DONE]\n\n"
    );
    let primary = MockProvider::start(vec![MockReply::sse(upstream)]).await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("semantic error stream response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = response.text().await.expect("semantic error stream body");
    assert!(stream.contains("partial"));
    assert!(stream.contains("event: error"));
    assert!(!stream.contains("data: [DONE]"));
    assert_eq!(fallback.calls().await, 0);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("semantic error attempts response")
        .json::<Value>()
        .await
        .expect("semantic error attempts JSON");
    let request_attempts = attempts_for_request(&attempts, &request_id);
    assert_eq!(request_attempts.len(), 1);
    assert_eq!(request_attempts[0]["error_class"], "stream_failure");

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("semantic error status response")
        .json::<Value>()
        .await
        .expect("semantic error status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "stream_failure");
}

#[tokio::test]
async fn provider_quota_stream_error_after_commit_keeps_response_and_classifies_circuit() {
    let upstream = concat!(
        "data: {\"id\":\"chatcmpl-stream\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"GoUsageLimitError\",\"message\":\"Subscription quota exceeded\"}}\n\n",
        "data: [DONE]\n\n"
    );
    let primary = MockProvider::start(vec![MockReply::sse(upstream)]).await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let mut body = chat_request();
    body["stream"] = json!(true);

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&body)
        .send()
        .await
        .expect("committed quota stream response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "opencode-go");
    let stream = response.text().await.expect("committed quota stream body");
    assert!(stream.contains("partial"));
    assert!(stream.contains("GoUsageLimitError"));
    assert!(!stream.contains("data: [DONE]"));
    assert_eq!(fallback.calls().await, 0);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("committed quota attempts response")
        .json::<Value>()
        .await
        .expect("committed quota attempts JSON");
    let request_attempts = attempts_for_request(&attempts, &request_id);
    assert_eq!(request_attempts.len(), 1);
    assert_eq!(request_attempts[0]["error_class"], "provider_quota");

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("committed quota status response")
        .json::<Value>()
        .await
        .expect("committed quota status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "provider_quota");
}

#[tokio::test]
async fn responses_nonstream_contains_translated_reasoning_output() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("responses reasoning", "responses answer"),
    )])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/responses"))
        .json(&json!({
            "model": LOGICAL_MODEL,
            "instructions": "be concise",
            "input": "hello",
            "reasoning": {"effort":"high"}
        }))
        .send()
        .await
        .expect("Responses response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<Value>().await.expect("Responses JSON");
    assert_eq!(body["model"], LOGICAL_MODEL);
    assert_eq!(body["instructions"], "be concise");
    assert!(body.get("store").is_none());
    assert_eq!(body["output"][0]["type"], "reasoning");
    assert_eq!(
        body["output"][0]["content"][0]["text"],
        "responses reasoning"
    );
    assert_eq!(body["output"][1]["type"], "message");
    assert_eq!(body["output"][1]["content"][0]["text"], "responses answer");
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
}

#[tokio::test]
async fn anthropic_translation_does_not_fabricate_thinking_signatures() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("anthropic thinking", "anthropic text"),
    )])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/messages"))
        .json(&json!({
            "model": LOGICAL_MODEL,
            "max_tokens": 128,
            "thinking": {"type":"enabled"},
            "messages": [{"role":"user","content":"hello"}]
        }))
        .send()
        .await
        .expect("Anthropic response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<Value>().await.expect("Anthropic JSON");
    assert_eq!(body["model"], LOGICAL_MODEL);
    assert_eq!(body["content"].as_array().unwrap().len(), 1);
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "anthropic text");
    assert!(!body.to_string().contains("signature"));
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
}

#[tokio::test]
async fn tool_call_history_is_preserved_and_upstream_validates_missing_reasoning() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::OK,
            chat_completion("tool response reasoning", "tool response"),
        ),
        MockReply::json(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"reasoning_content is required"}}),
        ),
    ])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();
    let complete_reasoning = "complete reasoning content must survive the gateway exactly";
    let complete_body = json!({
        "model": LOGICAL_MODEL,
        "messages": [
            {"role":"user","content":"look this up"},
            {
                "role":"assistant",
                "content": null,
                "reasoning_content": complete_reasoning,
                "tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]
            },
            {"role":"tool","tool_call_id":"call-1","content":"result"}
        ]
    });
    let complete_response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&complete_body)
        .send()
        .await
        .expect("tool history response");
    assert_eq!(complete_response.status(), StatusCode::OK);
    let sent_bodies = primary.request_bodies().await;
    assert_eq!(sent_bodies.len(), 1);
    assert_eq!(
        sent_bodies[0]["messages"][1]["reasoning_content"],
        complete_reasoning
    );
    assert_eq!(
        sent_bodies[0]["messages"][1]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"x\"}"
    );

    let missing_body = json!({
        "model": LOGICAL_MODEL,
        "messages": [
            {"role":"user","content":"look this up"},
            {
                "role":"assistant",
                "content": null,
                "tool_calls":[{"id":"call-2","type":"function","function":{"name":"lookup","arguments":"{}"}}]
            },
            {"role":"tool","tool_call_id":"call-2","content":"result"}
        ]
    });
    let missing_response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&missing_body)
        .send()
        .await
        .expect("missing reasoning response");
    assert_eq!(missing_response.status(), StatusCode::BAD_REQUEST);
    let missing_error = missing_response
        .json::<Value>()
        .await
        .expect("missing reasoning JSON");
    assert_eq!(missing_error["error"]["type"], "client_request");
    assert_eq!(missing_error["error"]["message"], "upstream request failed");
    assert_eq!(primary.calls().await, 2);
    assert_eq!(fallback.calls().await, 0);
    let sent_bodies = primary.request_bodies().await;
    assert_eq!(sent_bodies.len(), 2);
    assert_eq!(
        sent_bodies[1]["messages"][1]["tool_calls"][0]["id"],
        "call-2"
    );
    assert!(
        sent_bodies[1]["messages"][1]
            .get("reasoning_content")
            .is_none()
    );
}

#[tokio::test]
async fn circuit_recovers_primary_after_persisted_transient_failure() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"probe me later"}}),
        )
        .with_header("retry-after", "0"),
        MockReply::json(
            StatusCode::OK,
            chat_completion("recovered reasoning", "recovered answer"),
        ),
    ])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("first recovery request");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(header_value(&first, "x-relay-backend"), "deepseek");
    let _ = first.json::<Value>().await.expect("first recovery JSON");

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("recovery probe request");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-backend"), "opencode-go");
    assert_eq!(header_value(&second, "x-relay-fallback"), "0");
    let second_body = second.json::<Value>().await.expect("second recovery JSON");
    assert_eq!(
        second_body["choices"][0]["message"]["content"],
        "recovered answer"
    );
    assert_eq!(primary.calls().await, 2);
    assert_eq!(fallback.calls().await, 1);

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("status response")
        .json::<Value>()
        .await
        .expect("status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "closed");
    assert_eq!(target["circuit"]["consecutive_failures"], 0);

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("recovery attempts response")
        .json::<Value>()
        .await
        .expect("recovery attempts JSON");
    assert_eq!(attempts["attempts"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn abandoned_half_open_stream_probe_is_released_before_primary_recovers() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"trip primary"}}),
        )
        .with_header("retry-after", "0"),
        MockReply::hanging_sse(chat_stream("probe reasoning", "probe chunk", false)),
        MockReply::sse(chat_stream("recovered reasoning", "recovered answer", true)),
    ])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("initial transient response");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(header_value(&first, "x-relay-backend"), "deepseek");
    assert_eq!(header_value(&first, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&first, "x-relay-fallback-reason"),
        "provider_transient"
    );
    let _ = first.json::<Value>().await.expect("initial fallback JSON");

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("initial circuit status response")
        .json::<Value>()
        .await
        .expect("initial circuit status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "provider_transient");

    let mut abandoned_probe = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&{
            let mut body = chat_request();
            body["stream"] = json!(true);
            body
        })
        .send()
        .await
        .expect("half-open streaming probe response");
    assert_eq!(abandoned_probe.status(), StatusCode::OK);
    assert_eq!(
        header_value(&abandoned_probe, "x-relay-backend"),
        "opencode-go"
    );
    assert_eq!(header_value(&abandoned_probe, "x-relay-fallback"), "0");
    let chunk = abandoned_probe
        .chunk()
        .await
        .expect("half-open probe first chunk")
        .expect("half-open probe chunk present");
    assert!(String::from_utf8_lossy(&chunk).contains("probe chunk"));
    drop(abandoned_probe);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = client
                .get(gateway.url("/api/status"))
                .send()
                .await
                .expect("abandoned probe status response")
                .json::<Value>()
                .await
                .expect("abandoned probe status JSON");
            let target = status_target(&status, "opencode-go");
            if target["circuit"]["mode"] == "open" && target["circuit"]["probe_in_flight"] == false
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("abandoned probe released");
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let mut recovered_body = chat_request();
    recovered_body["stream"] = json!(true);
    let recovered = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&recovered_body)
        .send()
        .await
        .expect("recovered primary stream response");
    assert_eq!(recovered.status(), StatusCode::OK);
    assert_eq!(header_value(&recovered, "x-relay-backend"), "opencode-go");
    assert_eq!(header_value(&recovered, "x-relay-fallback"), "0");
    let stream = recovered.text().await.expect("recovered primary stream");
    assert!(stream.contains("recovered answer"));
    assert!(stream.contains("data: [DONE]"));
    assert_eq!(primary.calls().await, 3);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn concurrent_delayed_primary_429s_count_one_circuit_failure() {
    let primary = MockProvider::start(
        (0..8)
            .map(|_| {
                MockReply::json(
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({"error":{"message":"primary capacity reached"}}),
                )
                .delayed(Duration::from_millis(500))
            })
            .collect(),
    )
    .await;
    let fallback = MockProvider::start(
        (0..8)
            .map(|index| {
                MockReply::json(
                    StatusCode::OK,
                    chat_completion("fallback reasoning", &format!("fallback answer {index}")),
                )
            })
            .collect(),
    )
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let requests = (0..8)
        .map(|_| {
            let client = client.clone();
            let url = gateway.url("/v1/chat/completions");
            tokio::spawn(async move {
                client
                    .post(url)
                    .header("x-relay-include-metadata", "1")
                    .json(&chat_request())
                    .send()
                    .await
            })
        })
        .collect::<Vec<_>>();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if primary.calls().await == 8 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("all delayed primary 429s reached upstream");

    let responses = futures_util::future::join_all(requests).await;
    for response in responses {
        let response = response
            .expect("concurrent gateway request task")
            .expect("concurrent gateway response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
        assert_eq!(header_value(&response, "x-relay-fallback"), "1");
        let _ = response
            .json::<Value>()
            .await
            .expect("concurrent fallback JSON");
    }

    assert_eq!(primary.calls().await, 8);
    assert_eq!(fallback.calls().await, 8);
    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("concurrent circuit status response")
        .json::<Value>()
        .await
        .expect("concurrent circuit status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "provider_capacity");
    assert_eq!(target["circuit"]["consecutive_failures"], 1);
    assert_eq!(target["circuit"]["backoff_level"], 1);
}

#[tokio::test]
async fn opencode_go_usage_limit_error_is_quota_and_caps_retry_after() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "type":"error",
                "error":{
                    "type":"GoUsageLimitError",
                    "message":"Subscription quota exceeded. You can continue using free models."
                },
                "metadata":{"limitName":"5 hour"}
            }),
        )
        .with_header("retry-after", "999999999999"),
    ])
    .await;
    let fallback = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("fallback reasoning", "fallback answer"),
    )])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("quota fallback response");
    let request_id = header_value(&response, "x-relay-request-id");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "x-relay-backend"), "deepseek");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    assert_eq!(
        header_value(&response, "x-relay-fallback-reason"),
        "provider_quota"
    );
    let _ = response.json::<Value>().await.expect("quota fallback JSON");

    let attempts = client
        .get(gateway.url("/api/attempts?limit=20"))
        .send()
        .await
        .expect("quota attempts response")
        .json::<Value>()
        .await
        .expect("quota attempts JSON");
    let request_attempts = attempts_for_request(&attempts, &request_id);
    let primary_attempt = request_attempts
        .iter()
        .find(|attempt| attempt["backend"] == "opencode-go")
        .expect("quota primary attempt");
    assert_eq!(primary_attempt["error_class"], "provider_quota");
    assert!(primary_attempt["retry_after_ms"].as_i64().unwrap() > 900_000);

    let status = client
        .get(gateway.url("/api/status"))
        .send()
        .await
        .expect("quota circuit status response")
        .json::<Value>()
        .await
        .expect("quota circuit status JSON");
    let target = status_target(&status, "opencode-go");
    assert_eq!(target["circuit"]["mode"], "open");
    assert_eq!(target["circuit"]["reason"], "provider_quota");
    let opened_at = target["circuit"]["opened_at_ms"]
        .as_i64()
        .expect("quota circuit opened timestamp");
    let next_probe_at = target["circuit"]["next_probe_at_ms"]
        .as_i64()
        .expect("quota next probe timestamp");
    assert!(next_probe_at >= opened_at);
    assert!(next_probe_at - opened_at <= Duration::from_secs(15 * 60).as_millis() as i64);
}

#[tokio::test]
async fn unused_later_layer_does_not_claim_a_half_open_probe() {
    let primary = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"trip primary"}}),
        )
        .with_header("retry-after", "0"),
        MockReply::json(
            StatusCode::OK,
            chat_completion("primary probe", "primary recovered"),
        ),
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"primary fails again"}}),
        )
        .with_header("retry-after", "0"),
    ])
    .await;
    let fallback = MockProvider::start(vec![
        MockReply::json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"trip fallback"}}),
        )
        .with_header("retry-after", "0"),
        MockReply::json(
            StatusCode::OK,
            chat_completion("fallback probe", "fallback recovered"),
        ),
    ])
    .await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let first = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .expect("trip both circuits");
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("primary recovery probe");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-backend"), "opencode-go");
    assert_eq!(fallback.calls().await, 1);

    let third = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("fallback recovery probe");
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(header_value(&third, "x-relay-backend"), "deepseek");
    assert_eq!(primary.calls().await, 3);
    assert_eq!(fallback.calls().await, 2);
}

#[tokio::test]
async fn routing_endpoints_report_configured_hierarchy_and_final_key_usage() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("reasoning", "answer"),
    )])
    .await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .expect("routing statistics request");
    assert_eq!(response.status(), StatusCode::OK);

    let routing = client
        .get(gateway.url("/api/routing"))
        .send()
        .await
        .expect("routing response")
        .json::<Value>()
        .await
        .expect("routing JSON");
    assert_eq!(routing["models"][0]["name"], LOGICAL_MODEL);
    assert_eq!(routing["models"][0]["aliases"][0], UPSTREAM_MODEL);
    assert_eq!(routing["models"][0]["layers"][0]["name"], "plan");
    assert_eq!(routing["models"][0]["layers"][0]["strategy"], "random");
    assert_eq!(
        routing["models"][0]["layers"][0]["targets"][0]["backend"],
        "opencode-go"
    );
    assert!(
        routing["models"][0]["layers"][0]["targets"][0]
            .get("provider")
            .is_none()
    );
    assert_eq!(
        routing["models"][0]["layers"][0]["targets"][0]["credential"],
        "go-plan"
    );
    assert!(
        routing["models"][0]["layers"][0]["targets"][0]
            .get("protocols")
            .is_none()
    );

    let stats = client
        .get(gateway.url(&format!(
            "/api/routing/stats?model={LOGICAL_MODEL}&window=all"
        )))
        .send()
        .await
        .expect("routing stats response")
        .json::<Value>()
        .await
        .expect("routing stats JSON");
    assert_eq!(stats["totals"]["calls"], 1);
    assert_eq!(stats["totals"]["input_tokens"], 11);
    assert_eq!(stats["totals"]["output_tokens"], 7);
    assert_eq!(stats["totals"]["total_tokens"], 18);
    assert_eq!(stats["layers"][0]["totals"]["calls"], 1);
    assert_eq!(stats["layers"][0]["targets"][0]["totals"]["calls"], 1);
    assert_eq!(stats["layers"][0]["targets"][0]["backend"], "opencode-go");
    assert_eq!(stats["layers"][1]["totals"]["calls"], 0);
    assert_eq!(stats["unattributed"]["calls"], 0);
    assert_eq!(stats["historical_targets"].as_array().unwrap().len(), 0);

    let alias_stats = client
        .get(gateway.url(&format!(
            "/api/routing/stats?model={UPSTREAM_MODEL}&window=all"
        )))
        .send()
        .await
        .expect("routing stats alias response")
        .json::<Value>()
        .await
        .expect("routing stats alias JSON");
    assert_eq!(alias_stats["model"]["name"], LOGICAL_MODEL);
    assert_eq!(alias_stats["totals"], stats["totals"]);

    let overview = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("overview stats response")
        .json::<Value>()
        .await
        .expect("overview stats JSON");
    assert_eq!(overview["requests"]["total"], 1);
    assert_eq!(overview["backends"]["opencode-go"]["attempts"], 1);
    assert_eq!(overview["backends"]["opencode-go"]["successes"], 1);
    assert!(overview.get("providers").is_none());

    let unknown = client
        .get(gateway.url("/api/routing/stats?model=missing&window=all"))
        .send()
        .await
        .expect("unknown model response");
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        unknown.json::<Value>().await.unwrap()["code"],
        "unknown_model"
    );
    let invalid_window = client
        .get(gateway.url(&format!(
            "/api/routing/stats?model={LOGICAL_MODEL}&window=year"
        )))
        .send()
        .await
        .expect("invalid window response");
    assert_eq!(invalid_window.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_window.json::<Value>().await.unwrap()["code"],
        "invalid_window"
    );
}

#[tokio::test]
async fn embedded_dashboard_preserves_api_404s_and_cache_correct_assets() {
    let primary = MockProvider::start(Vec::new()).await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let index = client
        .get(gateway.url("/"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("embedded index response");
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(
        index.headers().get("cache-control").unwrap(),
        "public, max-age=0, s-maxage=60, must-revalidate"
    );
    let index_etag = index
        .headers()
        .get("etag")
        .expect("index ETag")
        .to_str()
        .unwrap()
        .to_string();
    let index_html = index.text().await.expect("embedded index HTML");
    assert!(index_html.contains("QuotaMux"));
    let asset_start = index_html.find("/assets/").expect("hashed asset path");
    let asset_end = index_html[asset_start..]
        .find('"')
        .expect("hashed asset path end");
    let asset_path = &index_html[asset_start..asset_start + asset_end];

    let identity_asset = client
        .get(gateway.url(asset_path))
        .header("accept-encoding", "identity")
        .send()
        .await
        .expect("identity asset response");
    assert_eq!(identity_asset.status(), StatusCode::OK);
    assert_eq!(
        identity_asset.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );
    let identity_etag = identity_asset.headers().get("etag").unwrap().clone();
    assert!(identity_asset.headers().get("content-encoding").is_none());

    let gzip_asset = client
        .get(gateway.url(asset_path))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .expect("gzip asset response");
    assert_eq!(gzip_asset.status(), StatusCode::OK);
    assert_eq!(
        gzip_asset.headers().get("content-encoding").unwrap(),
        "gzip"
    );
    assert_ne!(gzip_asset.headers().get("etag").unwrap(), &identity_etag);

    let brotli_asset = client
        .get(gateway.url(asset_path))
        .header("accept-encoding", "br")
        .send()
        .await
        .expect("Brotli asset response");
    assert_eq!(brotli_asset.status(), StatusCode::OK);
    assert_eq!(
        brotli_asset.headers().get("content-encoding").unwrap(),
        "br"
    );
    assert_ne!(brotli_asset.headers().get("etag").unwrap(), &identity_etag);
    assert_ne!(
        brotli_asset.headers().get("etag").unwrap(),
        gzip_asset.headers().get("etag").unwrap()
    );

    let revalidated = client
        .get(gateway.url("/"))
        .header("accept", "text/html")
        .header("if-none-match", index_etag)
        .send()
        .await
        .expect("revalidated index response");
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(revalidated.bytes().await.unwrap().len(), 0);

    let html_fallback = client
        .get(gateway.url("/routing"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("HTML fallback response");
    assert_eq!(html_fallback.status(), StatusCode::OK);

    let missing_api = client
        .get(gateway.url("/api/missing"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("missing API response");
    assert_eq!(missing_api.status(), StatusCode::NOT_FOUND);
    assert!(!missing_api.text().await.unwrap().contains("QuotaMux"));

    let missing_asset = client
        .get(gateway.url("/assets/missing-deadbeef.js"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("missing immutable asset response");
    assert_eq!(missing_asset.status(), StatusCode::NOT_FOUND);
}
