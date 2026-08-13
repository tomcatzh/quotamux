use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use quotamux::{Config, provider::ProviderClient, types::Protocol};

const CONFIG_PATH: &str = "quotamux.toml";
const MODEL: &str = "deepseek-v4-pro";
const PROVIDER_IDS: [&str; 2] = ["opencode-go", "deepseek"];

#[tokio::test]
#[ignore = "requires private DeepSeek V4 Pro credentials and consumes real requests"]
async fn configured_deepseek_v4_pro_targets_report_system_fingerprints() {
    let config = Config::load(CONFIG_PATH).unwrap_or_else(|error| {
        panic!("private {CONFIG_PATH} with DeepSeek V4 Pro credentials is required: {error}")
    });
    let targets = configured_targets(&config);
    let mut missing_required_fingerprints = Vec::new();

    for target in targets {
        let provider = config.provider(&target.provider).unwrap_or_else(|| {
            panic!(
                "private {CONFIG_PATH} is missing provider {}",
                target.provider
            )
        });
        let credential = provider.credential(&target.credential).unwrap_or_else(|| {
            panic!(
                "private {CONFIG_PATH} target for provider {} is missing credential {}",
                target.provider, target.credential
            )
        });
        let model = provider.model(MODEL).unwrap_or_else(|| {
            panic!(
                "private {CONFIG_PATH} provider {} is missing model {MODEL}",
                target.provider
            )
        });
        assert!(
            model.protocols.contains(&Protocol::OpenAiChat),
            "private {CONFIG_PATH} provider {} model {MODEL} must enable OpenAI Chat",
            target.provider
        );

        let client = ProviderClient::new(provider, credential, model).unwrap_or_else(|_| {
            panic!(
                "failed to construct ProviderClient for provider {} model {MODEL}",
                target.provider
            )
        });
        let response = match client
            .send(
                Protocol::OpenAiChat,
                &json!({
                    "model": MODEL,
                    "messages": [{"role": "user", "content": "Reply with exactly OK."}],
                    "max_tokens": 8,
                    "stream": false
                }),
                &HeaderMap::new(),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => panic!(
                "DeepSeek V4 Pro request failed for provider {} with status {:?}",
                target.provider, error.status
            ),
        };
        let body = response.json::<Value>().await.unwrap_or_else(|_| {
            panic!(
                "DeepSeek V4 Pro provider {} returned invalid JSON",
                target.provider
            )
        });
        let returned_model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "DeepSeek V4 Pro provider {} response is missing a model",
                    target.provider
                )
            });
        let fingerprint = body
            .get("system_fingerprint")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|fingerprint| !fingerprint.is_empty());
        if provider.kind == quotamux::config::ProviderKind::DeepSeekOfficial
            && fingerprint.is_none()
        {
            missing_required_fingerprints.push(target.provider.clone());
        }

        eprintln!(
            "REAL_DEEPSEEK_V4_PRO_EVIDENCE {}",
            json!({
                "provider": target.provider,
                "model": returned_model,
                "system_fingerprint": fingerprint
            })
        );
    }
    assert!(
        missing_required_fingerprints.is_empty(),
        "official DeepSeek V4 Pro responses missing system_fingerprint for: {}",
        missing_required_fingerprints.join(", ")
    );
}

fn configured_targets(config: &Config) -> Vec<quotamux::config::RouteTargetConfig> {
    let served_model = config
        .resolve_model(MODEL)
        .unwrap_or_else(|| panic!("private {CONFIG_PATH} is missing served model {MODEL}"));
    let targets = served_model
        .layers
        .iter()
        .flat_map(|layer| layer.targets.iter())
        .filter(|target| PROVIDER_IDS.contains(&target.provider.as_str()) && target.model == MODEL)
        .cloned()
        .collect::<Vec<_>>();

    for provider_id in PROVIDER_IDS {
        assert!(
            targets.iter().any(|target| target.provider == provider_id),
            "private {CONFIG_PATH} is missing a {MODEL} target for provider {provider_id}"
        );
    }
    targets
}
