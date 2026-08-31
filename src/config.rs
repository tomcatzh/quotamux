use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    affinity::PrefixAffinityConfig,
    provider::{EndpointPolicy, ModelProtocolPolicy, adapter_for},
    types::Protocol,
};

pub const LOGICAL_MODEL: &str = "deepseek-v4-flash-0731";
pub const UPSTREAM_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub config_version: u32,
    pub server: ServerConfig,
    #[serde(default)]
    pub affinity: PrefixAffinityConfig,
    pub backends: Vec<BackendConfig>,
    pub models: Vec<ServedModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: PathBuf,
    #[serde(default)]
    pub timeouts: ServerTimeoutConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerTimeoutConfig {
    pub upstream_connect_ms: u64,
    pub upstream_read_ms: u64,
    pub upstream_stream_read_ms: u64,
    pub upstream_total_ms: u64,
    pub downstream_sse_heartbeat_ms: u64,
    pub route_probe_wait_ms: u64,
}

impl Default for ServerTimeoutConfig {
    fn default() -> Self {
        Self {
            upstream_connect_ms: 10_000,
            upstream_read_ms: 90_000,
            upstream_stream_read_ms: 300_000,
            upstream_total_ms: 2 * 60 * 60 * 1_000,
            downstream_sse_heartbeat_ms: 15_000,
            route_probe_wait_ms: 5_000,
        }
    }
}

impl ServerTimeoutConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in [
            ("upstream_connect_ms", self.upstream_connect_ms),
            ("upstream_read_ms", self.upstream_read_ms),
            ("upstream_stream_read_ms", self.upstream_stream_read_ms),
            ("upstream_total_ms", self.upstream_total_ms),
            (
                "downstream_sse_heartbeat_ms",
                self.downstream_sse_heartbeat_ms,
            ),
            ("route_probe_wait_ms", self.route_probe_wait_ms),
        ] {
            if value == 0 {
                return Err(invalid(format!("server.timeouts.{name} must be positive")));
            }
        }
        if self.upstream_total_ms < self.upstream_connect_ms {
            return Err(invalid(
                "server.timeouts.upstream_total_ms must be at least upstream_connect_ms",
            ));
        }
        if self.upstream_total_ms < self.upstream_read_ms {
            return Err(invalid(
                "server.timeouts.upstream_total_ms must be at least upstream_read_ms",
            ));
        }
        if self.upstream_total_ms < self.upstream_stream_read_ms {
            return Err(invalid(
                "server.timeouts.upstream_total_ms must be at least upstream_stream_read_ms",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum AdapterKind {
    #[serde(rename = "deepseek-official")]
    DeepSeekOfficial,
    #[serde(rename = "kimi-official")]
    KimiOfficial,
    #[serde(rename = "kimi-code")]
    KimiCode,
    #[serde(rename = "aliyun-bailian")]
    AliyunBailian,
    #[serde(rename = "ollama-cloud")]
    OllamaCloud,
    #[serde(rename = "opencode-zen")]
    OpenCodeZen,
    #[serde(rename = "opencode-go")]
    OpenCodeGo,
    #[serde(rename = "zhipu-coding-plan")]
    ZhipuCodingPlan,
    #[serde(rename = "custom-chat-completions")]
    CustomChatCompletions,
    #[serde(rename = "custom-responses")]
    CustomResponses,
    #[serde(rename = "custom-anthropic")]
    CustomAnthropic,
}

impl AdapterKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeepSeekOfficial => "deepseek-official",
            Self::KimiOfficial => "kimi-official",
            Self::KimiCode => "kimi-code",
            Self::AliyunBailian => "aliyun-bailian",
            Self::OllamaCloud => "ollama-cloud",
            Self::OpenCodeZen => "opencode-zen",
            Self::OpenCodeGo => "opencode-go",
            Self::ZhipuCodingPlan => "zhipu-coding-plan",
            Self::CustomChatCompletions => "custom-chat-completions",
            Self::CustomResponses => "custom-responses",
            Self::CustomAnthropic => "custom-anthropic",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub id: String,
    pub adapter: AdapterKind,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub credentials: Vec<CredentialConfig>,
    pub models: Vec<BackendModelConfig>,
}

impl BackendConfig {
    pub fn credential(&self, id: &str) -> Option<&CredentialConfig> {
        self.credentials
            .iter()
            .find(|credential| credential.id == id)
    }

    pub fn model(&self, name: &str) -> Option<&BackendModelConfig> {
        self.models.iter().find(|model| model.name == name)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub id: String,
    pub api_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendModelConfig {
    pub name: String,
    #[serde(default)]
    pub protocols: Vec<Protocol>,
    #[serde(default)]
    pub pricing: Option<ModelPricingConfig>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelPricingConfig {
    pub cache_hit_input_usd_per_million: f64,
    pub cache_miss_input_usd_per_million: f64,
    pub output_usd_per_million: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServedModelConfig {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub protocols: Vec<Protocol>,
    pub layers: Vec<RouteLayerConfig>,
}

impl ServedModelConfig {
    pub fn accepts_name(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|alias| alias == name)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum RouteStrategy {
    Random,
    PromptPrefixAffinity,
}

impl RouteStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::PromptPrefixAffinity => "prompt-prefix-affinity",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLayerConfig {
    pub name: String,
    pub strategy: RouteStrategy,
    pub targets: Vec<RouteTargetConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RouteTargetConfig {
    pub backend: String,
    pub credential: String,
    pub model: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|mut source| {
            // toml attaches the complete source document to its error so it can
            // render snippets. Configuration files contain API keys, therefore
            // neither Debug nor Display may retain that input.
            source.set_input(None);
            ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;

        if config.server.data_dir.is_relative() {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            config.server.data_dir = parent.join(&config.server.data_dir);
        }
        if let Some(data_dir) = env::var_os("QUOTAMUX_DATA_DIR") {
            config.server.data_dir = PathBuf::from(data_dir);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.config_version != 3 {
            return Err(invalid(format!(
                "unsupported config_version {}; expected 3",
                self.config_version
            )));
        }
        self.server
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| invalid("server.listen must be an IP socket address"))?;
        self.server.timeouts.validate()?;
        if self.backends.is_empty() {
            return Err(invalid("backends must not be empty"));
        }
        if self.models.is_empty() {
            return Err(invalid("models must not be empty"));
        }
        self.affinity.validate().map_err(invalid)?;

        let mut backend_ids = HashSet::new();
        for (backend_index, backend) in self.backends.iter().enumerate() {
            let path = format!("backends[{backend_index}]");
            validate_id(&format!("{path}.id"), &backend.id)?;
            if !backend_ids.insert(backend.id.as_str()) {
                return Err(invalid(format!(
                    "{path}.id duplicates backend {}",
                    backend.id
                )));
            }
            validate_backend(&path, backend)?;
        }

        let backends = self
            .backends
            .iter()
            .map(|backend| (backend.id.as_str(), backend))
            .collect::<HashMap<_, _>>();
        let mut public_names = HashMap::<&str, &str>::new();
        for (model_index, model) in self.models.iter().enumerate() {
            let path = format!("models[{model_index}]");
            validate_id(&format!("{path}.name"), &model.name)?;
            register_public_name(&mut public_names, &path, &model.name, &model.name)?;
            for (alias_index, alias) in model.aliases.iter().enumerate() {
                validate_id(&format!("{path}.aliases[{alias_index}]"), alias)?;
                register_public_name(&mut public_names, &path, alias, &model.name)?;
            }
            if model.protocols.is_empty() {
                return Err(invalid(format!("{path}.protocols must not be empty")));
            }
            let mut protocols = HashSet::new();
            for protocol in &model.protocols {
                if !protocols.insert(*protocol) {
                    return Err(invalid(format!(
                        "{path}.protocols contains duplicate {}",
                        protocol.as_str()
                    )));
                }
            }
            if model.layers.is_empty() {
                return Err(invalid(format!("{path}.layers must not be empty")));
            }
            let mut layer_names = HashSet::new();
            for (layer_index, layer) in model.layers.iter().enumerate() {
                let layer_path = format!("{path}.layers[{layer_index}]");
                validate_id(&format!("{layer_path}.name"), &layer.name)?;
                if !layer_names.insert(layer.name.as_str()) {
                    return Err(invalid(format!(
                        "{layer_path}.name duplicates layer {}",
                        layer.name
                    )));
                }
                if layer.targets.is_empty() {
                    return Err(invalid(format!("{layer_path}.targets must not be empty")));
                }
                let mut targets = HashSet::new();
                for (target_index, target) in layer.targets.iter().enumerate() {
                    let target_path = format!("{layer_path}.targets[{target_index}]");
                    if !targets.insert(target) {
                        return Err(invalid(format!(
                            "{target_path} duplicates another target in layer {}",
                            layer.name
                        )));
                    }
                    validate_target(&target_path, target, &backends)?;
                }
            }
        }
        Ok(())
    }

    pub fn backend(&self, id: &str) -> Option<&BackendConfig> {
        self.backends.iter().find(|backend| backend.id == id)
    }

    pub fn resolve_model(&self, name: &str) -> Option<&ServedModelConfig> {
        self.models.iter().find(|model| model.accepts_name(name))
    }
}

fn validate_backend(path: &str, backend: &BackendConfig) -> Result<(), ConfigError> {
    let adapter = adapter_for(backend.adapter).ok_or_else(|| {
        invalid(format!(
            "{path}.adapter {} is recognized but not implemented by this QuotaMux build",
            backend.adapter.as_str()
        ))
    })?;
    if adapter.kind() != backend.adapter {
        return Err(invalid(format!(
            "internal adapter registry mismatch for adapter {}",
            backend.adapter.as_str()
        )));
    }
    match adapter.endpoint_policy() {
        EndpointPolicy::Official(_) if backend.endpoint.is_some() => {
            return Err(invalid(format!(
                "{path}.endpoint is not configurable for official adapter {}; use the matching custom-* adapter for a custom endpoint",
                backend.adapter.as_str()
            )));
        }
        EndpointPolicy::Official(_) => {}
        EndpointPolicy::ConfiguredExact => {
            let endpoint = backend.endpoint.as_deref().ok_or_else(|| {
                invalid(format!(
                    "{path}.endpoint is required for custom adapter {}",
                    backend.adapter.as_str()
                ))
            })?;
            validate_endpoint(&format!("{path}.endpoint"), endpoint)?;
        }
    }
    if backend.credentials.is_empty() {
        return Err(invalid(format!("{path}.credentials must not be empty")));
    }
    let mut credential_ids = HashSet::new();
    for (index, credential) in backend.credentials.iter().enumerate() {
        let credential_path = format!("{path}.credentials[{index}]");
        validate_id(&format!("{credential_path}.id"), &credential.id)?;
        if !credential_ids.insert(credential.id.as_str()) {
            return Err(invalid(format!(
                "{credential_path}.id duplicates credential {}",
                credential.id
            )));
        }
        if credential.api_key.trim().is_empty() {
            return Err(invalid(format!("{credential_path}.api_key is empty")));
        }
    }
    if backend.models.is_empty() {
        return Err(invalid(format!("{path}.models must not be empty")));
    }
    let mut model_names = HashSet::new();
    for (index, model) in backend.models.iter().enumerate() {
        let model_path = format!("{path}.models[{index}]");
        validate_id(&format!("{model_path}.name"), &model.name)?;
        if !model_names.insert(model.name.as_str()) {
            return Err(invalid(format!(
                "{model_path}.name duplicates enabled model {}",
                model.name
            )));
        }
        let native_protocols = match adapter.model_protocol_policy() {
            ModelProtocolPolicy::OfficialCatalog => {
                if !model.protocols.is_empty() {
                    return Err(invalid(format!(
                        "{model_path}.protocols is not configurable for adapter {}; the adapter owns each official model endpoint",
                        backend.adapter.as_str()
                    )));
                }
                let expected = adapter.protocol_for_model(&model.name).ok_or_else(|| {
                    invalid(format!(
                        "{model_path}.name {} is not in the current official catalog for adapter {}",
                        model.name,
                        backend.adapter.as_str()
                    ))
                })?;
                vec![expected]
            }
            ModelProtocolPolicy::Listed => {
                if model.protocols.is_empty() {
                    return Err(invalid(format!("{model_path}.protocols must not be empty")));
                }
                model.protocols.clone()
            }
        };
        let mut protocols = HashSet::new();
        for protocol in &native_protocols {
            if !protocols.insert(*protocol) {
                return Err(invalid(format!(
                    "{model_path}.protocols contains duplicate {}",
                    protocol.as_str()
                )));
            }
            if !adapter.supports_protocol(*protocol) {
                return Err(invalid(format!(
                    "{model_path}.protocols contains unsupported protocol {} for adapter {}",
                    protocol.as_str(),
                    backend.adapter.as_str()
                )));
            }
        }
        if let Some(pricing) = model.pricing {
            for (field, value) in [
                (
                    "cache_hit_input_usd_per_million",
                    pricing.cache_hit_input_usd_per_million,
                ),
                (
                    "cache_miss_input_usd_per_million",
                    pricing.cache_miss_input_usd_per_million,
                ),
                ("output_usd_per_million", pricing.output_usd_per_million),
            ] {
                if !value.is_finite() || value < 0.0 {
                    return Err(invalid(format!(
                        "{model_path}.pricing.{field} must be a finite non-negative USD amount"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_target(
    path: &str,
    target: &RouteTargetConfig,
    backends: &HashMap<&str, &BackendConfig>,
) -> Result<(), ConfigError> {
    let backend = backends.get(target.backend.as_str()).ok_or_else(|| {
        invalid(format!(
            "{path}.backend references missing backend {}",
            target.backend
        ))
    })?;
    if backend.credential(&target.credential).is_none() {
        return Err(invalid(format!(
            "{path}.credential references missing credential {} on backend {}",
            target.credential, target.backend
        )));
    }
    if backend.model(&target.model).is_none() {
        return Err(invalid(format!(
            "{path}.model references model {} not enabled on backend {}",
            target.model, target.backend
        )));
    }
    Ok(())
}

fn register_public_name<'a>(
    names: &mut HashMap<&'a str, &'a str>,
    path: &str,
    public_name: &'a str,
    model_name: &'a str,
) -> Result<(), ConfigError> {
    if let Some(existing) = names.insert(public_name, model_name) {
        return Err(invalid(format!(
            "{path} public name {public_name} already resolves to {existing}"
        )));
    }
    Ok(())
}

fn validate_id(path: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{path} must not be empty")));
    }
    if value.trim() != value {
        return Err(invalid(format!(
            "{path} must not contain leading or trailing whitespace"
        )));
    }
    Ok(())
}

fn validate_endpoint(path: &str, endpoint: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| invalid(format!("{path} must be an absolute HTTP(S) URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid(format!("{path} must be an absolute HTTP(S) URL")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn valid() -> Config {
        Config {
            config_version: 3,
            server: ServerConfig {
                listen: "127.0.0.1:8080".into(),
                data_dir: "data".into(),
                timeouts: ServerTimeoutConfig::default(),
            },
            affinity: PrefixAffinityConfig::default(),
            backends: vec![
                BackendConfig {
                    id: "go".into(),
                    adapter: AdapterKind::OpenCodeGo,
                    endpoint: None,
                    credentials: vec![CredentialConfig {
                        id: "go-plan".into(),
                        api_key: "x".into(),
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
                    endpoint: None,
                    credentials: vec![CredentialConfig {
                        id: "deepseek-payg".into(),
                        api_key: "y".into(),
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
                            backend: "go".into(),
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
        }
    }

    #[test]
    fn validates_three_layer_references_and_aliases() {
        let config = valid();
        config.validate().unwrap();
        assert_eq!(
            config.resolve_model(UPSTREAM_MODEL).unwrap().name,
            LOGICAL_MODEL
        );
    }

    #[test]
    fn server_timeouts_default_for_existing_configuration_files() {
        let server: ServerConfig = toml::from_str(
            r#"
listen = "127.0.0.1:8080"
data_dir = "data"
"#,
        )
        .unwrap();
        let defaults = ServerTimeoutConfig::default();
        assert_eq!(
            server.timeouts.upstream_connect_ms,
            defaults.upstream_connect_ms
        );
        assert_eq!(server.timeouts.upstream_read_ms, defaults.upstream_read_ms);
        assert_eq!(
            server.timeouts.upstream_stream_read_ms,
            defaults.upstream_stream_read_ms
        );
        assert_eq!(
            server.timeouts.upstream_total_ms,
            defaults.upstream_total_ms
        );
        assert_eq!(
            server.timeouts.downstream_sse_heartbeat_ms,
            defaults.downstream_sse_heartbeat_ms
        );
        assert_eq!(
            server.timeouts.route_probe_wait_ms,
            defaults.route_probe_wait_ms
        );
    }

    #[test]
    fn validates_positive_ordered_server_timeouts() {
        let mut config = valid();
        config.server.timeouts.upstream_stream_read_ms = 0;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("upstream_stream_read_ms must be positive"));

        config.server.timeouts.upstream_stream_read_ms = 301_000;
        config.server.timeouts.upstream_total_ms = 300_000;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("upstream_total_ms must be at least upstream_stream_read_ms"));
    }

    #[test]
    fn rejects_empty_keys_without_exposing_secret_values() {
        let mut config = valid();
        config.backends[0].credentials[0].api_key.clear();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("api_key is empty"));
        assert!(!error.contains("deepseek-payg"));
    }

    #[test]
    fn rejects_missing_target_model() {
        let mut config = valid();
        config.models[0].layers[0].targets[0].model = "not-enabled".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("not enabled"));
    }

    #[test]
    fn rejects_duplicate_public_alias() {
        let mut config = valid();
        let mut duplicate = config.models[0].clone();
        duplicate.name = "second-model".into();
        duplicate.aliases = vec![UPSTREAM_MODEL.into()];
        config.models.push(duplicate);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("already resolves"));
    }

    #[test]
    fn validates_optional_model_pricing_as_finite_non_negative_usd_rates() {
        let mut config = valid();
        config.backends[0].models[0].pricing = Some(ModelPricingConfig {
            cache_hit_input_usd_per_million: 1.0,
            cache_miss_input_usd_per_million: 2.0,
            output_usd_per_million: 3.0,
        });
        config.validate().unwrap();

        config.backends[0].models[0]
            .pricing
            .as_mut()
            .unwrap()
            .output_usd_per_million = f64::NAN;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("output_usd_per_million"));
        assert!(error.contains("finite non-negative"));
    }

    #[test]
    fn recognized_but_unimplemented_adapters_are_rejected() {
        for adapter in [
            AdapterKind::AliyunBailian,
            AdapterKind::OllamaCloud,
            AdapterKind::OpenCodeZen,
        ] {
            let mut config = valid();
            config.backends[0].adapter = adapter;
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("recognized but not implemented"),
                "{adapter:?}"
            );
        }
    }

    #[test]
    fn kimi_code_declares_chat_and_anthropic_as_official_protocols() {
        let adapter = adapter_for(AdapterKind::KimiCode).unwrap();
        assert_eq!(
            adapter.endpoint_policy(),
            EndpointPolicy::Official("https://api.kimi.com/coding/v1")
        );

        let mut config = valid();
        config.backends[0].adapter = AdapterKind::KimiCode;
        config.backends[0].models[0].name = "k3".into();
        config.backends[0].models[0].protocols = vec![Protocol::OpenAiChat];
        config.models[0].layers[0].targets[0].model = "k3".into();
        config.validate().unwrap();

        config.backends[0].models[0].protocols =
            vec![Protocol::OpenAiChat, Protocol::AnthropicMessages];
        config.validate().unwrap();

        assert!(!adapter.supports_protocol(Protocol::OpenAiResponses));
    }

    #[test]
    fn zhipu_coding_plan_declares_all_three_native_protocols() {
        let adapter = adapter_for(AdapterKind::ZhipuCodingPlan).unwrap();
        assert_eq!(
            adapter.endpoint_policy(),
            EndpointPolicy::Official("https://open.bigmodel.cn/api")
        );

        let mut config = valid();
        config.backends[0].adapter = AdapterKind::ZhipuCodingPlan;
        config.backends[0].models[0].name = "glm-5.3-flash".into();
        config.backends[0].models[0].protocols = vec![
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ];
        config.models[0].layers[0].targets[0].model = "glm-5.3-flash".into();
        config.validate().unwrap();

        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            assert!(adapter.supports_protocol(protocol));
        }
    }

    #[test]
    fn startup_rejects_unsupported_backend_protocols_instead_of_discarding_them() {
        let mut config = valid();
        config.backends[0].adapter = AdapterKind::KimiCode;
        config.backends[0].models[0].name = "k3".into();
        config.backends[0].models[0].protocols = vec![
            Protocol::OpenAiChat,
            Protocol::AnthropicMessages,
            Protocol::OpenAiResponses,
        ];
        config.models[0].layers[0].targets[0].model = "k3".into();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("unsupported protocol openai-responses"));
        assert_eq!(config.backends[0].models[0].protocols.len(), 3);
    }

    #[test]
    fn validation_keeps_unsupported_protocols_out_of_runtime_state() {
        let mut config = valid();
        config.backends[0].adapter = AdapterKind::KimiCode;
        config.backends[0].models[0].name = "k3".into();
        config.backends[0].models[0].protocols = vec![Protocol::OpenAiChat];
        config.models[0].layers[0].targets[0].model = "k3".into();
        config.backends[0].models[0]
            .protocols
            .push(Protocol::OpenAiResponses);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("unsupported protocol openai-responses"));
        assert!(error.contains("kimi-code"));
    }

    #[test]
    fn deepseek_official_supports_all_three_ingress_protocols() {
        let mut config = valid();
        config.backends[0].adapter = AdapterKind::DeepSeekOfficial;
        config.backends[0].models[0].protocols = vec![
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ];
        config.validate().unwrap();
    }

    #[test]
    fn opencode_go_owns_its_model_to_protocol_catalog() {
        let mut config = valid();
        config.validate().unwrap();
        config.backends[0].models[0].name = "unknown-go-model".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("not in the current official catalog"));

        config.backends[0].models[0].name = UPSTREAM_MODEL.into();
        config.backends[0].models[0].protocols = vec![Protocol::OpenAiChat];
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("protocols is not configurable"));
    }

    #[test]
    fn removed_endpoint_protocol_field_is_rejected_during_deserialization() {
        let error = toml::from_str::<BackendModelConfig>(
            "name = \"model\"\nendpoint_protocol = \"openai-chat\"\nprotocols = [\"openai-chat\"]",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field `endpoint_protocol`"));
    }

    #[test]
    fn official_adapter_endpoint_overrides_are_rejected() {
        let mut config = valid();
        config.backends[0].endpoint = Some("https://customer.example/v1".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("endpoint is not configurable"));
        assert!(error.contains("custom-*"));
    }

    #[test]
    fn official_endpoint_is_rejected_even_when_it_equals_the_default() {
        let mut config = valid();
        config.backends[0].endpoint = Some("https://opencode.ai/zen/go/v1/".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("endpoint is not configurable"));

        config.backends[0].endpoint = Some("https://customer.example/v1".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn custom_backend_requires_an_absolute_endpoint_and_uses_listed_protocols() {
        let mut config = valid();
        config.backends[0].adapter = AdapterKind::CustomChatCompletions;
        config.backends[0].endpoint = None;
        config.backends[0].models[0].protocols = vec![Protocol::OpenAiChat];

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("endpoint is required for custom adapter"));

        config.backends[0].endpoint = Some("relative/path".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("absolute HTTP(S) URL"));

        config.backends[0].endpoint = Some("https://customer.example/exact".into());
        config.validate().unwrap();
    }

    #[test]
    fn unknown_adapter_fails_during_deserialization() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            adapter: AdapterKind,
        }

        let error = toml::from_str::<Wrapper>("adapter = \"not-an-adapter\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant"));
    }

    #[test]
    fn legacy_provider_configuration_terms_are_rejected() {
        let backend_error = toml::from_str::<BackendConfig>(
            r#"
id = "go"
kind = "opencode-go"
credentials = []
models = []
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(backend_error.contains("unknown field `kind`"));

        let target_error = toml::from_str::<RouteTargetConfig>(
            r#"
provider = "go"
credential = "go-a"
model = "deepseek-v4-flash"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(target_error.contains("unknown field `provider`"));

        let document_error = toml::from_str::<Config>(
            r#"
config_version = 3
providers = []
models = []
[server]
listen = "127.0.0.1:8080"
data_dir = "data"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(document_error.contains("unknown field `providers`"));
    }

    #[test]
    fn config_load_parse_errors_never_retain_the_source_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quotamux.toml");
        let secret = "sk-must-never-appear-in-an-error";
        fs::write(
            &path,
            format!(
                r#"
config_version = 3
api_key = "{secret}"
backends = []
models = []
[server]
listen = "127.0.0.1:8080"
data_dir = "data"
"#
            ),
        )
        .unwrap();

        let error = Config::load(&path).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn validates_prompt_affinity_strategy_and_tuning() {
        let mut config = valid();
        config.models[0].layers[0].strategy = RouteStrategy::PromptPrefixAffinity;
        config.validate().unwrap();
        config.affinity.checkpoint_bytes = 0;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("affinity.checkpoint_bytes"));
    }

    #[test]
    fn parses_documented_v3_shape() {
        let config: Config = toml::from_str(
            r#"
config_version = 3
[server]
listen = "127.0.0.1:8080"
data_dir = "data"
[server.timeouts]
upstream_connect_ms = 5000
upstream_read_ms = 60000
upstream_stream_read_ms = 300000
upstream_total_ms = 7200000
downstream_sse_heartbeat_ms = 15000
route_probe_wait_ms = 5000
[affinity]
checkpoint_bytes = 256
max_checkpoints_per_path = 1024
max_candidates_per_prefix = 4
max_leases = 2048
success_ttl_ms = 60000
[[backends]]
id = "go"
adapter = "opencode-go"
[[backends.credentials]]
id = "go-a"
api_key = "secret"
[[backends.models]]
name = "deepseek-v4-flash"
pricing = { cache_hit_input_usd_per_million = 1.0, cache_miss_input_usd_per_million = 2.0, output_usd_per_million = 3.0 }
[[models]]
name = "deepseek-v4-flash-0731"
aliases = ["deepseek-v4-flash"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]
[[models.layers]]
name = "plan"
strategy = "prompt-prefix-affinity"
targets = [{ backend = "go", credential = "go-a", model = "deepseek-v4-flash" }]
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.affinity.checkpoint_bytes, 256);
        assert_eq!(config.affinity.max_checkpoints_per_path, 1024);
        assert_eq!(config.affinity.max_leases, 2048);
        assert_eq!(config.server.timeouts.upstream_connect_ms, 5_000);
        assert_eq!(config.server.timeouts.upstream_stream_read_ms, 300_000);
        assert_eq!(config.server.timeouts.downstream_sse_heartbeat_ms, 15_000);
        assert_eq!(config.server.timeouts.route_probe_wait_ms, 5_000);
        assert_eq!(
            config.backends[0].models[0].pricing,
            Some(ModelPricingConfig {
                cache_hit_input_usd_per_million: 1.0,
                cache_miss_input_usd_per_million: 2.0,
                output_usd_per_million: 3.0,
            })
        );
        assert_eq!(
            config.models[0].layers[0].strategy,
            RouteStrategy::PromptPrefixAffinity
        );
    }
}
