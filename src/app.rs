use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};

use async_stream::stream;
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    circuit::{CircuitBreaker, RouteDecision},
    config::{Config, LOGICAL_MODEL},
    dashboard::INDEX_HTML,
    protocol::{self, ValidationError, anthropic, chat, responses},
    provider::{ProviderClient, ProviderError},
    sse::{SseDecoder, SseEvent},
    store::Store,
    types::{AlertRecord, AttemptRecord, FailureClass, Protocol, Provider, RequestRecord, Usage},
};

const METADATA_HEADER: &str = "x-relay-include-metadata";

pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub circuit: Arc<CircuitBreaker>,
    primary: ProviderClient,
    fallback: ProviderClient,
    started_at_ms: i64,
    balance: RwLock<Option<Value>>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let store = Store::open(&config.server.data_dir)?;
        let circuit = Arc::new(CircuitBreaker::load(store.clone())?);
        let primary =
            ProviderClient::new(Provider::OpenCodeGo, config.providers.opencode_go.clone())?;
        let fallback = ProviderClient::new(Provider::DeepSeek, config.providers.deepseek.clone())?;
        let balance = store.get_state("deepseek-balance").unwrap_or(None);
        Ok(Self {
            config,
            store,
            circuit,
            primary,
            fallback,
            started_at_ms: Utc::now().timestamp_millis(),
            balance: RwLock::new(balance),
        })
    }

    pub fn start_background(self: &Arc<Self>) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
            loop {
                interval.tick().await;
                match state.fallback.balance().await {
                    Ok(balance) => {
                        *state.balance.write().await = Some(balance.clone());
                        if let Err(error) = state.store.put_state("deepseek-balance", &balance) {
                            tracing::warn!(%error, "failed to persist DeepSeek balance snapshot");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(class=%error.class.as_str(), "DeepSeek balance refresh failed")
                    }
                }
            }
        });
    }
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/api/status", get(status))
        .route("/api/stats", get(stats))
        .route("/api/requests", get(requests))
        .route("/api/attempts", get(attempts))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"status":"ok","model":LOGICAL_MODEL,"uptime_seconds":(Utc::now().timestamp_millis()-state.started_at_ms)/1000}),
    )
}

async fn models() -> Json<Value> {
    Json(
        json!({"object":"list","data":[{"id":LOGICAL_MODEL,"object":"model","created":0,"owned_by":"quotamux"},{"id":"deepseek-v4-flash","object":"model","created":0,"owned_by":"quotamux"}]}),
    )
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    handle_inference(state, Protocol::OpenAiChat, headers, body).await
}

async fn openai_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    handle_inference(state, Protocol::OpenAiResponses, headers, body).await
}

async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    handle_inference(state, Protocol::AnthropicMessages, headers, body).await
}

async fn count_tokens(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if has_provider_selector(&headers, &body) {
        return client_error(
            Protocol::AnthropicMessages,
            "clients cannot select a provider",
            Some("provider"),
        );
    }
    if let Err(error) = protocol::validate_model(&body) {
        return validation_error(Protocol::AnthropicMessages, error);
    }
    Json(json!({"input_tokens":anthropic::estimate_tokens(&body),"x_quotamux_estimated":true}))
        .into_response()
}

async fn handle_inference(
    state: Arc<AppState>,
    protocol: Protocol,
    headers: HeaderMap,
    body: Value,
) -> Response {
    if has_provider_selector(&headers, &body) {
        return client_error(
            protocol,
            "clients cannot select a provider",
            Some("provider"),
        );
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        handle_stream(state, protocol, headers, body).await
    } else {
        handle_nonstream(state, protocol, headers, body).await
    }
}

async fn handle_nonstream(
    state: Arc<AppState>,
    protocol: Protocol,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let started_at_ms = Utc::now().timestamp_millis();
    let started = Instant::now();
    let request_bytes = serde_json::to_vec(&body)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let include_metadata = metadata_requested(&headers);
    let decision = state.circuit.decide().await;
    let (first_provider, probe, circuit_reason) = match decision {
        RouteDecision::Primary { probe } => (Provider::OpenCodeGo, probe, None),
        RouteDecision::Fallback { reason } => (Provider::DeepSeek, false, reason),
    };

    let first = execute_json_attempt(
        &state,
        first_provider,
        protocol,
        &headers,
        &body,
        &request_id,
        1,
        probe,
        started,
    )
    .await;
    let outcome = match first {
        Ok(mut success) => {
            if first_provider == Provider::OpenCodeGo {
                state.circuit.success().await;
            } else {
                success.fallback_reason = circuit_reason;
            }
            success
        }
        Err(failure)
            if first_provider == Provider::OpenCodeGo && failure.error.class.allows_fallback() =>
        {
            state
                .circuit
                .failure(failure.error.class, failure.error.retry_after)
                .await;
            record_alert_if_needed(&state, &request_id, failure.error.class).await;
            match execute_json_attempt(
                &state,
                Provider::DeepSeek,
                protocol,
                &headers,
                &body,
                &request_id,
                2,
                false,
                started,
            )
            .await
            {
                Ok(mut success) => {
                    success.fallback_reason = Some(failure.error.class);
                    success
                }
                Err(fallback) => {
                    return terminal_failure(
                        &state,
                        protocol,
                        &request_id,
                        started_at_ms,
                        started,
                        request_bytes,
                        fallback,
                        true,
                        include_metadata,
                        Some(failure.error.class),
                    )
                    .await;
                }
            }
        }
        Err(failure) => {
            if first_provider == Provider::OpenCodeGo {
                state
                    .circuit
                    .failure(failure.error.class, failure.error.retry_after)
                    .await;
                record_alert_if_needed(&state, &request_id, failure.error.class).await;
            }
            return terminal_failure(
                &state,
                protocol,
                &request_id,
                started_at_ms,
                started,
                request_bytes,
                failure,
                first_provider == Provider::DeepSeek,
                include_metadata,
                circuit_reason,
            )
            .await;
        }
    };

    let response_bytes = serde_json::to_vec(&outcome.body).unwrap_or_default();
    let usage = usage_for(protocol, &outcome.raw_upstream);
    let record = RequestRecord {
        id: request_id.clone(),
        started_at_ms,
        completed_at_ms: Utc::now().timestamp_millis(),
        protocol,
        requested_model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(LOGICAL_MODEL)
            .into(),
        streaming: false,
        status: 200,
        error_class: None,
        provider: Some(outcome.provider),
        fallback: outcome.provider == Provider::DeepSeek,
        translated: outcome.translated,
        request_bytes,
        response_bytes: response_bytes.len() as u64,
        first_byte_ms: Some(outcome.first_byte_ms),
        total_ms: started.elapsed().as_millis() as u64,
        usage,
        claude_session_id: header_text(&headers, "x-claude-code-session-id"),
        claude_agent_id: header_text(&headers, "x-claude-code-agent-id"),
        claude_parent_agent_id: header_text(&headers, "x-claude-code-parent-agent-id"),
    };
    persist_request(&state, &record);
    let mut response = (StatusCode::OK, Json(outcome.body)).into_response();
    apply_metadata(
        &mut response,
        include_metadata,
        &request_id,
        outcome.provider,
        state_provider(&state, outcome.provider).model(),
        outcome.fallback_reason,
        protocol,
        outcome.egress,
        outcome.translated,
    );
    response
}

struct JsonSuccess {
    provider: Provider,
    egress: Protocol,
    translated: bool,
    body: Value,
    raw_upstream: Value,
    first_byte_ms: u64,
    fallback_reason: Option<FailureClass>,
}

struct AttemptFailure {
    error: ProviderError,
}

#[allow(clippy::too_many_arguments)]
async fn execute_json_attempt(
    state: &Arc<AppState>,
    provider: Provider,
    ingress: Protocol,
    headers: &HeaderMap,
    original: &Value,
    request_id: &str,
    sequence: u32,
    probe: bool,
    request_started: Instant,
) -> Result<JsonSuccess, AttemptFailure> {
    let attempt_id = Uuid::now_v7().to_string();
    let started_at_ms = Utc::now().timestamp_millis();
    let started = Instant::now();
    let (egress, translated, prepared) = match prepare_for_provider(
        provider,
        ingress,
        original,
        state_provider(state, provider).model(),
    ) {
        Ok(value) => value,
        Err(error) => {
            return Err(AttemptFailure {
                error: ProviderError {
                    class: FailureClass::ClientRequest,
                    status: Some(error.status),
                    retry_after: None,
                    safe_message: error.message,
                },
            });
        }
    };
    let request_bytes = serde_json::to_vec(&prepared)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let response = match state_provider(state, provider)
        .send(egress, &prepared, headers)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                provider,
                state_provider(state, provider).model(),
                egress,
                translated,
                probe,
                started_at_ms,
                started,
                request_bytes,
                &error,
            );
            persist_attempt(state, &record);
            return Err(AttemptFailure { error });
        }
    };
    let first_byte_ms = request_started.elapsed().as_millis() as u64;
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => {
            let error = ProviderError {
                class: FailureClass::ProviderTransient,
                status: Some(status),
                retry_after: None,
                safe_message: "upstream response body failed".into(),
            };
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                provider,
                state_provider(state, provider).model(),
                egress,
                translated,
                probe,
                started_at_ms,
                started,
                request_bytes,
                &error,
            );
            persist_attempt(state, &record);
            return Err(AttemptFailure { error });
        }
    };
    let raw: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            let error = ProviderError {
                class: FailureClass::ProviderUnknown5xxOrTransport,
                status: Some(status),
                retry_after: None,
                safe_message: "upstream returned malformed JSON".into(),
            };
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                provider,
                state_provider(state, provider).model(),
                egress,
                translated,
                probe,
                started_at_ms,
                started,
                request_bytes,
                &error,
            );
            persist_attempt(state, &record);
            return Err(AttemptFailure { error });
        }
    };
    let body = if translated {
        translate_nonstream(ingress, &raw)
    } else {
        raw.clone()
    };
    let usage = usage_for(egress, &raw);
    let cost = provider_cost(provider, &raw, &usage);
    let record = AttemptRecord {
        id: attempt_id,
        request_id: Some(request_id.into()),
        sequence,
        provider,
        upstream_model: state_provider(state, provider).model().into(),
        egress_protocol: egress,
        translated,
        probe,
        started_at_ms,
        completed_at_ms: Utc::now().timestamp_millis(),
        status: Some(status.as_u16()),
        error_class: None,
        retry_after_ms: None,
        committed: true,
        request_bytes,
        response_bytes: bytes.len() as u64,
        first_byte_ms: Some(first_byte_ms),
        total_ms: started.elapsed().as_millis() as u64,
        usage,
        provider_cost_usd: cost,
        sanitized_error: None,
    };
    persist_attempt(state, &record);
    Ok(JsonSuccess {
        provider,
        egress,
        translated,
        body,
        raw_upstream: raw,
        first_byte_ms,
        fallback_reason: None,
    })
}

async fn handle_stream(
    state: Arc<AppState>,
    protocol: Protocol,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let started_at_ms = Utc::now().timestamp_millis();
    let started = Instant::now();
    let request_bytes = serde_json::to_vec(&body)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let include_metadata = metadata_requested(&headers);
    let decision = state.circuit.decide().await;
    let (first_provider, probe, circuit_reason) = match decision {
        RouteDecision::Primary { probe } => (Provider::OpenCodeGo, probe, None),
        RouteDecision::Fallback { reason } => (Provider::DeepSeek, false, reason),
    };
    let first = prepare_stream_attempt(
        state.clone(),
        first_provider,
        protocol,
        &headers,
        &body,
        &request_id,
        1,
        probe,
        started,
    )
    .await;
    let prepared = match first {
        Ok(mut prepared) => {
            if first_provider == Provider::DeepSeek {
                prepared.fallback_reason = circuit_reason;
            }
            prepared
        }
        Err(failure)
            if first_provider == Provider::OpenCodeGo && failure.error.class.allows_fallback() =>
        {
            state
                .circuit
                .failure(failure.error.class, failure.error.retry_after)
                .await;
            record_alert_if_needed(&state, &request_id, failure.error.class).await;
            match prepare_stream_attempt(
                state.clone(),
                Provider::DeepSeek,
                protocol,
                &headers,
                &body,
                &request_id,
                2,
                false,
                started,
            )
            .await
            {
                Ok(mut prepared) => {
                    prepared.fallback_reason = Some(failure.error.class);
                    prepared
                }
                Err(fallback) => {
                    return terminal_failure(
                        &state,
                        protocol,
                        &request_id,
                        started_at_ms,
                        started,
                        request_bytes,
                        fallback,
                        true,
                        include_metadata,
                        Some(failure.error.class),
                    )
                    .await;
                }
            }
        }
        Err(failure) => {
            return terminal_failure(
                &state,
                protocol,
                &request_id,
                started_at_ms,
                started,
                request_bytes,
                failure,
                first_provider == Provider::DeepSeek,
                include_metadata,
                circuit_reason,
            )
            .await;
        }
    };

    let provider = prepared.provider;
    let egress = prepared.egress;
    let translated = prepared.translated;
    let fallback_reason = prepared.fallback_reason;
    let first_byte_ms = started.elapsed().as_millis() as u64;
    let state_for_stream = state.clone();
    let request_id_for_stream = request_id.clone();
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(LOGICAL_MODEL)
        .to_string();
    let claude_session_id = header_text(&headers, "x-claude-code-session-id");
    let claude_agent_id = header_text(&headers, "x-claude-code-agent-id");
    let claude_parent_agent_id = header_text(&headers, "x-claude-code-parent-agent-id");
    let first_event = prepared.first_event;
    let mut decoder = prepared.decoder;
    let attempt_id = prepared.attempt_id;
    let sequence = prepared.sequence;
    let probe = prepared.probe;
    let attempt_started_at_ms = prepared.attempt_started_at_ms;
    let upstream_request_bytes = prepared.request_bytes;
    let upstream_model = state_provider(&state, provider).model().to_string();
    let stream = stream! {
        let mut response_bytes = 0_u64;
        let mut usage = Usage::default();
        let mut stream_error = None;
        let mut response_translator = responses::ChatToResponsesStream::new();
        let mut anthropic_translator = anthropic::ChatToAnthropicStream::new();
        let mut pending_event = Some(first_event);
        loop {
            let next = if let Some(event) = pending_event.take() {
                Ok(Some(event))
            } else {
                decoder.next_event().await
            };
            match next {
                Ok(Some(event)) => {
                    if event.data == "[DONE]" {
                        if translated {
                            let tail = match protocol {
                                Protocol::OpenAiResponses => response_translator.finish(),
                                Protocol::AnthropicMessages => anthropic_translator.finish(),
                                Protocol::OpenAiChat => Vec::new(),
                            };
                            for event in tail { let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes); }
                        } else if protocol == Protocol::OpenAiChat {
                            let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes);
                        }
                        break;
                    }
                    if translated {
                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(chunk) => {
                                if chunk.get("usage").is_some_and(|value| !value.is_null()) { usage = Usage::from_openai(&chunk); }
                                let events = match protocol {
                                    Protocol::OpenAiResponses => response_translator.translate(&chunk),
                                    Protocol::AnthropicMessages => anthropic_translator.translate(&chunk),
                                    Protocol::OpenAiChat => vec![event],
                                };
                                for event in events { let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes); }
                            }
                            Err(_) => { stream_error=Some(FailureClass::StreamFailure); break; }
                        }
                    } else {
                        let parsed=serde_json::from_str::<Value>(&event.data).ok();
                        if let Some(value)=parsed.as_ref() { usage = usage_for(protocol, value); }
                        let event_type=event.event.as_deref().or_else(||parsed.as_ref()?.get("type")?.as_str());
                        let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes);
                        if protocol == Protocol::OpenAiResponses && matches!(event_type, Some("response.completed"|"response.incomplete"|"response.failed")) { break; }
                        if protocol == Protocol::AnthropicMessages && event_type==Some("message_stop") { break; }
                    }
                }
                Ok(None) => {
                    stream_error=Some(FailureClass::StreamFailure);
                    break;
                }
                Err(_) => { stream_error=Some(FailureClass::StreamFailure); break; }
            }
        }
        let completed_at_ms=Utc::now().timestamp_millis();
        let attempt = AttemptRecord { id:attempt_id,request_id:Some(request_id_for_stream.clone()),sequence,provider,upstream_model,egress_protocol:egress,translated,probe,started_at_ms:attempt_started_at_ms,completed_at_ms,status:Some(200),error_class:stream_error,retry_after_ms:None,committed:true,request_bytes:upstream_request_bytes,response_bytes,first_byte_ms:Some(first_byte_ms),total_ms:started.elapsed().as_millis() as u64,usage:usage.clone(),provider_cost_usd:provider_cost(provider,&Value::Null,&usage),sanitized_error:stream_error.map(|_|"upstream stream ended unexpectedly".into())};
        persist_attempt(&state_for_stream,&attempt);
        let request = RequestRecord { id:request_id_for_stream,started_at_ms,completed_at_ms,protocol,requested_model,streaming:true,status:if stream_error.is_some(){502}else{200},error_class:stream_error,provider:Some(provider),fallback:provider==Provider::DeepSeek,translated,request_bytes,response_bytes,first_byte_ms:Some(first_byte_ms),total_ms:started.elapsed().as_millis() as u64,usage,claude_session_id,claude_agent_id,claude_parent_agent_id};
        persist_request(&state_for_stream,&request);
        if provider==Provider::OpenCodeGo { if let Some(class)=stream_error { state_for_stream.circuit.failure(class,None).await; } else { state_for_stream.circuit.success().await; } }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    apply_metadata(
        &mut response,
        include_metadata,
        &request_id,
        provider,
        state_provider(&state, provider).model(),
        fallback_reason,
        protocol,
        egress,
        translated,
    );
    response
}

struct PreparedStream {
    provider: Provider,
    egress: Protocol,
    translated: bool,
    decoder: SseDecoder,
    first_event: SseEvent,
    attempt_id: String,
    sequence: u32,
    probe: bool,
    attempt_started_at_ms: i64,
    request_bytes: u64,
    fallback_reason: Option<FailureClass>,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_stream_attempt(
    state: Arc<AppState>,
    provider: Provider,
    ingress: Protocol,
    headers: &HeaderMap,
    original: &Value,
    request_id: &str,
    sequence: u32,
    probe: bool,
    request_started: Instant,
) -> Result<PreparedStream, AttemptFailure> {
    let attempt_id = Uuid::now_v7().to_string();
    let attempt_started_at_ms = Utc::now().timestamp_millis();
    let attempt_started = Instant::now();
    let (egress, translated, prepared) = match prepare_for_provider(
        provider,
        ingress,
        original,
        state_provider(&state, provider).model(),
    ) {
        Ok(value) => value,
        Err(error) => {
            return Err(AttemptFailure {
                error: ProviderError {
                    class: FailureClass::ClientRequest,
                    status: Some(error.status),
                    retry_after: None,
                    safe_message: error.message,
                },
            });
        }
    };
    let request_bytes = serde_json::to_vec(&prepared)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let response = match state_provider(&state, provider)
        .send(egress, &prepared, headers)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                provider,
                state_provider(&state, provider).model(),
                egress,
                translated,
                probe,
                attempt_started_at_ms,
                attempt_started,
                request_bytes,
                &error,
            );
            persist_attempt(&state, &record);
            return Err(AttemptFailure { error });
        }
    };
    let status = response.status();
    let mut decoder = SseDecoder::new(Box::pin(response.bytes_stream()));
    let event = match decoder.next_event().await {
        Ok(Some(event)) => event,
        Ok(None) | Err(_) => {
            let error = ProviderError {
                class: FailureClass::StreamFailure,
                status: Some(status),
                retry_after: None,
                safe_message: "upstream stream failed before its first semantic event".into(),
            };
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                provider,
                state_provider(&state, provider).model(),
                egress,
                translated,
                probe,
                attempt_started_at_ms,
                attempt_started,
                request_bytes,
                &error,
            );
            persist_attempt(&state, &record);
            return Err(AttemptFailure { error });
        }
    };
    if event.event.as_deref() == Some("error")
        || serde_json::from_str::<Value>(&event.data)
            .ok()
            .is_some_and(|value| value.get("error").is_some())
    {
        let error = ProviderError {
            class: FailureClass::StreamFailure,
            status: Some(status),
            retry_after: None,
            safe_message: "upstream returned an SSE error before commit".into(),
        };
        let record = failed_attempt(
            &attempt_id,
            Some(request_id),
            sequence,
            provider,
            state_provider(&state, provider).model(),
            egress,
            translated,
            probe,
            attempt_started_at_ms,
            attempt_started,
            request_bytes,
            &error,
        );
        persist_attempt(&state, &record);
        return Err(AttemptFailure { error });
    }
    if translated && event.data != "[DONE]" {
        serde_json::from_str::<Value>(&event.data).map_err(|_| AttemptFailure {
            error: ProviderError {
                class: FailureClass::StreamFailure,
                status: Some(status),
                retry_after: None,
                safe_message: "upstream sent malformed SSE before commit".into(),
            },
        })?;
    }
    let _ = request_started;
    Ok(PreparedStream {
        provider,
        egress,
        translated,
        decoder,
        first_event: event,
        attempt_id,
        sequence,
        probe,
        attempt_started_at_ms,
        request_bytes,
        fallback_reason: None,
    })
}

fn prepare_for_provider(
    provider: Provider,
    ingress: Protocol,
    original: &Value,
    model: &str,
) -> Result<(Protocol, bool, Value), ValidationError> {
    match (provider, ingress) {
        (_, Protocol::OpenAiChat) => Ok((
            Protocol::OpenAiChat,
            false,
            chat::prepare(original.clone(), model)?,
        )),
        (Provider::OpenCodeGo, Protocol::OpenAiResponses) => Ok((
            Protocol::OpenAiChat,
            true,
            responses::prepare_for_chat(original.clone(), model)?,
        )),
        (Provider::DeepSeek, Protocol::OpenAiResponses) => Ok((
            Protocol::OpenAiResponses,
            false,
            responses::prepare_direct(original.clone(), model)?,
        )),
        (Provider::OpenCodeGo, Protocol::AnthropicMessages) => Ok((
            Protocol::OpenAiChat,
            true,
            anthropic::prepare_for_chat(original.clone(), model)?,
        )),
        (Provider::DeepSeek, Protocol::AnthropicMessages) => Ok((
            Protocol::AnthropicMessages,
            false,
            anthropic::prepare_direct(original.clone(), model)?,
        )),
    }
}

fn translate_nonstream(ingress: Protocol, raw: &Value) -> Value {
    match ingress {
        Protocol::OpenAiResponses => responses::chat_to_response(raw),
        Protocol::AnthropicMessages => anthropic::chat_to_message(raw),
        Protocol::OpenAiChat => raw.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn terminal_failure(
    state: &Arc<AppState>,
    protocol: Protocol,
    request_id: &str,
    started_at_ms: i64,
    started: Instant,
    request_bytes: u64,
    failure: AttemptFailure,
    fallback: bool,
    include_metadata: bool,
    fallback_reason: Option<FailureClass>,
) -> Response {
    let class = if fallback {
        FailureClass::FallbackUnavailable
    } else {
        failure.error.class
    };
    let status = failure.error.status.unwrap_or(StatusCode::BAD_GATEWAY);
    let record = RequestRecord {
        id: request_id.into(),
        started_at_ms,
        completed_at_ms: Utc::now().timestamp_millis(),
        protocol,
        requested_model: LOGICAL_MODEL.into(),
        streaming: false,
        status: status.as_u16(),
        error_class: Some(class),
        provider: if fallback {
            Some(Provider::DeepSeek)
        } else {
            Some(Provider::OpenCodeGo)
        },
        fallback,
        translated: false,
        request_bytes,
        response_bytes: 0,
        first_byte_ms: None,
        total_ms: started.elapsed().as_millis() as u64,
        usage: Usage::default(),
        claude_session_id: None,
        claude_agent_id: None,
        claude_parent_agent_id: None,
    };
    persist_request(state, &record);
    record_alert_if_needed(state, request_id, class).await;
    let body = error_body(protocol, &failure.error.safe_message, class);
    let mut response = (status, Json(body)).into_response();
    let provider = if fallback {
        Provider::DeepSeek
    } else {
        Provider::OpenCodeGo
    };
    apply_metadata(
        &mut response,
        include_metadata,
        request_id,
        provider,
        state_provider(state, provider).model(),
        fallback_reason,
        protocol,
        protocol,
        false,
    );
    response
}

fn error_body(protocol: Protocol, message: &str, class: FailureClass) -> Value {
    match protocol {
        Protocol::AnthropicMessages => {
            json!({"type":"error","error":{"type":"api_error","message":message},"request_id":Uuid::now_v7().to_string()})
        }
        _ => json!({"error":{"message":message,"type":class.as_str(),"code":class.as_str()}}),
    }
}

fn client_error(protocol: Protocol, message: &str, param: Option<&str>) -> Response {
    let body = match protocol {
        Protocol::AnthropicMessages => {
            json!({"type":"error","error":{"type":"invalid_request_error","message":message}})
        }
        _ => {
            json!({"error":{"message":message,"type":"invalid_request_error","param":param,"code":"invalid_request_error"}})
        }
    };
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

fn validation_error(protocol: Protocol, error: ValidationError) -> Response {
    let status = error.status;
    if protocol == Protocol::AnthropicMessages {
        client_error(protocol, &error.message, error.param)
    } else {
        (status, Json(error.response())).into_response()
    }
}

#[allow(clippy::too_many_arguments)]
fn failed_attempt(
    id: &str,
    request_id: Option<&str>,
    sequence: u32,
    provider: Provider,
    model: &str,
    egress: Protocol,
    translated: bool,
    probe: bool,
    started_at_ms: i64,
    started: Instant,
    request_bytes: u64,
    error: &ProviderError,
) -> AttemptRecord {
    AttemptRecord {
        id: id.into(),
        request_id: request_id.map(str::to_string),
        sequence,
        provider,
        upstream_model: model.into(),
        egress_protocol: egress,
        translated,
        probe,
        started_at_ms,
        completed_at_ms: Utc::now().timestamp_millis(),
        status: error.status.map(|v| v.as_u16()),
        error_class: Some(error.class),
        retry_after_ms: error
            .retry_after
            .map(|v| v.as_millis().min(i64::MAX as u128) as i64),
        committed: false,
        request_bytes,
        response_bytes: 0,
        first_byte_ms: None,
        total_ms: started.elapsed().as_millis() as u64,
        usage: Usage::default(),
        provider_cost_usd: None,
        sanitized_error: Some(error.safe_message.clone()),
    }
}

fn usage_for(protocol: Protocol, value: &Value) -> Usage {
    match protocol {
        Protocol::OpenAiChat => Usage::from_openai(value),
        Protocol::OpenAiResponses => Usage::from_responses(value),
        Protocol::AnthropicMessages => Usage::from_anthropic(value),
    }
}

fn provider_cost(provider: Provider, raw: &Value, usage: &Usage) -> Option<f64> {
    if provider == Provider::OpenCodeGo {
        return raw
            .get("cost")
            .and_then(Value::as_f64)
            .or_else(|| raw.get("cost").and_then(Value::as_str)?.parse().ok());
    }
    Some(
        (usage.cache_hit_tokens as f64 * 0.0028
            + usage.cache_miss_tokens as f64 * 0.14
            + usage.output_tokens as f64 * 0.28)
            / 1_000_000.0,
    )
}

fn state_provider(state: &AppState, provider: Provider) -> &ProviderClient {
    match provider {
        Provider::OpenCodeGo => &state.primary,
        Provider::DeepSeek => &state.fallback,
    }
}
fn persist_request(state: &AppState, record: &RequestRecord) {
    if let Err(error) = state.store.record_request(record) {
        tracing::error!(%error,"failed to persist request")
    }
}
fn persist_attempt(state: &AppState, record: &AttemptRecord) {
    if let Err(error) = state.store.record_attempt(record) {
        tracing::error!(%error,"failed to persist attempt")
    }
}

async fn record_alert_if_needed(state: &AppState, request_id: &str, class: FailureClass) {
    if !matches!(
        class,
        FailureClass::ProviderAuth
            | FailureClass::ProviderBilling
            | FailureClass::ProviderConfiguration
            | FailureClass::ProviderUnknown4xx
            | FailureClass::FallbackUnavailable
    ) {
        return;
    }
    let now = Utc::now().timestamp_millis();
    let snapshot = state.circuit.snapshot().await;
    let provider = if class == FailureClass::FallbackUnavailable {
        Provider::DeepSeek
    } else {
        Provider::OpenCodeGo
    };
    let record = AlertRecord {
        id: format!("{}:{}", provider.as_str(), class.as_str()),
        provider,
        class,
        active: true,
        first_seen_ms: now,
        last_seen_ms: now,
        next_probe_at_ms: snapshot.next_probe_at_ms,
        request_id: Some(request_id.into()),
    };
    if let Err(error) = state.store.record_alert(&record) {
        tracing::error!(%error,"failed to persist alert")
    }
}

fn metadata_requested(headers: &HeaderMap) -> bool {
    headers.get(METADATA_HEADER).and_then(|v| v.to_str().ok()) == Some("1")
}
fn has_provider_selector(headers: &HeaderMap, body: &Value) -> bool {
    body.get("provider").is_some()
        || headers.contains_key("x-relay-provider")
        || headers.contains_key("x-provider")
}
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(256).collect())
}

#[allow(clippy::too_many_arguments)]
fn apply_metadata(
    response: &mut Response,
    include: bool,
    request_id: &str,
    provider: Provider,
    model: &str,
    fallback_reason: Option<FailureClass>,
    ingress: Protocol,
    egress: Protocol,
    translated: bool,
) {
    if !include {
        return;
    }
    let headers = response.headers_mut();
    for (name, value) in [
        ("x-relay-request-id", request_id.to_string()),
        ("x-relay-provider", provider.as_str().into()),
        ("x-relay-upstream-model", model.into()),
        (
            "x-relay-fallback",
            if provider == Provider::DeepSeek {
                "1".into()
            } else {
                "0".into()
            },
        ),
        (
            "x-relay-fallback-reason",
            fallback_reason.map(|v| v.as_str()).unwrap_or("none").into(),
        ),
        ("x-relay-ingress-protocol", ingress.as_str().into()),
        ("x-relay-egress-protocol", egress.as_str().into()),
        (
            "x-relay-translated",
            if translated { "1".into() } else { "0".into() },
        ),
    ] {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            headers.insert(name, value);
        }
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}
async fn requests(State(state): State<Arc<AppState>>, Query(query): Query<LimitQuery>) -> Response {
    match state.store.requests(query.limit.unwrap_or(100).min(1000)) {
        Ok(requests) => Json(json!({"requests":requests})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error})),
        )
            .into_response(),
    }
}
async fn attempts(State(state): State<Arc<AppState>>, Query(query): Query<LimitQuery>) -> Response {
    match state.store.attempts(query.limit.unwrap_or(100).min(1000)) {
        Ok(attempts) => Json(json!({"attempts":attempts})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error})),
        )
            .into_response(),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let circuit = state.circuit.snapshot().await;
    let alerts = state.store.alerts(100).unwrap_or_default();
    let balance = state.balance.read().await.clone();
    let display = balance
        .as_ref()
        .and_then(|v| v.get("balance_infos"))
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .and_then(|v| {
            Some(format!(
                "{} {}",
                v.get("total_balance")?.as_str()?,
                v.get("currency")?.as_str()?
            ))
        });
    Json(
        json!({"active_provider":if circuit.mode==crate::circuit::CircuitMode::Closed{"opencode-go"}else{"deepseek"},"circuit":circuit,"providers":{"opencode-go":{"endpoint":safe_endpoint(state.primary.endpoint()),"model":state.primary.model(),"key_configured":true},"deepseek":{"endpoint":safe_endpoint(state.fallback.endpoint()),"model":state.fallback.model(),"key_configured":true}},"deepseek_balance":{"display":display,"raw":balance},"alerts":alerts}),
    )
}

fn safe_endpoint(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return "invalid endpoint".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string().trim_end_matches('/').to_string()
}

async fn stats(State(state): State<Arc<AppState>>) -> Response {
    let requests = match state.store.requests(10000) {
        Ok(v) => v,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":error})),
            )
                .into_response();
        }
    };
    let attempts = state.store.attempts(20000).unwrap_or_default();
    let mut providers: BTreeMap<String, Value> = BTreeMap::new();
    for provider in [Provider::OpenCodeGo, Provider::DeepSeek] {
        let rows = attempts
            .iter()
            .filter(|a| a.provider == provider)
            .collect::<Vec<_>>();
        let total = rows.len() as u64;
        let successes = rows.iter().filter(|a| a.error_class.is_none()).count() as u64;
        let cache_reported = rows.iter().filter(|a| a.usage.provider_reported).count() as u64;
        let hit = rows.iter().map(|a| a.usage.cache_hit_tokens).sum::<u64>();
        let miss = rows.iter().map(|a| a.usage.cache_miss_tokens).sum::<u64>();
        let cost = rows.iter().filter_map(|a| a.provider_cost_usd).sum::<f64>();
        providers.insert(provider.as_str().into(),json!({"attempts":total,"successes":successes,"errors":total-successes,"cache_reported_attempts":cache_reported,"cache_hit_tokens":hit,"cache_miss_tokens":miss,"cost_usd":cost}));
    }
    let total = requests.len() as u64;
    let errors = requests.iter().filter(|r| r.error_class.is_some()).count() as u64;
    let fallbacks = requests.iter().filter(|r| r.fallback).count() as u64;
    let output_tokens = requests.iter().map(|r| r.usage.output_tokens).sum::<u64>();
    let bytes = requests
        .iter()
        .map(|r| r.request_bytes + r.response_bytes)
        .sum::<u64>();
    Json(json!({"requests":{"total":total,"errors":errors,"fallbacks":fallbacks,"output_tokens":output_tokens,"bytes":bytes},"providers":providers})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_provider_selection() {
        assert!(has_provider_selector(
            &HeaderMap::new(),
            &json!({"provider":"deepseek"})
        ));
    }
    #[test]
    fn calculates_deepseek_cost() {
        let usage = Usage {
            cache_hit_tokens: 1_000_000,
            ..Default::default()
        };
        assert!(
            (provider_cost(Provider::DeepSeek, &Value::Null, &usage).unwrap() - 0.0028).abs()
                < 1e-9
        );
    }
}
