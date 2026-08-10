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
    http::{HeaderName, HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{
        LOGICAL_MODEL, ModelConfig, ProviderConfig, ProvidersConfig, ServerConfig, UPSTREAM_MODEL,
    },
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
}

async fn mock_provider_handler(
    State(state): State<Arc<MockProviderState>>,
    Json(body): Json<Value>,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.request_bodies.lock().await.push(body);
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
        let data_dir = tempfile::tempdir().expect("create gateway data directory");
        let config = Config {
            config_version: 1,
            server: ServerConfig {
                listen: "127.0.0.1:0".into(),
                data_dir: data_dir.path().to_path_buf(),
            },
            model: ModelConfig {
                logical_name: LOGICAL_MODEL.into(),
            },
            providers: ProvidersConfig {
                opencode_go: ProviderConfig {
                    endpoint: primary.endpoint(),
                    api_key: "test-opencode-key".into(),
                    model: UPSTREAM_MODEL.into(),
                },
                deepseek: ProviderConfig {
                    endpoint: fallback.endpoint(),
                    api_key: "test-deepseek-key".into(),
                    model: UPSTREAM_MODEL.into(),
                },
            },
        };
        let state = Arc::new(AppState::new(config).await.expect("create gateway state"));
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
        }
    })
}

fn chat_request() -> Value {
    json!({
        "model": LOGICAL_MODEL,
        "messages": [{"role":"user","content":"hello"}]
    })
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
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
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
    assert_eq!(status["active_provider"], "deepseek");
    assert_eq!(status["circuit"]["mode"], "open");
    assert_eq!(status["circuit"]["reason"], "stream_failure");
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
async fn tool_call_history_preserves_reasoning_and_missing_reasoning_is_rejected() {
    let primary = MockProvider::start(vec![MockReply::json(
        StatusCode::OK,
        chat_completion("tool response reasoning", "tool response"),
    )])
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
    assert_eq!(primary.calls().await, 1);
    assert_eq!(fallback.calls().await, 0);
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
    assert_eq!(status["active_provider"], "opencode-go");
    assert_eq!(status["circuit"]["mode"], "closed");
    assert_eq!(status["circuit"]["consecutive_failures"], 0);

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
