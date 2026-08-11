use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::Router;
use quotamux::{
    AppState, Config,
    app::build_app,
    config::{RouteLayerConfig, RouteStrategy, RouteTargetConfig, ServedModelConfig, ServerConfig},
    types::{Protocol, Usage},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const PROBE_MODEL: &str = "kimi-k3-real-probe";

#[tokio::test]
#[ignore = "requires three private Kimi credentials and consumes real quota"]
async fn each_configured_kimi_k3_target_streams_successfully() {
    let config = load_private_config();
    for target in all_targets(&config) {
        let (base_url, server, _data_dir) =
            start_single_target(config.clone(), target.clone()).await;
        let response = real_client()
            .post(format!("{base_url}/v1/chat/completions"))
            .header("x-relay-include-metadata", "1")
            .json(&json!({
                "model":PROBE_MODEL,
                "reasoning_effort":"low",
                "max_tokens":64,
                "stream":true,
                "messages":[{"role":"user","content":"Reply with exactly OK."}]
            }))
            .send()
            .await
            .expect("real Kimi stream request");
        let status = response.status();
        let provider = header(&response, "x-relay-provider");
        let upstream_model = header(&response, "x-relay-upstream-model");
        let body = response.text().await.expect("real Kimi stream body");
        assert!(
            status.is_success(),
            "{provider}/{upstream_model} returned {status}: {body}"
        );
        assert!(
            body.contains("data: [DONE]"),
            "{provider} stream did not complete"
        );
        eprintln!(
            "REAL_KIMI_STREAM_EVIDENCE {}",
            json!({"provider":provider,"upstream_model":upstream_model,"status":status.as_u16(),"done":true})
        );
        server.abort();
    }
}

#[tokio::test]
#[ignore = "requires private Kimi credentials and consumes two real subscription requests"]
async fn mixed_subscription_targets_keep_a_divergent_prefix_on_the_warm_target() {
    let mut config = load_private_config();
    let data_dir = TempDir::new().expect("temporary real Kimi affinity data directory");
    config.server = ServerConfig {
        listen: "127.0.0.1:0".into(),
        data_dir: data_dir.path().to_path_buf(),
    };
    let state = Arc::new(
        AppState::new_with_random_seed(config, 0x31_aff1)
            .await
            .expect("real Kimi affinity state"),
    );
    let (base_url, server) = start_gateway(build_app(state)).await;
    let client = real_client();
    let common = "QuotaMux Kimi real prefix affinity evidence. ".repeat(180);

    let first = send_nonstream(&client, &base_url, &format!("{common}Cold branch.")).await;
    let warm_provider = header(&first, "x-relay-provider");
    assert_eq!(header(&first, "x-relay-route-layer"), "subscriptions");
    assert_eq!(header(&first, "x-relay-selection-reason"), "random");
    let first_body = first.json::<Value>().await.expect("first real Kimi JSON");
    let first_usage = Usage::from_openai(&first_body);

    let second = send_nonstream(&client, &base_url, &format!("{common}Divergent branch.")).await;
    assert_eq!(header(&second, "x-relay-provider"), warm_provider);
    assert_eq!(header(&second, "x-relay-route-layer"), "subscriptions");
    assert_eq!(
        header(&second, "x-relay-selection-reason"),
        "prompt-prefix-affinity"
    );
    let matched_prefix_bytes = header(&second, "x-relay-matched-prefix-bytes")
        .parse::<u64>()
        .expect("real Kimi matched prefix bytes");
    assert!(matched_prefix_bytes > 6_000);
    let second_body = second.json::<Value>().await.expect("second real Kimi JSON");
    let second_usage = Usage::from_openai(&second_body);
    eprintln!(
        "REAL_KIMI_AFFINITY_EVIDENCE {}",
        json!({
            "provider":warm_provider,
            "matched_prefix_bytes":matched_prefix_bytes,
            "first_cache_hit_tokens":first_usage.cache_hit_tokens,
            "second_cache_hit_tokens":second_usage.cache_hit_tokens
        })
    );
    server.abort();
}

#[tokio::test]
#[ignore = "set QUOTAMUX_CONFIRM_1M=1; sends >256K real prompt tokens to all three targets"]
async fn each_kimi_k3_target_accepts_more_than_256k_prompt_tokens() {
    if env::var("QUOTAMUX_CONFIRM_1M").as_deref() != Ok("1") {
        eprintln!("REAL_KIMI_1M_EVIDENCE skipped: set QUOTAMUX_CONFIRM_1M=1");
        return;
    }

    let config = load_private_config();
    let long_context = " token".repeat(300_000);
    for target in all_targets(&config) {
        let (base_url, server, _data_dir) =
            start_single_target(config.clone(), target.clone()).await;
        let response = real_client()
            .post(format!("{base_url}/v1/chat/completions"))
            .header("x-relay-include-metadata", "1")
            .json(&json!({
                "model":PROBE_MODEL,
                "reasoning_effort":"low",
                "max_tokens":64,
                "messages":[{"role":"user","content":format!("{long_context}\nReply with exactly OK.")}]
            }))
            .send()
            .await
            .expect("real Kimi >256K request");
        let status = response.status();
        let provider = header(&response, "x-relay-provider");
        let upstream_model = header(&response, "x-relay-upstream-model");
        let body = response.text().await.expect("real Kimi >256K body");
        assert!(
            status.is_success(),
            "{provider}/{upstream_model} returned {status}: {body}"
        );
        let value = serde_json::from_str::<Value>(&body).expect("real Kimi >256K JSON");
        let usage = Usage::from_openai(&value);
        assert!(
            usage.input_tokens > 256 * 1024,
            "{provider}/{upstream_model} only reported {} prompt tokens",
            usage.input_tokens
        );
        eprintln!(
            "REAL_KIMI_1M_EVIDENCE {}",
            json!({"provider":provider,"upstream_model":upstream_model,"input_tokens":usage.input_tokens,"status":status.as_u16()})
        );
        server.abort();
    }
}

fn load_private_config() -> Config {
    let path = env::var_os("QUOTAMUX_REAL_KIMI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quotamux.toml"));
    Config::load(path).expect("load private Kimi configuration")
}

fn all_targets(config: &Config) -> Vec<RouteTargetConfig> {
    config
        .models
        .iter()
        .find(|model| {
            model.name == "kimi-k3" || model.aliases.iter().any(|alias| alias == "kimi-k3")
        })
        .expect("private configuration must expose kimi-k3")
        .layers
        .iter()
        .flat_map(|layer| layer.targets.iter().cloned())
        .collect()
}

async fn start_single_target(
    mut config: Config,
    target: RouteTargetConfig,
) -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let data_dir = TempDir::new().expect("temporary real Kimi probe data directory");
    config.server = ServerConfig {
        listen: "127.0.0.1:0".into(),
        data_dir: data_dir.path().to_path_buf(),
    };
    config.models = vec![ServedModelConfig {
        name: PROBE_MODEL.into(),
        aliases: Vec::new(),
        protocols: vec![Protocol::OpenAiChat],
        layers: vec![RouteLayerConfig {
            name: "real-probe".into(),
            strategy: RouteStrategy::Random,
            targets: vec![target],
        }],
    }];
    let state = Arc::new(AppState::new(config).await.expect("real Kimi probe state"));
    let (base_url, server) = start_gateway(build_app(state)).await;
    (base_url, server, data_dir)
}

async fn send_nonstream(
    client: &reqwest::Client,
    base_url: &str,
    content: &str,
) -> reqwest::Response {
    let response = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("x-relay-include-metadata", "1")
        .json(&json!({
            "model":"kimi-k3",
            "reasoning_effort":"low",
            "max_tokens":64,
            "messages":[{"role":"user","content":content}]
        }))
        .send()
        .await
        .expect("real Kimi affinity request");
    assert!(
        response.status().is_success(),
        "real Kimi affinity request failed with {}",
        response.status()
    );
    response
}

async fn start_gateway(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind real Kimi gateway");
    let address = listener.local_addr().expect("real Kimi gateway address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}

fn real_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("real Kimi HTTP client")
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name} response header"))
        .to_str()
        .expect("valid real Kimi metadata header")
        .to_owned()
}
