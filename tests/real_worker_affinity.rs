use std::{env, fs, path::PathBuf, sync::Arc, time::Duration};

use axum::Router;
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{
        AdapterKind, BackendConfig, BackendModelConfig, CredentialConfig, RouteLayerConfig,
        RouteStrategy, RouteTargetConfig, ServedModelConfig, ServerConfig,
    },
    provider::BackendClient,
    types::{Protocol, Usage},
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires a local private config and makes two real provider requests"]
async fn mixed_go_and_deepseek_workers_keep_a_divergent_branch_on_the_warm_target() {
    let private_config = env::var_os("QUOTAMUX_REAL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quotamux.toml"));
    let data_dir = TempDir::new().expect("temporary real-probe data directory");
    let (config, served_model) = real_probe_config(&private_config, &data_dir);
    let state = Arc::new(
        AppState::new_with_random_seed(config, 0x5eed_aff1)
            .await
            .expect("real-probe AppState"),
    );
    let (base_url, server) = start_gateway(build_app(state)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("real-probe HTTP client");
    let common = "QuotaMux real prefix affinity context. ".repeat(140);

    let first = send_probe(
        &client,
        &base_url,
        &served_model,
        &format!("{common}\nFirst branch. Reply with OK."),
    )
    .await;
    let warm_provider = header(&first, "x-relay-backend");
    assert_eq!(header(&first, "x-relay-selection-reason"), "random");
    let first_body = first
        .json::<Value>()
        .await
        .expect("first real response JSON");
    let first_usage = Usage::from_openai(&first_body);

    let second = send_probe(
        &client,
        &base_url,
        &served_model,
        &format!("{common}\nSecond divergent branch. Reply with OK."),
    )
    .await;
    assert_eq!(header(&second, "x-relay-backend"), warm_provider);
    assert_eq!(
        header(&second, "x-relay-selection-reason"),
        "prompt-prefix-affinity"
    );
    let matched_prefix_bytes = header(&second, "x-relay-matched-prefix-bytes")
        .parse::<u64>()
        .expect("real matched-prefix byte count");
    assert!(matched_prefix_bytes >= (common.len() - 256) as u64);
    let second_body = second
        .json::<Value>()
        .await
        .expect("second real response JSON");
    let second_usage = Usage::from_openai(&second_body);

    eprintln!(
        "REAL_AFFINITY_EVIDENCE {}",
        json!({
            "backend": warm_provider,
            "matched_prefix_bytes": matched_prefix_bytes,
            "first_cache_hit_tokens": first_usage.cache_hit_tokens,
            "first_cache_miss_tokens": first_usage.cache_miss_tokens,
            "second_cache_hit_tokens": second_usage.cache_hit_tokens,
            "second_cache_miss_tokens": second_usage.cache_miss_tokens,
        })
    );
    server.abort();
}

async fn send_probe(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    content: &str,
) -> reqwest::Response {
    let response = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&json!({
            "model": model,
            "messages": [{"role":"user","content":content}],
            "max_tokens": 16
        }))
        .send()
        .await
        .expect("real affinity request");
    assert!(
        response.status().is_success(),
        "real affinity request failed with {}",
        response.status()
    );
    response
}

async fn start_gateway(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind real-probe gateway");
    let address = listener.local_addr().expect("real-probe gateway address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name} response header"))
        .to_str()
        .expect("valid metadata header")
        .to_owned()
}

fn real_probe_config(path: &PathBuf, data_dir: &TempDir) -> (Config, String) {
    let raw = fs::read_to_string(path).expect("read private real-probe config");
    let document = toml::from_str::<toml::Value>(&raw).expect("parse private real-probe config");
    let version = document
        .get("config_version")
        .and_then(toml::Value::as_integer)
        .expect("config_version");
    let mut config = match version {
        3 => Config::load(path).expect("load v3 private config"),
        2 => migrate_legacy_v2_config(document.clone()),
        1 => legacy_v1_config(&document),
        _ => panic!("real probe supports config versions 1, 2, and 3"),
    };

    let targets = [AdapterKind::OpenCodeGo, AdapterKind::DeepSeekOfficial]
        .map(|kind| first_chat_target(&config, kind));
    let served_model = "quotamux-real-affinity-probe".to_string();
    config.server = ServerConfig {
        listen: "127.0.0.1:0".into(),
        data_dir: data_dir.path().to_path_buf(),
    };
    config.models = vec![ServedModelConfig {
        name: served_model.clone(),
        aliases: Vec::new(),
        protocols: vec![Protocol::OpenAiChat],
        layers: vec![RouteLayerConfig {
            name: "mixed-real-workers".into(),
            strategy: RouteStrategy::PromptPrefixAffinity,
            targets: targets.into_iter().collect(),
        }],
    }];
    config.validate().expect("validate real-probe config");
    (config, served_model)
}

fn first_chat_target(config: &Config, kind: AdapterKind) -> RouteTargetConfig {
    let provider = config
        .backends
        .iter()
        .find(|provider| provider.adapter == kind)
        .unwrap_or_else(|| panic!("private config has no {} provider", kind.as_str()));
    let credential = provider.credentials.first().expect("provider credential");
    let model = provider
        .models
        .iter()
        .find(|model| {
            BackendClient::new(provider, credential, model)
                .is_ok_and(|client| client.protocols().contains(&Protocol::OpenAiChat))
        })
        .expect("provider OpenAI Chat model");
    RouteTargetConfig {
        backend: provider.id.clone(),
        credential: credential.id.clone(),
        model: model.name.clone(),
    }
}

fn legacy_v1_config(document: &toml::Value) -> Config {
    let logical_name = text_at(document, &["model", "logical_name"]);
    let backends = vec![
        legacy_provider(
            document,
            "opencode_go",
            "opencode-go",
            AdapterKind::OpenCodeGo,
        ),
        legacy_provider(
            document,
            "deepseek",
            "deepseek",
            AdapterKind::DeepSeekOfficial,
        ),
    ];
    Config {
        config_version: 3,
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            data_dir: PathBuf::from("unused-real-probe-data"),
        },
        affinity: Default::default(),
        backends,
        models: vec![ServedModelConfig {
            name: logical_name,
            aliases: Vec::new(),
            protocols: vec![Protocol::OpenAiChat],
            layers: Vec::new(),
        }],
    }
}

fn migrate_legacy_v2_config(mut document: toml::Value) -> Config {
    let root = document.as_table_mut().expect("v2 config root table");
    root.insert("config_version".into(), toml::Value::Integer(3));
    let mut backends = root.remove("providers").expect("v2 providers");
    for backend in backends
        .as_array_mut()
        .expect("v2 providers array")
        .iter_mut()
    {
        let backend = backend.as_table_mut().expect("v2 provider table");
        let adapter = backend.remove("kind").expect("v2 provider kind");
        backend.insert("adapter".into(), adapter);
    }
    root.insert("backends".into(), backends);

    for model in root
        .get_mut("models")
        .and_then(toml::Value::as_array_mut)
        .expect("v2 models array")
    {
        for layer in model
            .get_mut("layers")
            .and_then(toml::Value::as_array_mut)
            .expect("v2 model layers")
        {
            for target in layer
                .get_mut("targets")
                .and_then(toml::Value::as_array_mut)
                .expect("v2 layer targets")
            {
                let target = target.as_table_mut().expect("v2 target table");
                let backend = target.remove("provider").expect("v2 target provider");
                target.insert("backend".into(), backend);
            }
        }
    }

    let config = document.try_into::<Config>().expect("migrate v2 config");
    config.validate().expect("validate migrated v2 config");
    config
}

fn legacy_provider(
    document: &toml::Value,
    legacy_id: &str,
    id: &str,
    kind: AdapterKind,
) -> BackendConfig {
    BackendConfig {
        id: id.into(),
        adapter: kind,
        endpoint: None,
        credentials: vec![CredentialConfig {
            id: format!("{id}-real"),
            api_key: text_at(document, &["providers", legacy_id, "api_key"]),
        }],
        models: vec![BackendModelConfig {
            name: text_at(document, &["providers", legacy_id, "model"]),
            protocols: if kind == AdapterKind::OpenCodeGo {
                Vec::new()
            } else {
                vec![Protocol::OpenAiChat]
            },
            pricing: None,
        }],
    }
}

fn text_at(document: &toml::Value, path: &[&str]) -> String {
    path.iter()
        .try_fold(document, |value, key| value.get(*key))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("private config is missing {}", path.join(".")))
        .to_owned()
}
