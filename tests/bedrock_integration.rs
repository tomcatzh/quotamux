#![cfg(feature = "test-support")]

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use quotamux::{AppState, Config, build_app};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{sync::Mutex, task::JoinHandle};
use tower::ServiceExt;

const IDS: [&str; 4] = [
    "global.anthropic.claude-opus-5-5",
    "global.anthropic.claude-sonnet-5-5",
    "global.anthropic.claude-haiku-5-5",
    "global.anthropic.claude-fable-5-2",
];

#[derive(Clone)]
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Value,
}

enum Reply {
    Json(StatusCode, Option<&'static str>, Value),
    Stream(Vec<u8>),
    Sse(String),
    HangingStream(Vec<u8>),
}

struct FakeState {
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<Captured>>,
}

struct FakeBedrock {
    url: String,
    state: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Drop for FakeBedrock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeBedrock {
    async fn start(replies: Vec<Reply>) -> Self {
        async fn handle(State(state): State<Arc<FakeState>>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, 1024 * 1024).await.unwrap();
            state.calls.lock().await.push(Captured {
                path: parts.uri.path().into(),
                headers: parts.headers,
                body: serde_json::from_slice(&body).unwrap(),
            });
            match state
                .replies
                .lock()
                .await
                .pop_front()
                .expect("unexpected SDK retry")
            {
                Reply::Json(status, code, value) => {
                    let mut response = (status, Json(value)).into_response();
                    if let Some(code) = code {
                        response
                            .headers_mut()
                            .insert("x-amzn-errortype", code.parse().unwrap());
                    }
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        response
                            .headers_mut()
                            .insert("retry-after", "7".parse().unwrap());
                    }
                    response
                }
                Reply::Stream(bytes) => {
                    // Network chunks deliberately split the AWS prelude, CRCs,
                    // headers, base64 payload, and UTF-8 content arbitrarily.
                    let chunks = bytes
                        .chunks(17)
                        .map(Bytes::copy_from_slice)
                        .collect::<Vec<_>>();
                    let stream =
                        futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
                    Response::builder()
                        .header("content-type", "application/vnd.amazon.eventstream")
                        .header("x-amzn-bedrock-content-type", "application/json")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
                Reply::Sse(text) => Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(text))
                    .unwrap(),
                Reply::HangingStream(bytes) => {
                    let stream = async_stream::stream! {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(bytes));
                        std::future::pending::<()>().await;
                    };
                    Response::builder()
                        .header("content-type", "application/vnd.amazon.eventstream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }
        }
        let state = Arc::new(FakeState {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
        });
        let router = Router::new().fallback(handle).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, state, task }
    }
}

struct Gateway {
    app: Router,
    _data: TempDir,
}

impl Gateway {
    async fn start(primary: &FakeBedrock, fallback: Option<&FakeBedrock>) -> Self {
        Self::start_with_timeouts(primary, fallback, json!({})).await
    }

    async fn start_with_timeouts(
        primary: &FakeBedrock,
        fallback: Option<&FakeBedrock>,
        timeouts: Value,
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut backends = vec![json!({
            "id":"bedrock", "adapter":"aws-bedrock", "region":"us-east-1",
            "credentials":[{"id":"instance-role"}],
            "models":IDS.iter().map(|id|json!({"name":id})).collect::<Vec<_>>()
        })];
        if let Some(fallback) = fallback {
            backends.push(json!({
                "id":"fallback", "adapter":"custom-anthropic", "endpoint":format!("{}/messages", fallback.url),
                "credentials":[{"id":"key", "api_key":"fake-fallback-key"}],
                "models":[{"name":"same-model", "protocols":["anthropic-messages"]}]
            }));
        }
        let models = IDS.iter().enumerate().map(|(index, id)| {
            let mut layers = vec![json!({"name":"aws", "strategy":"random", "targets":[{"backend":"bedrock", "credential":"instance-role", "model":id}]})];
            if fallback.is_some() {
                layers.push(json!({"name":"backup", "strategy":"random", "targets":[{"backend":"fallback", "credential":"key", "model":"same-model"}]}));
            }
            json!({"name":format!("public-{index}"), "aliases":[id], "protocols":["anthropic-messages", "openai-chat", "openai-responses"], "layers":layers})
        }).collect::<Vec<_>>();
        let config: Config = serde_json::from_value(json!({
            "config_version":4,
            "server":{"listen":"127.0.0.1:8081", "data_dir":data.path(), "timeouts":timeouts},
            "backends":backends, "models":models
        }))
        .unwrap();
        let endpoints = HashMap::from([("bedrock".into(), primary.url.clone())]);
        let state = AppState::new_with_random_seed_and_test_endpoints(config, 13, endpoints)
            .await
            .unwrap();
        Self {
            app: build_app(Arc::new(state)),
            _data: data,
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .header("x-relay-include-metadata", "1")
                    .header("anthropic-version", "2023-06-01")
                    .header(
                        "anthropic-beta",
                        "context-management-2025-06-27, compact-2026-09-04",
                    )
                    .header("authorization", "Bearer client-secret-do-not-forward")
                    .body(Body::from(serde_json::to_vec(body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        json_body(response).await
    }
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn request(model: usize, stream: bool) -> Value {
    json!({"model":format!("public-{model}"), "max_tokens":4096, "stream":stream,
        "messages":[{"role":"user", "content":"你好"}], "thinking":{"type":"adaptive"},
        "output_config":{"effort":"high"}})
}

fn answer() -> Value {
    json!({"id":"msg_bedrock", "type":"message", "role":"assistant", "model":IDS[0],
        "content":[{"type":"thinking", "thinking":"保留思考", "signature":"opaque-signature"},
            {"type":"text", "text":"你好"}], "stop_reason":"end_turn", "stop_sequence":null,
        "usage":{"input_tokens":7, "cache_creation_input_tokens":3, "cache_read_input_tokens":11, "output_tokens":5},
        "future_field":{"preserve":true}})
}

#[tokio::test]
async fn four_global_profiles_use_role_sigv4_and_preserve_native_payloads() {
    let upstream = FakeBedrock::start(
        (0..4)
            .map(|_| Reply::Json(StatusCode::OK, None, answer()))
            .collect(),
    )
    .await;
    let gateway = Gateway::start(&upstream, None).await;
    for index in 0..IDS.len() {
        let mut body = request(index, false);
        body["system"] = json!([{"type":"text", "text":"系统", "cache_control":{"type":"ephemeral", "ttl":"1h"}}]);
        body["messages"] = json!([
            {"role":"user", "content":"请调用工具"},
            {"role":"assistant", "content":[{"type":"thinking", "thinking":"历史", "signature":"unchanged"},
                {"type":"tool_use", "id":"tool_1", "name":"lookup", "input":{"q":"值"}}]},
            {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool_1", "content":"结果"}]}
        ]);
        body["tools"] = json!([{"name":"lookup", "input_schema":{"type":"object"}}]);
        body["future_field"] = json!({"transparent":true});
        let response = gateway.post("/v1/messages", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-relay-backend"], "bedrock");
        assert_eq!(json_body(response).await, answer());
        let calls = upstream.state.calls.lock().await;
        let captured = &calls[index];
        assert_eq!(captured.path, format!("/model/{}/invoke", IDS[index]));
        let auth = captured.headers["authorization"].to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 "));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
        assert!(!auth.contains("client-secret"));
        assert_eq!(
            captured.headers["x-amz-security-token"],
            "quotamux-test-session"
        );
        assert!(!captured.headers.contains_key("x-api-key"));
        assert!(!captured.headers.contains_key("anthropic-beta"));
        assert_eq!(captured.body["anthropic_version"], "bedrock-2023-05-31");
        assert!(captured.body.get("model").is_none());
        assert!(captured.body.get("stream").is_none());
        assert_eq!(
            captured.body["anthropic_beta"],
            json!(["context-management-2025-06-27", "compact-2026-09-04"])
        );
        for key in [
            "messages",
            "system",
            "tools",
            "thinking",
            "output_config",
            "future_field",
        ] {
            assert_eq!(captured.body[key], body[key], "changed native field {key}");
        }
    }
    let attempts = gateway.get("/api/attempts?limit=20").await;
    for attempt in attempts["attempts"].as_array().unwrap() {
        assert_eq!(attempt["adapter"], "aws-bedrock");
        assert_eq!(attempt["translated"], false);
        assert_eq!(attempt["usage"]["input_tokens"], 21);
        assert_eq!(attempt["usage"]["output_tokens"], 5);
    }
}

#[tokio::test]
async fn global_profile_names_are_accepted_as_gateway_aliases() {
    let upstream = FakeBedrock::start(
        (0..4)
            .map(|_| Reply::Json(StatusCode::OK, None, answer()))
            .collect(),
    )
    .await;
    let gateway = Gateway::start(&upstream, None).await;
    for (index, id) in IDS.iter().enumerate() {
        let mut body = request(index, false);
        body["model"] = json!(id);
        let response = gateway.post("/v1/messages", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            upstream.state.calls.lock().await[index].path,
            format!("/model/{id}/invoke")
        );
    }
}

#[tokio::test]
async fn auth_cooldown_preserves_the_original_failure_in_status_and_attempts() {
    let upstream = FakeBedrock::start(vec![Reply::Json(
        StatusCode::FORBIDDEN,
        Some("AccessDeniedException"),
        json!({"message":"synthetic permission denied"}),
    )])
    .await;
    let gateway = Gateway::start(&upstream, None).await;
    let first = gateway.post("/v1/messages", &request(0, false)).await;
    assert_eq!(first.status(), StatusCode::FORBIDDEN);
    let second = gateway.post("/v1/messages", &request(0, false)).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = json_body(second).await;
    assert_eq!(
        error["error"]["message"],
        "no route target is currently available"
    );
    assert!(error["error"]["upstream_type"].is_null());
    let state = gateway.get("/api/status").await;
    let target = state["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == IDS[0])
        .unwrap();
    assert_eq!(target["circuit"]["mode"], "suspended");
    assert_eq!(target["circuit"]["reason"], "provider_auth");
    assert!(target["circuit"]["next_probe_at_ms"].as_i64().is_some());
    let attempts = gateway.get("/api/attempts?limit=10").await;
    assert_eq!(attempts["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(
        attempts["attempts"][0]["upstream_error_type"],
        "AccessDeniedException"
    );
    assert_eq!(upstream.state.calls.lock().await.len(), 1);
}

#[tokio::test]
async fn aws_validation_errors_never_fallback_or_open_the_circuit() {
    let upstream = FakeBedrock::start(vec![
        Reply::Json(
            StatusCode::BAD_REQUEST,
            Some("ValidationException"),
            json!({"message":"invalid request"}),
        ),
        Reply::Json(StatusCode::OK, None, answer()),
    ])
    .await;
    let fallback = FakeBedrock::start(vec![]).await;
    let gateway = Gateway::start(&upstream, Some(&fallback)).await;
    let response = gateway.post("/v1/messages", &request(0, false)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["upstream_type"],
        "ValidationException"
    );
    assert!(fallback.state.calls.lock().await.is_empty());
    let second = gateway.post("/v1/messages", &request(0, false)).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(upstream.state.calls.lock().await.len(), 2);
    let attempts = gateway.get("/api/attempts?limit=20").await;
    assert!(
        attempts["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["error_class"] == "client_request")
    );
}

#[tokio::test]
async fn aws_throttling_falls_back_once_without_hidden_sdk_retries() {
    let upstream = FakeBedrock::start(vec![Reply::Json(
        StatusCode::TOO_MANY_REQUESTS,
        Some("ThrottlingException"),
        json!({"message":"rate exceeded"}),
    )])
    .await;
    let fallback = FakeBedrock::start(vec![Reply::Json(StatusCode::OK, None, answer())]).await;
    let gateway = Gateway::start(&upstream, Some(&fallback)).await;
    let response = gateway.post("/v1/messages", &request(0, false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-relay-backend"], "fallback");
    assert_eq!(
        response.headers()["x-relay-fallback-reason"],
        "provider_capacity"
    );
    assert_eq!(upstream.state.calls.lock().await.len(), 1);
    assert_eq!(fallback.state.calls.lock().await.len(), 1);
    let attempts = gateway.get("/api/attempts?limit=20").await;
    let primary = attempts["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["backend"] == "bedrock")
        .unwrap();
    assert_eq!(primary["status"], 429);
    assert_eq!(primary["retry_after_ms"], 7000);
}

#[tokio::test]
async fn aws_future_errors_and_expired_tokens_keep_real_status_and_classification() {
    for (status, code, class) in [
        (400, "FutureRequestException", "client_request"),
        (403, "FutureAuthException", "provider_auth"),
        (403, "ExpiredTokenException", "provider_auth"),
        (400, "ServiceQuotaExceededException", "provider_capacity"),
    ] {
        let upstream = FakeBedrock::start(vec![Reply::Json(
            StatusCode::from_u16(status).unwrap(),
            Some(code),
            json!({"message":"account-private-detail"}),
        )])
        .await;
        let gateway = Gateway::start(&upstream, None).await;
        let response = gateway.post("/v1/messages", &request(0, false)).await;
        let body = json_body(response).await;
        assert!(
            !body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("account-private-detail")
        );
        let attempts = gateway.get("/api/attempts?limit=20").await;
        let row = &attempts["attempts"][0];
        assert_eq!(row["status"], status, "{code}");
        assert_eq!(row["error_class"], class, "{code}");
        assert_eq!(upstream.state.calls.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn existing_openai_ingress_translates_to_bedrock_messages() {
    let upstream = FakeBedrock::start(vec![
        Reply::Json(StatusCode::OK, None, answer()),
        Reply::Json(StatusCode::OK, None, answer()),
    ])
    .await;
    let gateway = Gateway::start(&upstream, None).await;
    let chat = gateway
        .post(
            "/v1/chat/completions",
            &json!({"model":"public-0", "messages":[{"role":"user","content":"hello"}]}),
        )
        .await;
    assert_eq!(chat.status(), StatusCode::OK);
    assert_eq!(
        json_body(chat).await["choices"][0]["message"]["content"],
        "你好"
    );
    let responses = gateway
        .post(
            "/v1/responses",
            &json!({"model":"public-0", "input":"hello"}),
        )
        .await;
    assert_eq!(responses.status(), StatusCode::OK);
    assert_eq!(json_body(responses).await["status"], "completed");
    for call in upstream.state.calls.lock().await.iter() {
        assert_eq!(call.body["messages"][0]["role"], "user");
        assert_eq!(call.body["anthropic_version"], "bedrock-2023-05-31");
    }
}

fn stream_events() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{"id":"msg_stream", "type":"message", "role":"assistant", "model":IDS[0],
            "content":[], "stop_reason":null, "stop_sequence":null, "usage":{"input_tokens":7,"cache_read_input_tokens":11,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta","thinking":"思考"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta","signature":"opaque"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use","id":"tool_stream","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta","partial_json":"{\"q\":\"值\"}"}}),
        json!({"type":"content_block_stop", "index":1}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use","stop_sequence":null}, "usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]
}

#[tokio::test]
async fn fragmented_aws_eventstream_becomes_native_sse_with_tools_thinking_and_usage() {
    let events = stream_events();
    let upstream = FakeBedrock::start(vec![Reply::Stream(
        events.iter().flat_map(chunk_frame).collect(),
    )])
    .await;
    let gateway = Gateway::start(&upstream, None).await;
    let response = gateway.post("/v1/messages", &request(0, true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let actual = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, events);
    assert_eq!(
        upstream.state.calls.lock().await[0].path,
        format!("/model/{}/invoke-with-response-stream", IDS[0])
    );
    let attempts = gateway.get("/api/attempts?limit=20").await;
    let row = &attempts["attempts"][0];
    assert!(row["error_class"].is_null());
    assert_eq!(row["usage"]["input_tokens"], 18);
    assert_eq!(row["usage"]["output_tokens"], 5);
}

#[tokio::test]
async fn aws_stream_exception_before_first_event_can_fallback() {
    let upstream = FakeBedrock::start(vec![Reply::Stream(exception_frame())]).await;
    let text = stream_events()
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect::<String>();
    let fallback = FakeBedrock::start(vec![Reply::Sse(text)]).await;
    let gateway = Gateway::start(&upstream, Some(&fallback)).await;
    let response = gateway.post("/v1/messages", &request(0, true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-relay-backend"], "fallback");
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("message_stop")
    );
    let attempts = gateway.get("/api/attempts?limit=20").await;
    let primary = attempts["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["backend"] == "bedrock")
        .unwrap();
    assert_eq!(primary["error_class"], "provider_transient");
    assert_eq!(fallback.state.calls.lock().await.len(), 1);
}

#[tokio::test]
async fn aws_stream_exception_after_commit_is_recorded_and_never_falls_back() {
    let mut bytes = chunk_frame(&stream_events()[0]);
    bytes.extend(exception_frame());
    let upstream = FakeBedrock::start(vec![Reply::Stream(bytes)]).await;
    let fallback = FakeBedrock::start(vec![]).await;
    let gateway = Gateway::start(&upstream, Some(&fallback)).await;
    let response = gateway.post("/v1/messages", &request(0, true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("event: error"));
    assert!(!text.contains("event: message_stop"));
    assert!(fallback.state.calls.lock().await.is_empty());
    let attempts = gateway.get("/api/attempts?limit=20").await;
    assert_eq!(attempts["attempts"][0]["error_class"], "provider_transient");
}

#[tokio::test]
async fn count_tokens_stays_an_explicit_local_estimate_without_aws_calls() {
    let upstream = FakeBedrock::start(vec![]).await;
    let gateway = Gateway::start(&upstream, None).await;
    let response = gateway
        .post("/v1/messages/count_tokens", &request(0, false))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result = json_body(response).await;
    assert_eq!(result["x_quotamux_estimated"], true);
    assert!(result["input_tokens"].as_u64().unwrap() > 0);
    assert!(upstream.state.calls.lock().await.is_empty());
}

#[tokio::test]
async fn stalled_aws_stream_ends_at_configured_idle_deadline_after_commit() {
    let upstream =
        FakeBedrock::start(vec![Reply::HangingStream(chunk_frame(&stream_events()[0]))]).await;
    let fallback = FakeBedrock::start(vec![]).await;
    let gateway = Gateway::start_with_timeouts(
        &upstream,
        Some(&fallback),
        json!({
            "upstream_connect_ms":100, "upstream_read_ms":100, "upstream_stream_read_ms":50,
            "upstream_total_ms":1000
        }),
    )
    .await;
    let response = gateway.post("/v1/messages", &request(0, true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("event: message_start"));
    assert!(!text.contains("event: message_stop"));
    let attempts = gateway.get("/api/attempts?limit=20").await;
    assert_eq!(attempts["attempts"][0]["error_class"], "stream_failure");
    assert!(fallback.state.calls.lock().await.is_empty());
}

// Minimal fixture encoder for the AWS event-stream wire contract. Production
// decoding is performed by the official AWS SDK, independently of this helper.
fn chunk_frame(value: &Value) -> Vec<u8> {
    frame(
        &[
            (":message-type", "event"),
            (":event-type", "chunk"),
            (":content-type", "application/json"),
        ],
        serde_json::to_vec(&json!({"bytes":base64(&serde_json::to_vec(value).unwrap())})).unwrap(),
    )
}

fn exception_frame() -> Vec<u8> {
    frame(&[(":message-type", "exception"), (":exception-type", "modelStreamErrorException"), (":content-type", "application/json")],
        serde_json::to_vec(&json!({"message":"stream failed", "originalStatusCode":503, "originalMessage":"overloaded"})).unwrap())
}

fn frame(headers: &[(&str, &str)], payload: Vec<u8>) -> Vec<u8> {
    let mut encoded_headers = Vec::new();
    for (name, value) in headers {
        encoded_headers.push(name.len() as u8);
        encoded_headers.extend(name.as_bytes());
        encoded_headers.push(7); // UTF-8 string
        encoded_headers.extend((value.len() as u16).to_be_bytes());
        encoded_headers.extend(value.as_bytes());
    }
    let mut bytes = Vec::new();
    bytes.extend(((16 + encoded_headers.len() + payload.len()) as u32).to_be_bytes());
    bytes.extend((encoded_headers.len() as u32).to_be_bytes());
    bytes.extend(crc32(&bytes).to_be_bytes());
    bytes.extend(encoded_headers);
    bytes.extend(payload);
    bytes.extend(crc32(&bytes).to_be_bytes());
    bytes
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        result.push(TABLE[((n >> 18) & 63) as usize] as char);
        result.push(TABLE[((n >> 12) & 63) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

#[tokio::test]
async fn reported_bedrock_thinking_usage_survives_native_responses_and_ledgers() {
    for streaming in [false, true] {
        let mut expected = answer();
        expected["usage"]["output_tokens_details"] = json!({"thinking_tokens":4});
        let mut events = stream_events();
        let delta = events
            .iter_mut()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        delta["usage"]["output_tokens_details"] = json!({"thinking_tokens":4});
        let reply = if streaming {
            Reply::Stream(events.iter().flat_map(chunk_frame).collect())
        } else {
            Reply::Json(StatusCode::OK, None, expected.clone())
        };
        let upstream = FakeBedrock::start(vec![reply]).await;
        let gateway = Gateway::start(&upstream, None).await;
        let response = gateway.post("/v1/messages", &request(0, streaming)).await;
        assert_eq!(response.status(), StatusCode::OK);
        if streaming {
            let text = String::from_utf8(
                to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            let actual = text
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, events);
        } else {
            assert_eq!(json_body(response).await, expected);
        }
        for (path, key) in [
            ("/api/attempts?limit=10", "attempts"),
            ("/api/requests?limit=10", "requests"),
        ] {
            let rows = gateway.get(path).await;
            let usage = &rows[key][0]["usage"];
            assert_eq!(usage["reasoning_tokens"], 4);
            assert_eq!(usage["reasoning_reported"], true);
            assert_eq!(usage["reasoning_estimated_tokens"], 0);
            assert_eq!(usage["output_tokens"], 5);
            assert_eq!(usage["input_tokens"], if streaming { 18 } else { 21 });
        }
    }
}
