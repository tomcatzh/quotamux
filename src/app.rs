use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
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
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    affinity::{
        AffinityDirectory, CacheDomain, CacheEvidence, EvidenceConfidence, FingerprintPath,
    },
    circuit::{CircuitBreaker, RouteDecision},
    config::{Config, ProviderKind, RouteStrategy, RouteTargetConfig, ServedModelConfig},
    dashboard::serve_spa,
    protocol::{self, ValidationError, anthropic, chat, responses},
    provider::{ProviderClient, ProviderError},
    routing::RandomSelector,
    sse::{SseDecoder, SseEvent},
    store::{AttemptRollup, RequestRollup, RouteRollupKey, Store},
    types::{AlertRecord, AttemptRecord, FailureClass, Protocol, RequestRecord, Usage},
};

const METADATA_HEADER: &str = "x-relay-include-metadata";

pub struct AppState {
    pub config: Config,
    pub store: Store,
    targets: HashMap<RouteTargetConfig, Arc<TargetRuntime>>,
    selector: RandomSelector,
    affinity: AffinityDirectory,
    started_at_ms: i64,
}

struct TargetRuntime {
    client: ProviderClient,
    circuit: Arc<CircuitBreaker>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_with_selector(config, RandomSelector::default()).await
    }

    #[doc(hidden)]
    pub async fn new_with_random_seed(
        config: Config,
        seed: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_with_selector(config, RandomSelector::with_seed(seed)).await
    }

    async fn new_with_selector(
        config: Config,
        selector: RandomSelector,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let store = Store::open(&config.server.data_dir)?;
        if config.provider("opencode-go").is_some() && config.provider("open-code-go").is_none() {
            let migrated = store.rename_provider("open-code-go", "opencode-go")?;
            if migrated > 0 {
                tracing::info!(migrated, "merged legacy open-code-go provider data");
            }
        }
        store.ensure_stats_schema()?;
        let affinity =
            AffinityDirectory::new(config.affinity.clone()).map_err(std::io::Error::other)?;
        let mut targets = HashMap::new();
        for served_model in &config.models {
            for layer in &served_model.layers {
                for target in &layer.targets {
                    if let Entry::Vacant(entry) = targets.entry(target.clone()) {
                        let provider = config
                            .provider(&target.provider)
                            .expect("validated route provider");
                        let credential = provider
                            .credential(&target.credential)
                            .expect("validated route credential");
                        let model = provider
                            .model(&target.model)
                            .expect("validated route model");
                        let client = ProviderClient::new(provider, credential, model)?;
                        let circuit_key = format!(
                            "circuit:{}:{}:{}",
                            target.provider, target.credential, target.model
                        );
                        let circuit = Arc::new(CircuitBreaker::load(store.clone(), circuit_key)?);
                        entry.insert(Arc::new(TargetRuntime { client, circuit }));
                    }
                }
            }
        }
        Ok(Self {
            config,
            store,
            targets,
            selector,
            affinity,
            started_at_ms: Utc::now().timestamp_millis(),
        })
    }

    pub fn start_background(self: &Arc<Self>) {
        let affinity_state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                affinity_state
                    .affinity
                    .expire(Utc::now().timestamp_millis());
            }
        });
        let stats_store = self.store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            interval.tick().await;
            loop {
                interval.tick().await;
                let store = stats_store.clone();
                match tokio::task::spawn_blocking(move || store.prune_expired_stats()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::error!(%error, "failed to prune statistics buckets"),
                    Err(error) => tracing::error!(%error, "statistics pruning task failed"),
                }
            }
        });
    }
}

pub fn build_app(state: Arc<AppState>) -> Router {
    let v1 = Router::new()
        .route("/models", get(models))
        .route("/chat/completions", post(chat_completions))
        .route("/responses", post(openai_responses))
        .route("/messages", post(anthropic_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .fallback(api_not_found);
    let api = Router::new()
        .route("/status", get(status))
        .route("/stats", get(stats))
        .route("/routing", get(routing))
        .route("/routing/stats", get(routing_stats))
        .route("/requests", get(requests))
        .route("/attempts", get(attempts))
        .fallback(api_not_found);
    Router::new()
        .route("/healthz", get(health))
        .nest("/v1", v1)
        .nest("/api", api)
        .fallback(serve_spa)
        .with_state(state)
}

async fn api_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status":"ok",
        "models":state.config.models.iter().map(|model|model.name.as_str()).collect::<Vec<_>>(),
        "uptime_seconds":(Utc::now().timestamp_millis()-state.started_at_ms)/1000
    }))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data = state
        .config
        .models
        .iter()
        .flat_map(|model| std::iter::once(&model.name).chain(model.aliases.iter()))
        .map(|name| json!({"id":name,"object":"model","created":0,"owned_by":"quotamux"}))
        .collect::<Vec<_>>();
    Json(json!({"object":"list","data":data}))
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

async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if has_provider_selector(&headers, &body) {
        return client_error(
            Protocol::AnthropicMessages,
            "clients cannot select a provider",
            Some("provider"),
        );
    }
    if let Err(error) = resolve_served_model(&state, Protocol::AnthropicMessages, &body) {
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
    if let Err(error) = resolve_served_model(&state, protocol, &body) {
        return validation_error(protocol, error);
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        handle_stream(state, protocol, headers, body).await
    } else {
        handle_nonstream(state, protocol, headers, body).await
    }
}

fn resolve_served_model(
    state: &AppState,
    protocol: Protocol,
    body: &Value,
) -> Result<ServedModelConfig, ValidationError> {
    let requested = protocol::model_name(body)?;
    let model = state.config.resolve_model(requested).ok_or_else(|| {
        ValidationError::invalid(format!("unsupported model {requested}"), Some("model"))
    })?;
    if !model.protocols.contains(&protocol) {
        return Err(ValidationError::invalid(
            format!(
                "model {requested} is not exposed through {}",
                protocol.as_str()
            ),
            Some("model"),
        ));
    }
    Ok(model.clone())
}

#[derive(Clone)]
struct RouteCandidate {
    target: RouteTargetConfig,
    layer_name: String,
    layer_index: usize,
    selection_reason: String,
    matched_prefix_bytes: Option<u64>,
    affinity_path: Option<FingerprintPath>,
    cache_domain: Option<CacheDomain>,
    probe: bool,
}

fn route_candidates(
    state: &AppState,
    model: &ServedModelConfig,
    ingress: Protocol,
    original: &Value,
) -> Vec<RouteCandidate> {
    let mut candidates = Vec::new();
    for (layer_index, layer) in model.layers.iter().enumerate() {
        let order = state.selector.order(layer.targets.len());
        let selection_reason = if layer.targets.len() == 1 {
            "single-target"
        } else {
            "random"
        };
        let mut layer_candidates = Vec::with_capacity(layer.targets.len());
        for target_index in order {
            let target = layer.targets[target_index].clone();
            layer_candidates.push(RouteCandidate {
                target,
                layer_name: layer.name.clone(),
                layer_index,
                selection_reason: selection_reason.into(),
                matched_prefix_bytes: None,
                affinity_path: None,
                cache_domain: None,
                probe: false,
            });
        }
        if layer.strategy == RouteStrategy::PromptPrefixAffinity && layer_candidates.len() > 1 {
            let now_ms = Utc::now().timestamp_millis();
            for candidate in &mut layer_candidates {
                let Some((path, domain)) =
                    affinity_context(state, &candidate.target, ingress, original)
                else {
                    continue;
                };
                candidate.matched_prefix_bytes = state
                    .affinity
                    .lookup(&path, &HashSet::from([domain.clone()]), now_ms)
                    .map(|found| found.matched_bytes);
                candidate.affinity_path = Some(path);
                candidate.cache_domain = Some(domain);
                if candidate.matched_prefix_bytes.is_some() {
                    candidate.selection_reason = "prompt-prefix-affinity".into();
                }
            }
            layer_candidates.sort_by(|left, right| {
                right
                    .matched_prefix_bytes
                    .unwrap_or(0)
                    .cmp(&left.matched_prefix_bytes.unwrap_or(0))
            });
        }
        candidates.extend(layer_candidates);
    }
    candidates
}

fn affinity_context(
    state: &AppState,
    target: &RouteTargetConfig,
    ingress: Protocol,
    original: &Value,
) -> Option<(FingerprintPath, CacheDomain)> {
    let runtime = target_runtime(state, target);
    let egress = runtime.client.protocol_for(ingress);
    let (_, _, prepared) =
        prepare_for_provider(egress, ingress, original, runtime.client.model()).ok()?;
    let bytes = canonical_prompt_bytes(prepared)?;
    let namespace = affinity_namespace(runtime.client.kind(), egress, runtime.client.model());
    let path = state
        .affinity
        .fingerprint(namespace.as_bytes(), [bytes.as_slice()]);
    let domain = CacheDomain {
        id: format!(
            "{}\0{}\0{}\0{}",
            target.provider,
            target.credential,
            target.model,
            egress.as_str()
        ),
        generation: "configured-target-v1".into(),
    };
    Some((path, domain))
}

fn affinity_namespace(kind: ProviderKind, egress: Protocol, model: &str) -> String {
    format!(
        "{}\0{}\0{}\0canonical-json-v1",
        kind.as_str(),
        egress.as_str(),
        model
    )
}

fn canonical_prompt_bytes(mut prepared: Value) -> Option<Vec<u8>> {
    let object = prepared.as_object_mut()?;
    object.remove("model");
    object.remove("stream");
    object.remove("stream_options");
    serde_json::to_vec(&prepared).ok()
}

fn target_runtime<'a>(state: &'a AppState, target: &RouteTargetConfig) -> &'a TargetRuntime {
    state.targets.get(target).expect("validated runtime target")
}

fn observe_affinity(state: &AppState, candidate: &RouteCandidate, usage: &Usage) {
    let (Some(path), Some(domain), Some(through_checkpoint)) = (
        candidate.affinity_path.clone(),
        candidate.cache_domain.clone(),
        candidate
            .affinity_path
            .as_ref()
            .and_then(FingerprintPath::deepest_ordinal),
    ) else {
        return;
    };
    let now_ms = Utc::now().timestamp_millis();
    let evidence = CacheEvidence {
        through_checkpoint,
        expires_at_ms: now_ms.saturating_add(state.affinity.success_ttl_ms()),
        cached_tokens: usage.cache_hit_tokens,
        confidence: if usage.cache_hit_tokens > 0 {
            EvidenceConfidence::ProviderReported
        } else {
            EvidenceConfidence::SuccessfulRequest
        },
    };
    if let Err(error) = state.affinity.observe(path, domain, evidence, now_ms) {
        tracing::warn!(%error, "failed to update in-memory affinity directory");
    }
}

fn terminal_route_failure(
    failure: Option<AttemptFailure>,
    candidate: Option<RouteCandidate>,
) -> (AttemptFailure, Option<RouteCandidate>) {
    let failure = failure.unwrap_or_else(|| AttemptFailure {
        error: ProviderError {
            class: FailureClass::FallbackUnavailable,
            status: Some(StatusCode::SERVICE_UNAVAILABLE),
            retry_after: None,
            safe_message: "no route target is currently available".into(),
        },
    });
    (failure, candidate)
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
    let served_model = resolve_served_model(&state, protocol, &body).expect("validated model");
    let candidates = route_candidates(&state, &served_model, protocol, &body);
    let mut fallback_reason = None;
    let mut last_failure = None;
    let mut last_candidate = None;
    let mut outcome = None;
    let mut sequence = 0_u32;
    for mut candidate in candidates {
        let runtime = target_runtime(&state, &candidate.target);
        match runtime.circuit.decide().await {
            RouteDecision::Primary { probe } => candidate.probe = probe,
            RouteDecision::Fallback { reason } => {
                fallback_reason = fallback_reason.or(reason);
                continue;
            }
        }
        sequence += 1;
        match execute_json_attempt(
            &state,
            &candidate,
            runtime,
            protocol,
            &headers,
            &body,
            &request_id,
            sequence,
            started,
        )
        .await
        {
            Ok(mut success) => {
                runtime.circuit.success().await;
                observe_affinity(
                    &state,
                    &success.candidate,
                    &usage_for(success.egress, &success.raw_upstream),
                );
                success.fallback_reason = fallback_reason;
                outcome = Some(success);
                break;
            }
            Err(failure) => {
                let allows_fallback = failure.error.class.allows_fallback();
                runtime
                    .circuit
                    .failure(failure.error.class, failure.error.retry_after)
                    .await;
                record_alert_if_needed(&state, &request_id, &candidate, failure.error.class).await;
                fallback_reason.get_or_insert(failure.error.class);
                last_candidate = Some(candidate.clone());
                last_failure = Some(failure);
                if !allows_fallback {
                    break;
                }
            }
        }
    }
    let Some(outcome) = outcome else {
        let (failure, candidate) = terminal_route_failure(last_failure, last_candidate);
        return terminal_failure(
            &state,
            protocol,
            protocol::model_name(&body).unwrap_or(&served_model.name),
            &served_model.name,
            &request_id,
            started_at_ms,
            started,
            request_bytes,
            failure,
            candidate.as_ref(),
            include_metadata,
            fallback_reason,
        )
        .await;
    };

    let response_bytes = serde_json::to_vec(&outcome.body).unwrap_or_default();
    let usage = usage_for(outcome.egress, &outcome.raw_upstream);
    let record = RequestRecord {
        id: request_id.clone(),
        started_at_ms,
        completed_at_ms: Utc::now().timestamp_millis(),
        protocol,
        requested_model: protocol::model_name(&body)
            .unwrap_or(&served_model.name)
            .into(),
        served_model: Some(served_model.name.clone()),
        upstream_model: Some(outcome.candidate.target.model.clone()),
        streaming: false,
        status: 200,
        error_class: None,
        provider: Some(outcome.candidate.target.provider.clone()),
        provider_kind: Some(outcome.provider_kind),
        credential: Some(outcome.candidate.target.credential.clone()),
        route_layer: Some(outcome.candidate.layer_name.clone()),
        route_layer_index: Some(outcome.candidate.layer_index),
        selection_reason: Some(outcome.candidate.selection_reason.clone()),
        matched_prefix_bytes: outcome.candidate.matched_prefix_bytes,
        fallback: outcome.candidate.layer_index > 0,
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
        &outcome.candidate,
        &outcome.candidate.target.model,
        outcome.fallback_reason,
        protocol,
        outcome.egress,
        outcome.translated,
    );
    response
}

struct JsonSuccess {
    candidate: RouteCandidate,
    provider_kind: ProviderKind,
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
    candidate: &RouteCandidate,
    runtime: &TargetRuntime,
    ingress: Protocol,
    headers: &HeaderMap,
    original: &Value,
    request_id: &str,
    sequence: u32,
    request_started: Instant,
) -> Result<JsonSuccess, AttemptFailure> {
    let attempt_id = Uuid::now_v7().to_string();
    let started_at_ms = Utc::now().timestamp_millis();
    let started = Instant::now();
    let provider_protocol = runtime.client.protocol_for(ingress);
    let (egress, translated, prepared) =
        match prepare_for_provider(provider_protocol, ingress, original, runtime.client.model()) {
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
    let response = match runtime.client.send(egress, &prepared, headers).await {
        Ok(response) => response,
        Err(error) => {
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                candidate,
                runtime.client.kind(),
                runtime.client.model(),
                egress,
                translated,
                candidate.probe,
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
                candidate,
                runtime.client.kind(),
                runtime.client.model(),
                egress,
                translated,
                candidate.probe,
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
                candidate,
                runtime.client.kind(),
                runtime.client.model(),
                egress,
                translated,
                candidate.probe,
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
        translate_nonstream(
            ingress,
            egress,
            &raw,
            protocol::model_name(original).unwrap_or(runtime.client.model()),
        )
    } else {
        raw.clone()
    };
    let usage = usage_for(egress, &raw);
    let cost = provider_cost(runtime.client.kind(), &raw, &usage);
    let record = AttemptRecord {
        id: attempt_id,
        request_id: Some(request_id.into()),
        sequence,
        provider: candidate.target.provider.clone(),
        provider_kind: Some(runtime.client.kind()),
        credential: Some(candidate.target.credential.clone()),
        route_layer: Some(candidate.layer_name.clone()),
        route_layer_index: Some(candidate.layer_index),
        selection_reason: Some(candidate.selection_reason.clone()),
        matched_prefix_bytes: candidate.matched_prefix_bytes,
        upstream_model: runtime.client.model().into(),
        egress_protocol: egress,
        translated,
        probe: candidate.probe,
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
        candidate: candidate.clone(),
        provider_kind: runtime.client.kind(),
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
    let served_model = resolve_served_model(&state, protocol, &body).expect("validated model");
    let candidates = route_candidates(&state, &served_model, protocol, &body);
    let mut fallback_reason = None;
    let mut last_failure = None;
    let mut last_candidate = None;
    let mut prepared = None;
    let mut sequence = 0_u32;
    for mut candidate in candidates {
        let runtime = state
            .targets
            .get(&candidate.target)
            .expect("validated runtime target")
            .clone();
        match runtime.circuit.decide().await {
            RouteDecision::Primary { probe } => candidate.probe = probe,
            RouteDecision::Fallback { reason } => {
                fallback_reason = fallback_reason.or(reason);
                continue;
            }
        }
        sequence += 1;
        match prepare_stream_attempt(
            state.clone(),
            &candidate,
            runtime.clone(),
            protocol,
            &headers,
            &body,
            &request_id,
            sequence,
            started,
        )
        .await
        {
            Ok(mut success) => {
                success.fallback_reason = fallback_reason;
                prepared = Some(success);
                break;
            }
            Err(failure) => {
                let allows_fallback = failure.error.class.allows_fallback();
                runtime
                    .circuit
                    .failure(failure.error.class, failure.error.retry_after)
                    .await;
                record_alert_if_needed(&state, &request_id, &candidate, failure.error.class).await;
                fallback_reason.get_or_insert(failure.error.class);
                last_candidate = Some(candidate.clone());
                last_failure = Some(failure);
                if !allows_fallback {
                    break;
                }
            }
        }
    }
    let Some(prepared) = prepared else {
        let (failure, candidate) = terminal_route_failure(last_failure, last_candidate);
        return terminal_failure(
            &state,
            protocol,
            protocol::model_name(&body).unwrap_or(&served_model.name),
            &served_model.name,
            &request_id,
            started_at_ms,
            started,
            request_bytes,
            failure,
            candidate.as_ref(),
            include_metadata,
            fallback_reason,
        )
        .await;
    };

    let candidate = prepared.candidate.clone();
    let provider_kind = prepared.runtime.client.kind();
    let egress = prepared.egress;
    let translated = prepared.translated;
    let fallback_reason = prepared.fallback_reason;
    let first_byte_ms = started.elapsed().as_millis() as u64;
    let state_for_stream = state.clone();
    let request_id_for_stream = request_id.clone();
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&served_model.name)
        .to_string();
    let configured_model = served_model.name.clone();
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
    let upstream_model = prepared.runtime.client.model().to_string();
    let runtime_for_stream = prepared.runtime.clone();
    let candidate_for_stream = candidate.clone();
    let stream = stream! {
        let mut response_bytes = 0_u64;
        let mut usage = Usage::default();
        let mut stream_error = None;
        let mut response_translator =
            responses::ChatToResponsesStream::new(requested_model.clone());
        let mut anthropic_translator = anthropic::ChatToAnthropicStream::new();
        let mut responses_source = responses::ResponsesToChatStream::new();
        let mut anthropic_source = anthropic::AnthropicToChatStream::new();
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
                            Ok(upstream_chunk) => {
                                let upstream_type=upstream_chunk.get("type").and_then(Value::as_str);
                                if matches!(upstream_type,Some("response.failed"|"error")) {
                                    stream_error=Some(FailureClass::StreamFailure);
                                    break;
                                }
                                let upstream_terminal = match egress {
                                    Protocol::OpenAiChat => false,
                                    Protocol::OpenAiResponses => matches!(upstream_type,Some("response.completed"|"response.incomplete")),
                                    Protocol::AnthropicMessages => upstream_type==Some("message_stop"),
                                };
                                let chat_chunks = match egress {
                                    Protocol::OpenAiChat => vec![upstream_chunk],
                                    Protocol::OpenAiResponses => responses_source.translate(&upstream_chunk),
                                    Protocol::AnthropicMessages => anthropic_source.translate(&upstream_chunk),
                                };
                                for chunk in chat_chunks {
                                    if chunk.get("usage").is_some_and(|value| !value.is_null()) { usage = Usage::from_openai(&chunk); }
                                    let events = match protocol {
                                        Protocol::OpenAiResponses => response_translator.translate(&chunk),
                                        Protocol::AnthropicMessages => anthropic_translator.translate(&chunk),
                                        Protocol::OpenAiChat => vec![SseEvent{event:None,data:chunk.to_string()}],
                                    };
                                    for event in events { let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes); }
                                }
                                if upstream_terminal {
                                    let tail = match protocol {
                                        Protocol::OpenAiResponses => response_translator.finish(),
                                        Protocol::AnthropicMessages => anthropic_translator.finish(),
                                        Protocol::OpenAiChat => vec![SseEvent{event:None,data:"[DONE]".into()}],
                                    };
                                    for event in tail { let bytes=event.encode(); response_bytes+=bytes.len() as u64; yield Ok::<Bytes, Infallible>(bytes); }
                                    break;
                                }
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
        let attempt = AttemptRecord {
            id:attempt_id,
            request_id:Some(request_id_for_stream.clone()),
            sequence,
            provider:candidate_for_stream.target.provider.clone(),
            provider_kind:Some(provider_kind),
            credential:Some(candidate_for_stream.target.credential.clone()),
            route_layer:Some(candidate_for_stream.layer_name.clone()),
            route_layer_index:Some(candidate_for_stream.layer_index),
            selection_reason:Some(candidate_for_stream.selection_reason.clone()),
            matched_prefix_bytes:candidate_for_stream.matched_prefix_bytes,
            upstream_model:upstream_model.clone(),
            egress_protocol:egress,
            translated,
            probe,
            started_at_ms:attempt_started_at_ms,
            completed_at_ms,
            status:Some(200),
            error_class:stream_error,
            retry_after_ms:None,
            committed:true,
            request_bytes:upstream_request_bytes,
            response_bytes,
            first_byte_ms:Some(first_byte_ms),
            total_ms:started.elapsed().as_millis() as u64,
            usage:usage.clone(),
            provider_cost_usd:provider_cost(provider_kind,&Value::Null,&usage),
            sanitized_error:stream_error.map(|_|"upstream stream ended unexpectedly".into())
        };
        persist_attempt(&state_for_stream,&attempt);
        let request = RequestRecord {
            id:request_id_for_stream.clone(),
            started_at_ms,
            completed_at_ms,
            protocol,
            requested_model,
            served_model:Some(configured_model),
            upstream_model:Some(upstream_model),
            streaming:true,
            status:if stream_error.is_some(){502}else{200},
            error_class:stream_error,
            provider:Some(candidate_for_stream.target.provider.clone()),
            provider_kind:Some(provider_kind),
            credential:Some(candidate_for_stream.target.credential.clone()),
            route_layer:Some(candidate_for_stream.layer_name.clone()),
            route_layer_index:Some(candidate_for_stream.layer_index),
            selection_reason:Some(candidate_for_stream.selection_reason.clone()),
            matched_prefix_bytes:candidate_for_stream.matched_prefix_bytes,
            fallback:candidate_for_stream.layer_index>0,
            translated,
            request_bytes,
            response_bytes,
            first_byte_ms:Some(first_byte_ms),
            total_ms:started.elapsed().as_millis() as u64,
            usage:usage.clone(),
            claude_session_id,
            claude_agent_id,
            claude_parent_agent_id
        };
        persist_request(&state_for_stream,&request);
        if let Some(class)=stream_error {
            runtime_for_stream.circuit.failure(class,None).await;
            record_alert_if_needed(&state_for_stream,&request_id_for_stream,&candidate_for_stream,class).await;
        } else {
            observe_affinity(&state_for_stream,&candidate_for_stream,&usage);
            runtime_for_stream.circuit.success().await;
        }
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
        &candidate,
        &candidate.target.model,
        fallback_reason,
        protocol,
        egress,
        translated,
    );
    response
}

struct PreparedStream {
    candidate: RouteCandidate,
    runtime: Arc<TargetRuntime>,
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
    candidate: &RouteCandidate,
    runtime: Arc<TargetRuntime>,
    ingress: Protocol,
    headers: &HeaderMap,
    original: &Value,
    request_id: &str,
    sequence: u32,
    request_started: Instant,
) -> Result<PreparedStream, AttemptFailure> {
    let attempt_id = Uuid::now_v7().to_string();
    let attempt_started_at_ms = Utc::now().timestamp_millis();
    let attempt_started = Instant::now();
    let provider_protocol = runtime.client.protocol_for(ingress);
    let (egress, translated, prepared) =
        match prepare_for_provider(provider_protocol, ingress, original, runtime.client.model()) {
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
    let response = match runtime.client.send(egress, &prepared, headers).await {
        Ok(response) => response,
        Err(error) => {
            let record = failed_attempt(
                &attempt_id,
                Some(request_id),
                sequence,
                candidate,
                runtime.client.kind(),
                runtime.client.model(),
                egress,
                translated,
                candidate.probe,
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
                candidate,
                runtime.client.kind(),
                runtime.client.model(),
                egress,
                translated,
                candidate.probe,
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
            .is_some_and(|value| {
                value.get("error").is_some()
                    || matches!(
                        value.get("type").and_then(Value::as_str),
                        Some("response.failed" | "error")
                    )
            })
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
            candidate,
            runtime.client.kind(),
            runtime.client.model(),
            egress,
            translated,
            candidate.probe,
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
        candidate: candidate.clone(),
        runtime,
        egress,
        translated,
        decoder,
        first_event: event,
        attempt_id,
        sequence,
        probe: candidate.probe,
        attempt_started_at_ms,
        request_bytes,
        fallback_reason: None,
    })
}

fn prepare_for_provider(
    egress: Protocol,
    ingress: Protocol,
    original: &Value,
    model: &str,
) -> Result<(Protocol, bool, Value), ValidationError> {
    if egress == ingress {
        let direct = match ingress {
            Protocol::OpenAiChat => chat::prepare(original.clone(), model)?,
            Protocol::OpenAiResponses => responses::prepare_direct(original.clone(), model)?,
            Protocol::AnthropicMessages => anthropic::prepare_direct(original.clone(), model)?,
        };
        return Ok((egress, false, direct));
    }
    let canonical_chat = match ingress {
        Protocol::OpenAiChat => chat::prepare(original.clone(), model)?,
        Protocol::OpenAiResponses => responses::prepare_for_chat(original.clone(), model)?,
        Protocol::AnthropicMessages => anthropic::prepare_for_chat(original.clone(), model)?,
    };
    let prepared = match egress {
        Protocol::OpenAiChat => canonical_chat,
        Protocol::OpenAiResponses => responses::prepare_from_chat(canonical_chat, model)?,
        Protocol::AnthropicMessages => anthropic::prepare_from_chat(canonical_chat, model)?,
    };
    Ok((egress, true, prepared))
}

fn translate_nonstream(
    ingress: Protocol,
    egress: Protocol,
    raw: &Value,
    response_model: &str,
) -> Value {
    if ingress == egress {
        return raw.clone();
    }
    let chat = match egress {
        Protocol::OpenAiChat => raw.clone(),
        Protocol::OpenAiResponses => responses::response_to_chat(raw),
        Protocol::AnthropicMessages => anthropic::message_to_chat(raw),
    };
    match ingress {
        Protocol::OpenAiChat => chat,
        Protocol::OpenAiResponses => responses::chat_to_response(&chat, response_model),
        Protocol::AnthropicMessages => anthropic::chat_to_message(&chat),
    }
}

#[allow(clippy::too_many_arguments)]
async fn terminal_failure(
    state: &Arc<AppState>,
    protocol: Protocol,
    requested_model: &str,
    served_model: &str,
    request_id: &str,
    started_at_ms: i64,
    started: Instant,
    request_bytes: u64,
    failure: AttemptFailure,
    candidate: Option<&RouteCandidate>,
    include_metadata: bool,
    fallback_reason: Option<FailureClass>,
) -> Response {
    let class = if fallback_reason.is_some() && failure.error.class.allows_fallback() {
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
        requested_model: requested_model.into(),
        served_model: Some(served_model.into()),
        upstream_model: candidate.map(|candidate| candidate.target.model.clone()),
        streaming: false,
        status: status.as_u16(),
        error_class: Some(class),
        provider: candidate.map(|candidate| candidate.target.provider.clone()),
        provider_kind: candidate
            .map(|candidate| target_runtime(state, &candidate.target).client.kind()),
        credential: candidate.map(|candidate| candidate.target.credential.clone()),
        route_layer: candidate.map(|candidate| candidate.layer_name.clone()),
        route_layer_index: candidate.map(|candidate| candidate.layer_index),
        selection_reason: candidate.map(|candidate| candidate.selection_reason.clone()),
        matched_prefix_bytes: candidate.and_then(|candidate| candidate.matched_prefix_bytes),
        fallback: candidate.is_some_and(|candidate| candidate.layer_index > 0),
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
    if let Some(candidate) = candidate {
        record_alert_if_needed(state, request_id, candidate, class).await;
    }
    let body = error_body(protocol, &failure.error.safe_message, class);
    let mut response = (status, Json(body)).into_response();
    if let Some(candidate) = candidate {
        let egress = target_runtime(state, &candidate.target)
            .client
            .protocol_for(protocol);
        apply_metadata(
            &mut response,
            include_metadata,
            request_id,
            candidate,
            &candidate.target.model,
            fallback_reason,
            protocol,
            egress,
            protocol != egress,
        );
    }
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
    candidate: &RouteCandidate,
    provider_kind: ProviderKind,
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
        provider: candidate.target.provider.clone(),
        provider_kind: Some(provider_kind),
        credential: Some(candidate.target.credential.clone()),
        route_layer: Some(candidate.layer_name.clone()),
        route_layer_index: Some(candidate.layer_index),
        selection_reason: Some(candidate.selection_reason.clone()),
        matched_prefix_bytes: candidate.matched_prefix_bytes,
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

fn provider_cost(provider: ProviderKind, raw: &Value, usage: &Usage) -> Option<f64> {
    if matches!(
        provider,
        ProviderKind::OpenCodeGo | ProviderKind::OpenCodeZen
    ) {
        return raw
            .get("cost")
            .and_then(Value::as_f64)
            .or_else(|| raw.get("cost").and_then(Value::as_str)?.parse().ok());
    }
    if provider == ProviderKind::DeepSeekOfficial {
        return Some(
            (usage.cache_hit_tokens as f64 * 0.0028
                + usage.cache_miss_tokens as f64 * 0.14
                + usage.output_tokens as f64 * 0.28)
                / 1_000_000.0,
        );
    }
    None
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

async fn record_alert_if_needed(
    state: &AppState,
    request_id: &str,
    candidate: &RouteCandidate,
    class: FailureClass,
) {
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
    let runtime = target_runtime(state, &candidate.target);
    let snapshot = runtime.circuit.snapshot().await;
    let record = AlertRecord {
        id: format!(
            "{}:{}:{}:{}",
            candidate.target.provider,
            candidate.target.credential,
            candidate.target.model,
            class.as_str()
        ),
        provider: candidate.target.provider.clone(),
        provider_kind: Some(runtime.client.kind()),
        credential: Some(candidate.target.credential.clone()),
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
    candidate: &RouteCandidate,
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
        ("x-relay-provider", candidate.target.provider.clone()),
        ("x-relay-credential", candidate.target.credential.clone()),
        ("x-relay-route-layer", candidate.layer_name.clone()),
        (
            "x-relay-route-layer-index",
            candidate.layer_index.to_string(),
        ),
        (
            "x-relay-selection-reason",
            candidate.selection_reason.clone(),
        ),
        ("x-relay-upstream-model", model.into()),
        (
            "x-relay-fallback",
            if candidate.layer_index > 0 {
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
    if let Some(bytes) = candidate.matched_prefix_bytes
        && let Ok(value) = HeaderValue::from_str(&bytes.to_string())
    {
        headers.insert("x-relay-matched-prefix-bytes", value);
    }
}

#[derive(Deserialize)]
struct RequestPageQuery {
    limit: Option<usize>,
    before: Option<String>,
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}
async fn requests(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RequestPageQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100).min(1000);
    match state.store.request_page(limit, query.before.as_deref()) {
        Ok(page) => Json(json!({
            "requests":page.requests,
            "next_cursor":page.next_cursor,
            "limit":limit,
        }))
        .into_response(),
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

async fn routing(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut models = Vec::with_capacity(state.config.models.len());
    for model in &state.config.models {
        let mut layers = Vec::with_capacity(model.layers.len());
        for (layer_index, layer) in model.layers.iter().enumerate() {
            let mut targets = Vec::with_capacity(layer.targets.len());
            for target in &layer.targets {
                targets.push(json!({
                    "provider": target.provider,
                    "credential": target.credential,
                    "upstream_model": target.model,
                    "circuit": target_runtime(&state, target).circuit.snapshot().await,
                }));
            }
            layers.push(json!({
                "index": layer_index,
                "name": layer.name,
                "strategy": layer.strategy.as_str(),
                "targets": targets,
            }));
        }
        models.push(json!({
            "name": model.name,
            "aliases": model.aliases,
            "layers": layers,
        }));
    }
    Json(json!({"models": models}))
}

#[derive(Deserialize)]
struct RoutingStatsQuery {
    model: String,
    window: String,
}

async fn routing_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RoutingStatsQuery>,
) -> Response {
    let Some(model) = state.config.resolve_model(&query.model) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unknown served model","code":"unknown_model"})),
        )
            .into_response();
    };
    let now = Utc::now().timestamp_millis();
    let (window_id, from_ms, to_ms) = match routing_window(&query.window, now) {
        Ok(window) => window,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":error,"code":"invalid_window"})),
            )
                .into_response();
        }
    };
    let rollup = match state.store.routing_rollup(&model.name, from_ms, to_ms) {
        Ok(rollup) => rollup,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":error,"code":"statistics_query_failed"})),
            )
                .into_response();
        }
    };
    let mut remaining = rollup.targets;
    let mut layers = Vec::with_capacity(model.layers.len());
    for (layer_index, layer) in model.layers.iter().enumerate() {
        let mut layer_total = RequestRollup::default();
        let mut targets = Vec::with_capacity(layer.targets.len());
        for target in &layer.targets {
            let key = RouteRollupKey {
                layer_index: layer_index as u32,
                layer_name: layer.name.clone(),
                provider: target.provider.clone(),
                credential: target.credential.clone(),
                upstream_model: target.model.clone(),
            };
            let value = remaining.remove(&key).unwrap_or_default();
            if let Err(error) = layer_total.checked_add_assign(value) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":error,"code":"statistics_overflow"})),
                )
                    .into_response();
            }
            targets.push(json!({
                "provider": target.provider,
                "credential": target.credential,
                "upstream_model": target.model,
                "configured": true,
                "totals": request_metric_json(value),
            }));
        }
        layers.push(json!({
            "index": layer_index,
            "name": layer.name,
            "strategy": layer.strategy.as_str(),
            "totals": request_metric_json(layer_total),
            "targets": targets,
        }));
    }
    let historical_targets = remaining
        .into_iter()
        .filter(|(_, value)| value.calls > 0 || value.total_tokens > 0)
        .map(|(key, value)| {
            json!({
                "layer_index": key.layer_index,
                "layer_name": key.layer_name,
                "provider": key.provider,
                "credential": key.credential,
                "upstream_model": key.upstream_model,
                "configured": false,
                "totals": request_metric_json(value),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "model": {"name":model.name,"aliases":model.aliases},
        "window": {"id":window_id,"from_ms":from_ms,"to_ms":to_ms},
        "totals": request_metric_json(rollup.total),
        "layers": layers,
        "historical_targets": historical_targets,
        "unattributed": request_metric_json(rollup.unattributed),
    }))
    .into_response()
}

fn routing_window(
    window: &str,
    now_ms: i64,
) -> Result<(&'static str, Option<i64>, Option<i64>), String> {
    let to_ms = now_ms.div_euclid(60_000) * 60_000;
    let normalized = window.trim().to_ascii_lowercase();
    let (id, duration_ms) = match normalized.as_str() {
        "1h" => ("1H", Some(60 * 60_000)),
        "1d" => ("1D", Some(24 * 60 * 60_000)),
        "1w" => ("1W", Some(7 * 24 * 60 * 60_000)),
        "1m" => ("1M", Some(30 * 24 * 60 * 60_000)),
        "all" => return Ok(("All", None, None)),
        _ => return Err("window must be one of 1h, 1d, 1w, 1m, or all".into()),
    };
    Ok((
        id,
        duration_ms.map(|duration| to_ms - duration),
        Some(to_ms),
    ))
}

fn request_metric_json(value: RequestRollup) -> Value {
    json!({
        "calls": value.calls,
        "input_tokens": value.input_tokens,
        "output_tokens": value.output_tokens,
        "total_tokens": value.total_tokens,
    })
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let alerts = state.store.alerts(100).unwrap_or_default();
    let mut target_rows = Vec::new();
    let mut seen = HashSet::new();
    for model in &state.config.models {
        for layer in &model.layers {
            for target in &layer.targets {
                if seen.insert(target) {
                    let runtime = target_runtime(&state, target);
                    target_rows.push(json!({
                        "provider":target.provider,
                        "provider_kind":runtime.client.kind().as_str(),
                        "credential":target.credential,
                        "model":target.model,
                        "protocols":runtime.client.protocols().iter().map(|protocol|protocol.as_str()).collect::<Vec<_>>(),
                        "endpoint":safe_endpoint(runtime.client.endpoint()),
                        "circuit":runtime.circuit.snapshot().await,
                    }));
                }
            }
        }
    }
    Json(json!({
        "models":state.config.models.iter().map(|model|model.name.as_str()).collect::<Vec<_>>(),
        "targets":target_rows,
        "affinity":{
            "storage":"memory-only",
            "leases":state.affinity.lease_count(),
            "max_leases":state.config.affinity.max_leases,
            "checkpoint_bytes":state.config.affinity.checkpoint_bytes,
            "max_checkpoints_per_path":state.config.affinity.max_checkpoints_per_path,
            "max_candidates_per_prefix":state.config.affinity.max_candidates_per_prefix,
            "success_ttl_ms":state.config.affinity.success_ttl_ms,
        },
        "alerts":alerts
    }))
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
    let rollup = match state.store.overview_rollup() {
        Ok(rollup) => rollup,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":error})),
            )
                .into_response();
        }
    };
    let mut providers: BTreeMap<String, Value> = BTreeMap::new();
    let provider_names = state
        .config
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .chain(rollup.attempts.keys().map(|(provider, _)| provider.clone()))
        .collect::<std::collections::BTreeSet<_>>();
    for provider in provider_names {
        let rows = rollup
            .attempts
            .iter()
            .filter(|((row_provider, _), _)| row_provider == &provider)
            .collect::<Vec<_>>();
        let models = state
            .config
            .providers
            .iter()
            .filter(|configured| configured.id == provider)
            .flat_map(|configured| configured.models.iter().map(|model| model.name.as_str()))
            .chain(
                rows.iter()
                    .map(|((_, upstream_model), _)| upstream_model.as_str()),
            )
            .collect::<std::collections::BTreeSet<_>>();
        let served_models = state
            .config
            .models
            .iter()
            .filter(|model| {
                model.layers.iter().any(|layer| {
                    layer
                        .targets
                        .iter()
                        .any(|target| target.provider == provider)
                })
            })
            .map(|model| model.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut aggregate = AttemptRollup::default();
        for (_, value) in rows {
            if let Err(error) = aggregate.checked_add_assign(*value) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":error})),
                )
                    .into_response();
            }
        }
        let mut summary = attempt_rollup_json(aggregate);
        let summary = summary.as_object_mut().expect("attempt rollup object");
        summary.insert("models".into(), json!(models));
        summary.insert("served_models".into(), json!(served_models));
        providers.insert(provider, Value::Object(summary.clone()));
    }
    let routes = rollup
        .attempts
        .iter()
        .map(|((provider, upstream_model), value)| {
            let served_models = state
                .config
                .models
                .iter()
                .filter(|model| {
                    model.layers.iter().any(|layer| {
                        layer.targets.iter().any(|target| {
                            target.provider.as_str() == provider.as_str()
                                && target.model.as_str() == upstream_model.as_str()
                        })
                    })
                })
                .map(|model| model.name.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut summary = attempt_rollup_json(*value);
            let summary = summary.as_object_mut().expect("attempt rollup object");
            summary.insert("served_models".into(), json!(served_models));
            summary.insert("provider".into(), Value::String(provider.clone()));
            summary.insert(
                "upstream_model".into(),
                Value::String(upstream_model.clone()),
            );
            Value::Object(summary.clone())
        })
        .collect::<Vec<_>>();
    let requests = rollup.requests;
    Json(json!({
        "requests":{
            "total":requests.calls,
            "errors":requests.errors,
            "fallbacks":requests.fallbacks,
            "output_tokens":requests.output_tokens,
            "bytes":requests.bytes,
        },
        "providers":providers,
        "routes":routes
    }))
    .into_response()
}

fn attempt_rollup_json(value: AttemptRollup) -> Value {
    json!({
        "attempts":value.attempts,
        "successes":value.successes,
        "errors":value.errors,
        "cache_reported_attempts":value.cache_reported_attempts,
        "cache_hit_tokens":value.cache_hit_tokens,
        "cache_miss_tokens":value.cache_miss_tokens,
        "cost_usd":value.cost_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_canonicalization_ignores_transport_and_model_fields() {
        let baseline = canonical_prompt_bytes(json!({
            "model":"provider-model-a",
            "messages":[{"role":"user","content":"same prompt"}],
            "stream":false
        }))
        .unwrap();
        let streaming = canonical_prompt_bytes(json!({
            "model":"provider-model-b",
            "messages":[{"role":"user","content":"same prompt"}],
            "stream":true,
            "stream_options":{"include_usage":true}
        }))
        .unwrap();
        assert_eq!(baseline, streaming);
    }

    #[test]
    fn affinity_canonicalization_preserves_prompt_semantics() {
        let baseline = canonical_prompt_bytes(json!({
            "model":"provider-model",
            "messages":[{"role":"user","content":"same prompt"}],
            "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]
        }))
        .unwrap();
        for changed in [
            json!({
                "model":"provider-model",
                "messages":[{"role":"assistant","content":"same prompt"}],
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]
            }),
            json!({
                "model":"provider-model",
                "messages":[{"role":"user","content":" same prompt"}],
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]
            }),
            json!({
                "model":"provider-model",
                "messages":[{"role":"user","content":"same prompt"}],
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}]
            }),
        ] {
            assert_ne!(baseline, canonical_prompt_bytes(changed).unwrap());
        }
    }

    #[test]
    fn affinity_namespaces_isolate_provider_kind_protocol_and_upstream_model() {
        let go_k3 = affinity_namespace(ProviderKind::OpenCodeGo, Protocol::OpenAiChat, "kimi-k3");
        assert_ne!(
            go_k3,
            affinity_namespace(ProviderKind::KimiCode, Protocol::OpenAiChat, "k3")
        );
        assert_ne!(
            go_k3,
            affinity_namespace(
                ProviderKind::OpenCodeGo,
                Protocol::AnthropicMessages,
                "kimi-k3"
            )
        );
        assert_ne!(
            go_k3,
            affinity_namespace(
                ProviderKind::OpenCodeGo,
                Protocol::OpenAiChat,
                "kimi-k3-256k"
            )
        );
    }

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
            (provider_cost(ProviderKind::DeepSeekOfficial, &Value::Null, &usage).unwrap() - 0.0028)
                .abs()
                < 1e-9
        );
    }
}
