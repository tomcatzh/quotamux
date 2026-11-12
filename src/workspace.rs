//! One routing document, one active request runtime, one save transaction.
use crate::{AppState, Config, build_app, provider::BackendClient, types::Protocol};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;
use tower::ServiceExt;

type Error = Box<dyn std::error::Error + Send + Sync>;

struct Revision {
    id: String,
    document: Config,
    state: Arc<AppState>,
    router: Router,
    file_hash: blake3::Hash,
    migration: Option<Value>,
}

pub struct Workspace {
    path: PathBuf,
    active: RwLock<Arc<Revision>>,
    saving: Mutex<()>,
    background: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        for task in &self.background {
            task.abort();
        }
    }
}

impl Workspace {
    pub async fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, Error> {
        let path = fs::canonicalize(path)?;
        let text = fs::read_to_string(&path)?;
        let mut document: Config = toml::from_str(&text).map_err(|mut error| {
            error.set_input(None);
            error
        })?;
        let upgraded = document.upgrade()?;
        let state = Arc::new(AppState::new(document.for_runtime(&path)).await?);
        let mut file_hash = blake3::hash(text.as_bytes());
        let migration = if upgraded {
            let normalized = toml::to_string_pretty(&document)?;
            let persisted = migrate_file(&path, text.as_bytes(), normalized.as_bytes()).is_ok();
            if persisted {
                file_hash = blake3::hash(normalized.as_bytes());
            } else {
                tracing::warn!(
                    "legacy configuration migrated in memory; file migration awaits writable configuration directory"
                );
            }
            Some(json!({"from":3,"to":4,"persisted":persisted}))
        } else {
            None
        };
        let background = state.start_background();
        let router = build_app(state.clone());
        let workspace = Arc::new(Self {
            path,
            saving: Mutex::new(()),
            background,
            active: RwLock::new(Arc::new(Revision {
                id: uuid::Uuid::now_v7().to_string(),
                document,
                state,
                router,
                file_hash,
                migration,
            })),
        });
        let weak = Arc::downgrade(&workspace);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let Some(workspace) = weak.upgrade() else {
                    break;
                };
                workspace
                    .revision()
                    .state
                    .affinity
                    .expire(Utc::now().timestamp_millis());
            }
        });
        Ok(workspace)
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/routing", get(read).put(save))
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .fallback(dispatch)
            .with_state(self.clone())
    }

    fn revision(&self) -> Arc<Revision> {
        self.active.read().unwrap().clone()
    }

    async fn view(&self, revision: Arc<Revision>, at: DateTime<Utc>) -> Value {
        let mut document = serde_json::to_value(&revision.document).unwrap();
        for backend in document["backends"].as_array_mut().unwrap() {
            if backend["endpoint"]
                .as_str()
                .and_then(|value| reqwest::Url::parse(value).ok())
                .is_some_and(|url| {
                    !url.username().is_empty()
                        || url.password().is_some()
                        || url.query().is_some()
                        || url.fragment().is_some()
                })
            {
                backend["endpoint"] = Value::Null;
            }
            for credential in backend["credentials"].as_array_mut().unwrap() {
                credential["api_key"] = json!("");
            }
        }
        let models = revision.state.routing_rows(at).await;
        let pricing = revision.document.backends.iter().flat_map(|backend| backend.models.iter().map(|model| {
            let (active_window, rates) = crate::pricing::Tariff::new(model).expect("validated pricing").at(at);
            json!({"backend":backend.id,"model":model.name,"active_window":active_window,"rates":rates})
        })).collect::<Vec<_>>();
        let pools = revision
            .document
            .capacity_pools
            .iter()
            .filter_map(|p| revision.state.admission.snapshot(&p.id))
            .collect::<Vec<_>>();
        json!({"revision":revision.id,"document":document,"models":models,"pools":pools,"pricing":pricing,"now":at.to_rfc3339(),"migration":revision.migration})
    }

    async fn commit(&self, input: Save) -> Result<Arc<Revision>, SaveError> {
        let _save = self.saving.lock().await;
        let current = self.revision();
        if input.revision != current.id {
            return Err(conflict(
                "Routing changed. Reload the active revision before saving.",
            ));
        }
        let mut candidate = input.document;
        if candidate.server.listen != current.document.server.listen
            || candidate.server.data_dir != current.document.server.data_dir
        {
            return Err(invalid("Listener and data directory are startup settings."));
        }
        for backend in &mut candidate.backends {
            if backend.endpoint.as_deref().is_none_or(str::is_empty)
                && let Some(existing) = current
                    .document
                    .backend(&backend.id)
                    .filter(|old| old.adapter == backend.adapter)
            {
                backend.endpoint = existing.endpoint.clone();
            }
            for credential in &mut backend.credentials {
                if credential.api_key.is_empty()
                    && let Some(existing) = current
                        .document
                        .backend(&backend.id)
                        .filter(|b| b.adapter == backend.adapter)
                        .and_then(|b| b.credential(&credential.id))
                {
                    credential.api_key = existing.api_key.clone();
                }
            }
        }
        candidate.upgrade().map_err(|e| invalid(&e.to_string()))?;
        let mut effective = candidate.clone();
        effective.server.data_dir = current.state.config.server.data_dir.clone();
        let runtime = Arc::new(
            AppState::revise(effective, &current.state)
                .await
                .map_err(|_| invalid("Unable to construct candidate routing clients."))?,
        );
        // Validate every new or changed account/model binding, including ones not yet routed.
        for backend in &candidate.backends {
            for credential in &backend.credentials {
                for model in &backend.models {
                    let unchanged = serde_json::to_value(current.document.server.timeouts).unwrap()
                        == serde_json::to_value(candidate.server.timeouts).unwrap()
                        && current.document.backend(&backend.id).is_some_and(|old| {
                            old.adapter == backend.adapter
                                && old.endpoint == backend.endpoint
                                && old.region == backend.region
                                && old
                                    .credential(&credential.id)
                                    .is_some_and(|c| c.api_key == credential.api_key)
                                && old
                                    .model(&model.name)
                                    .is_some_and(|m| m.protocols == model.protocols)
                        });
                    if unchanged {
                        continue;
                    }
                    let _lease = if let Some(pool) = credential
                        .pool
                        .as_deref()
                        .filter(|id| current.state.admission.snapshot(id).is_some())
                    {
                        Some(current.state.admission.acquire_account(&crate::admission::account_key(&backend.id,&credential.id),Some(pool)).ok_or_else(||conflict("An account is busy. Retry saving after an active request completes."))?)
                    } else {
                        None
                    };
                    let client = BackendClient::new_with_timeouts(
                        backend,
                        credential,
                        model,
                        candidate.server.timeouts,
                    )
                    .map_err(|_| invalid("Unable to construct an account validation client."))?;
                    probe(&client).await.map_err(|message| {
                        invalid(&format!(
                            "Account {} / {}, model {}: {message}",
                            backend.id, credential.id, model.name
                        ))
                    })?;
                }
            }
        }
        let text = toml::to_string_pretty(&candidate)
            .map_err(|_| invalid("Unable to serialize routing configuration."))?;
        let router = build_app(runtime.clone());
        let next = Arc::new(Revision {
            id: uuid::Uuid::now_v7().to_string(),
            document: candidate,
            state: runtime,
            router,
            file_hash: blake3::hash(text.as_bytes()),
            migration: None,
        });
        // No await after taking the publication lock: readers see one complete revision.
        let mut active = self.active.write().unwrap();
        let on_disk =
            fs::read(&self.path).map_err(|_| storage("Cannot read the configuration file."))?;
        if blake3::hash(&on_disk) != current.file_hash {
            return Err(conflict(
                "The configuration file changed externally. Restart to load it before editing.",
            ));
        }
        persist(&self.path, text.as_bytes()).map_err(|_| {
            storage("Cannot persist configuration. Active routing was not changed.")
        })?;
        next.state.admission.publish_config(&next.document);
        *active = next.clone();
        Ok(next)
    }
}

async fn probe(client: &BackendClient) -> Result<(), &'static str> {
    let protocol = client.protocols()[0];
    let body = match protocol {
        Protocol::OpenAiChat => {
            json!({"model":client.model(),"messages":[{"role":"user","content":"Reply OK."}],"max_tokens":8,"stream":false})
        }
        Protocol::OpenAiResponses => {
            json!({"model":client.model(),"input":"Reply OK.","max_output_tokens":16,"stream":false})
        }
        Protocol::AnthropicMessages => {
            json!({"model":client.model(),"messages":[{"role":"user","content":"Reply OK."}],"max_tokens":8,"stream":false})
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let response = client
            .send(protocol, &body, &HeaderMap::new())
            .await
            .map_err(|_| "Upstream credential/model check failed.")?;
        let value: Value = response
            .json()
            .await
            .map_err(|_| "Upstream did not return a valid JSON response.")?;
        if value.get("error").is_some_and(|e| !e.is_null()) {
            return Err("Upstream returned an error.");
        }
        let valid = match protocol {
            Protocol::OpenAiChat => value["choices"].as_array().is_some_and(|a| !a.is_empty()),
            Protocol::OpenAiResponses => value["output"].is_array(),
            Protocol::AnthropicMessages => value["content"].is_array(),
        };
        if !valid {
            return Err("Upstream response did not match its protocol.");
        }
        Ok(())
    })
    .await
    .map_err(|_| "Upstream check timed out.")?
}

fn migrate_file(path: &Path, original: &[u8], normalized: &[u8]) -> std::io::Result<()> {
    // Preserve the valid pre-upgrade document for an operational downgrade.
    let backup = path.with_extension("toml.v3.bak");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&backup) {
        Ok(mut file) => {
            file.write_all(original)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if fs::read(path)? != original {
        return Err(std::io::Error::other(
            "configuration changed during migration",
        ));
    }
    persist(path, normalized)
}

fn persist(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing configuration directory"))?;
    let temporary = parent.join(format!(".routing-{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        // Once rename succeeds the new file is authoritative; publication cannot fail.
        if fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            tracing::warn!("routing saved; directory durability confirmation unavailable");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    revision: String,
    document: Config,
}
#[derive(Deserialize)]
struct Preview {
    at: Option<DateTime<Utc>>,
}
type SaveError = (StatusCode, Json<Value>);
fn invalid(message: &str) -> SaveError {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"error":message})),
    )
}
fn conflict(message: &str) -> SaveError {
    (StatusCode::CONFLICT, Json(json!({"error":message})))
}
fn storage(message: &str) -> SaveError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error":message})),
    )
}

async fn read(State(workspace): State<Arc<Workspace>>, Query(query): Query<Preview>) -> Response {
    let mut response = Json(
        workspace
            .view(workspace.revision(), query.at.unwrap_or_else(Utc::now))
            .await,
    )
    .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

async fn save(
    State(workspace): State<Arc<Workspace>>,
    headers: HeaderMap,
    input: Result<Json<Save>, JsonRejection>,
) -> Response {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let trusted = origin
            .to_str()
            .ok()
            .and_then(|o| reqwest::Url::parse(o).ok())
            .is_some_and(|url| {
                let prefix = format!("{}://", url.scheme());
                let authority = url
                    .as_str()
                    .strip_prefix(&prefix)
                    .unwrap_or_default()
                    .trim_end_matches('/');
                headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(authority)
                    && matches!(url.scheme(), "http" | "https")
            });
        if !trusted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Foreign browser origin rejected."})),
            )
                .into_response();
        }
    }
    let Ok(Json(input)) = input else {
        return invalid("Invalid routing document.").into_response();
    };
    match workspace.commit(input).await {
        Ok(revision) => Json(workspace.view(revision, Utc::now()).await).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn dispatch(State(workspace): State<Arc<Workspace>>, request: Request) -> Response {
    let revision = workspace.revision();
    let mut response = revision.router.clone().oneshot(request).await.unwrap();
    response
        .headers_mut()
        .insert("x-quotamux-revision", revision.id.parse().unwrap());
    response
}
