use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{
        CredentialConfig, LOGICAL_MODEL, ModelPricingConfig, ProviderConfig, ProviderKind,
        ProviderModelConfig, RouteLayerConfig, RouteStrategy, RouteTargetConfig, ServedModelConfig,
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
}

struct MockReply {
    status: StatusCode,
    body: MockBody,
    headers: Vec<(String, String)>,
}

impl MockReply {
    fn json(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body: MockBody::Json(body),
            headers: Vec::new(),
        }
    }

    fn sse(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: MockBody::Raw(body.into()),
            headers: vec![(CONTENT_TYPE.as_str().into(), "text/event-stream".into())],
        }
    }

    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn into_response(self) -> Response {
        let mut response = match self.body {
            MockBody::Json(value) => Json(value).into_response(),
            MockBody::Raw(body) => Response::new(Body::from(body)),
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
            config_version: 2,
            server: ServerConfig {
                listen: "127.0.0.1:0".into(),
                data_dir: "unused-test-data".into(),
            },
            affinity: Default::default(),
            providers: vec![
                ProviderConfig {
                    id: "opencode-go".into(),
                    kind: ProviderKind::OpenCodeGo,
                    endpoint: Some(primary.endpoint()),
                    credentials: vec![CredentialConfig {
                        id: "go-plan".into(),
                        api_key: "test-opencode-key".into(),
                    }],
                    models: vec![ProviderModelConfig {
                        name: UPSTREAM_MODEL.into(),
                        protocols: vec![Protocol::OpenAiChat],
                        pricing: None,
                    }],
                },
                ProviderConfig {
                    id: "deepseek".into(),
                    kind: ProviderKind::DeepSeekOfficial,
                    endpoint: Some(fallback.endpoint()),
                    credentials: vec![CredentialConfig {
                        id: "deepseek-payg".into(),
                        api_key: "test-deepseek-key".into(),
                    }],
                    models: vec![ProviderModelConfig {
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
                            provider: "opencode-go".into(),
                            credential: "go-plan".into(),
                            model: UPSTREAM_MODEL.into(),
                        }],
                    },
                    RouteLayerConfig {
                        name: "payg".into(),
                        strategy: RouteStrategy::Random,
                        targets: vec![RouteTargetConfig {
                            provider: "deepseek".into(),
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
        let data_dir = tempfile::tempdir().expect("create gateway data directory");
        config.server.data_dir = data_dir.path().to_path_buf();
        let state = Arc::new(
            AppState::new_with_random_seed(config, seed)
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

fn test_provider(id: &str, credential: &str, upstream: &MockProvider) -> ProviderConfig {
    test_provider_kind(
        id,
        credential,
        upstream,
        ProviderKind::OpenCodeGo,
        Protocol::OpenAiChat,
    )
}

fn test_provider_protocol(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    protocol: Protocol,
) -> ProviderConfig {
    test_provider_kind(id, credential, upstream, ProviderKind::OpenCodeGo, protocol)
}

fn test_provider_kind(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    kind: ProviderKind,
    protocol: Protocol,
) -> ProviderConfig {
    test_provider_kind_model(id, credential, upstream, kind, protocol, UPSTREAM_MODEL)
}

fn test_provider_kind_model(
    id: &str,
    credential: &str,
    upstream: &MockProvider,
    kind: ProviderKind,
    protocol: Protocol,
    model: &str,
) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        kind,
        endpoint: Some(upstream.endpoint()),
        credentials: vec![CredentialConfig {
            id: credential.into(),
            api_key: format!("test-key-{credential}"),
        }],
        models: vec![ProviderModelConfig {
            name: model.into(),
            protocols: vec![protocol],
            pricing: None,
        }],
    }
}

fn target(provider: &str, credential: &str) -> RouteTargetConfig {
    target_model(provider, credential, UPSTREAM_MODEL)
}

fn target_model(provider: &str, credential: &str, model: &str) -> RouteTargetConfig {
    RouteTargetConfig {
        provider: provider.into(),
        credential: credential.into(),
        model: model.into(),
    }
}

fn test_config(
    providers: Vec<ProviderConfig>,
    layers: Vec<(&str, Vec<RouteTargetConfig>)>,
) -> Config {
    Config {
        config_version: 2,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
        },
        affinity: Default::default(),
        providers,
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

fn status_target<'a>(body: &'a Value, provider: &str) -> &'a Value {
    body["targets"]
        .as_array()
        .expect("status targets array")
        .iter()
        .find(|target| target["provider"].as_str() == Some(provider))
        .unwrap_or_else(|| panic!("missing status target {provider}"))
}

#[tokio::test]
async fn openai_chat_success_exposes_reasoning_and_provider_metadata() {
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
    assert_eq!(header_value(&response, "x-relay-provider"), "opencode-go");
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

    let stats = client
        .get(gateway.url("/api/stats"))
        .send()
        .await
        .expect("provider stats response")
        .json::<Value>()
        .await
        .expect("provider stats JSON");
    assert_eq!(stats["providers"]["deepseek"]["attempts"], 0);
    assert_eq!(
        stats["providers"]["deepseek"]["models"],
        json!([UPSTREAM_MODEL])
    );
}

#[tokio::test]
async fn configured_model_pricing_drives_cost_for_any_provider_kind() {
    let upstream = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("priced reasoning", "priced answer"),
    )])
    .await;
    let mut config = test_config(
        vec![test_provider_kind(
            "priced-custom",
            "priced-key",
            &upstream,
            ProviderKind::CustomChatCompletions,
            Protocol::OpenAiChat,
        )],
        vec![("priced", vec![target("priced-custom", "priced-key")])],
    );
    config.providers[0].models[0].pricing = Some(ModelPricingConfig {
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
    let actual = stats["providers"]["priced-custom"]["cost_usd"]
        .as_f64()
        .expect("configured cost");
    assert!((actual - expected).abs() < 1e-12);
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
        config_version: 2,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
        },
        affinity: Default::default(),
        providers: vec![
            test_provider_kind_model(
                "kimi-primary",
                "primary-key",
                &primary,
                ProviderKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_provider_kind_model(
                "kimi-fallback",
                "fallback-key",
                &fallback,
                ProviderKind::KimiOfficial,
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
        assert_eq!(body["error"]["message"], "named tool choice is unsupported");
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
        config_version: 2,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: "unused-test-data".into(),
        },
        affinity: Default::default(),
        providers: vec![
            test_provider_kind_model(
                "kimi-primary",
                "primary-key",
                &primary,
                ProviderKind::KimiOfficial,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_provider_kind_model(
                "kimi-fallback",
                "fallback-key",
                &fallback,
                ProviderKind::KimiOfficial,
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
    assert_eq!(body["error"]["message"], "reasoning_content is required");

    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
    let upstream_requests = primary.request_bodies().await;
    assert_eq!(upstream_requests.len(), 1);
    let upstream = &upstream_requests[0];
    assert_eq!(upstream["model"], "kimi-k3");
    assert_eq!(upstream["reasoning_effort"], "high");
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
        vec![test_provider_kind_model(
            "kimi-code",
            "allegretto",
            &upstream,
            ProviderKind::KimiCode,
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
    assert_eq!(header_value(&response, "x-relay-provider"), "kimi-code");
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
async fn semantic_validation_is_deferred_upstream_for_every_ingress_protocol() {
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
    assert!(
        upstream[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("future_item")
    );
    assert_eq!(upstream[1]["reasoning_effort"], "future_effort");
    assert_eq!(upstream[1]["tools"][0]["type"], "future_tool");
    assert!(
        upstream[2]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("future_block")
    );
    assert_eq!(upstream[2]["thinking"]["type"], "future_thinking");
    assert_eq!(upstream[2]["reasoning_effort"], "future_effort");
    assert_eq!(upstream[2]["response_format"]["type"], "future_format");
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
            test_provider("worker-a", "key-a", &worker_a),
            test_provider("worker-b", "key-b", &worker_b),
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
            .filter(|attempt| attempt["provider"] == "worker-a")
            .count(),
        count_a
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["provider"] == "worker-b")
            .count(),
        count_b
    );
    eprintln!(
        "RANDOM_LAYER_EVIDENCE {}",
        json!({"requests":REQUESTS,"worker_a":count_a,"worker_b":count_b,"persisted_attempts":attempts.len()})
    );
}

#[tokio::test]
async fn prompt_prefix_affinity_routes_a_divergent_branch_across_mixed_provider_kinds() {
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
            test_provider("worker-a", "key-a", &worker_a),
            test_provider_kind(
                "worker-b",
                "key-b",
                &worker_b,
                ProviderKind::DeepSeekOfficial,
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
    let warm_provider = header_value(&first, "x-relay-provider");
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
    assert_eq!(header_value(&second, "x-relay-provider"), warm_provider);
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
    assert_eq!(warm_attempt[0]["provider"], warm_provider);
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
            test_provider("single-worker", "single-key", &worker),
            test_provider("unused-worker", "unused-key", &unused),
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
    assert_eq!(stats["providers"]["single-worker"]["attempts"], 1);
    assert_eq!(stats["providers"]["unused-worker"]["attempts"], 0);
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
            test_provider("worker-a", "key-a", &worker_a),
            test_provider("worker-b", "key-b", &worker_b),
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
    let warm_provider = header_value(&first, "x-relay-provider");
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
    assert_eq!(header_value(&second, "x-relay-provider"), warm_provider);
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
            test_provider("plan-a", "key-a", &plan_a),
            test_provider("plan-b", "key-b", &plan_b),
            test_provider("payg", "key-payg", &payg),
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
    assert_eq!(header_value(&response, "x-relay-provider"), "payg");
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
            test_provider_kind_model(
                "opencode-go-kimi",
                "go-plan",
                &go,
                ProviderKind::OpenCodeGo,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_provider_kind_model(
                "kimi-code",
                "allegretto",
                &code,
                ProviderKind::KimiCode,
                Protocol::OpenAiChat,
                "k3",
            ),
            test_provider_kind_model(
                "kimi-official",
                "official-payg",
                &official,
                ProviderKind::KimiOfficial,
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
    assert_eq!(header_value(&response, "x-relay-provider"), "kimi-official");
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
        stats["providers"]["opencode-go-kimi"]["models"][0],
        "kimi-k3"
    );
    assert_eq!(stats["providers"]["kimi-code"]["models"][0], "k3");
    assert_eq!(stats["providers"]["kimi-official"]["models"][0], "kimi-k3");
    let routes = stats["routes"].as_array().expect("Kimi stats routes");
    for (provider, upstream_model) in [
        ("opencode-go-kimi", "kimi-k3"),
        ("kimi-code", "k3"),
        ("kimi-official", "kimi-k3"),
    ] {
        let route = routes
            .iter()
            .find(|route| {
                route["provider"].as_str() == Some(provider)
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
            test_provider_kind_model(
                "opencode-go-kimi",
                "go-plan",
                &go,
                ProviderKind::OpenCodeGo,
                Protocol::OpenAiChat,
                "kimi-k3",
            ),
            test_provider_kind_model(
                "kimi-code",
                "allegretto",
                &code,
                ProviderKind::KimiCode,
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
    let warm_provider = header_value(&first, "x-relay-provider");
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
    assert_eq!(header_value(&second, "x-relay-provider"), warm_provider);
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
        vec![test_provider_protocol(
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
        vec![test_provider_protocol(
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
        vec![test_provider_protocol(
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
        vec![test_provider_protocol(
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
    assert_eq!(header_value(&response, "x-relay-provider"), "deepseek");
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
    assert_eq!(overview["providers"]["opencode-go"]["attempts"], 1);
    assert_eq!(overview["providers"]["deepseek"]["attempts"], 1);
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
    assert_eq!(header_value(&response, "x-relay-provider"), "deepseek");
    assert_eq!(header_value(&response, "x-relay-fallback"), "1");
    let body = response.json::<Value>().await.expect("terminal JSON");
    assert_eq!(body["error"]["type"], "fallback_unavailable");
    assert_eq!(body["error"]["message"], "fallback unavailable");
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 1);
}

#[tokio::test]
async fn client_provider_selection_is_rejected_without_upstream_calls() {
    let primary = MockProvider::start(Vec::new()).await;
    let fallback = MockProvider::start(Vec::new()).await;
    let gateway = Gateway::start(&primary, &fallback).await;
    let client = reqwest::Client::new();

    let mut field_body = chat_request();
    field_body["provider"] = json!("deepseek");
    let field_response = client
        .post(gateway.url("/v1/chat/completions"))
        .json(&field_body)
        .send()
        .await
        .expect("field validation response");
    assert_eq!(field_response.status(), StatusCode::BAD_REQUEST);
    let field_error = field_response
        .json::<Value>()
        .await
        .expect("field validation JSON");
    assert_eq!(field_error["error"]["type"], "invalid_request_error");

    let header_response = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-provider", "opencode-go")
        .json(&chat_request())
        .send()
        .await
        .expect("header validation response");
    assert_eq!(header_response.status(), StatusCode::BAD_REQUEST);
    let header_error = header_response
        .json::<Value>()
        .await
        .expect("header validation JSON");
    assert_eq!(header_error["error"]["type"], "invalid_request_error");
    assert_eq!(primary.calls().await, 0);
    assert_eq!(fallback.calls().await, 0);
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
    assert_eq!(header_value(&response, "x-relay-provider"), "deepseek");
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
    assert_eq!(header_value(&response, "x-relay-provider"), "opencode-go");
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
async fn anthropic_nonstream_contains_thinking_and_text() {
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
    assert_eq!(body["content"][0]["type"], "thinking");
    assert_eq!(body["content"][0]["thinking"], "anthropic thinking");
    assert_eq!(body["content"][1]["type"], "text");
    assert_eq!(body["content"][1]["text"], "anthropic text");
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
    assert_eq!(
        missing_error["error"]["message"],
        "reasoning_content is required"
    );
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
    assert_eq!(header_value(&first, "x-relay-provider"), "deepseek");
    let _ = first.json::<Value>().await.expect("first recovery JSON");

    let second = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("recovery probe request");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(header_value(&second, "x-relay-provider"), "opencode-go");
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
    assert_eq!(header_value(&second, "x-relay-provider"), "opencode-go");
    assert_eq!(fallback.calls().await, 1);

    let third = client
        .post(gateway.url("/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&chat_request())
        .send()
        .await
        .expect("fallback recovery probe");
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(header_value(&third, "x-relay-provider"), "deepseek");
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
    assert_eq!(overview["providers"]["opencode-go"]["attempts"], 1);
    assert_eq!(overview["providers"]["opencode-go"]["successes"], 1);

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
