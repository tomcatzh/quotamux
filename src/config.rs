use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{affinity::PrefixAffinityConfig, types::Protocol};

pub const LOGICAL_MODEL: &str = "deepseek-v4-flash-0731";
pub const UPSTREAM_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub config_version: u32,
    pub server: ServerConfig,
    #[serde(default)]
    pub affinity: PrefixAffinityConfig,
    pub providers: Vec<ProviderConfig>,
    pub models: Vec<ServedModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ProviderKind {
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
    #[serde(rename = "custom-chat-completions")]
    CustomChatCompletions,
    #[serde(rename = "custom-responses")]
    CustomResponses,
    #[serde(rename = "custom-anthropic")]
    CustomAnthropic,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeepSeekOfficial => "deepseek-official",
            Self::KimiOfficial => "kimi-official",
            Self::KimiCode => "kimi-code",
            Self::AliyunBailian => "aliyun-bailian",
            Self::OllamaCloud => "ollama-cloud",
            Self::OpenCodeZen => "opencode-zen",
            Self::OpenCodeGo => "opencode-go",
            Self::CustomChatCompletions => "custom-chat-completions",
            Self::CustomResponses => "custom-responses",
            Self::CustomAnthropic => "custom-anthropic",
        }
    }

    pub const fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::DeepSeekOfficial => Some("https://api.deepseek.com"),
            Self::KimiOfficial => Some("https://api.moonshot.cn/v1"),
            Self::KimiCode => Some("https://api.kimi.com/coding/v1"),
            Self::OllamaCloud => Some("https://ollama.com/v1"),
            Self::OpenCodeZen => Some("https://opencode.ai/zen/v1"),
            Self::OpenCodeGo => Some("https://opencode.ai/zen/go/v1"),
            Self::AliyunBailian
            | Self::CustomChatCompletions
            | Self::CustomResponses
            | Self::CustomAnthropic => None,
        }
    }

    pub const fn fixed_protocol(self) -> Option<Protocol> {
        match self {
            Self::KimiOfficial | Self::KimiCode | Self::CustomChatCompletions => {
                Some(Protocol::OpenAiChat)
            }
            Self::CustomResponses => Some(Protocol::OpenAiResponses),
            Self::CustomAnthropic => Some(Protocol::AnthropicMessages),
            _ => None,
        }
    }

    pub const fn uses_exact_endpoint(self) -> bool {
        matches!(
            self,
            Self::CustomChatCompletions | Self::CustomResponses | Self::CustomAnthropic
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub credentials: Vec<CredentialConfig>,
    pub models: Vec<ProviderModelConfig>,
}

impl ProviderConfig {
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint
            .as_deref()
            .or_else(|| self.kind.default_endpoint())
    }

    pub fn credential(&self, id: &str) -> Option<&CredentialConfig> {
        self.credentials
            .iter()
            .find(|credential| credential.id == id)
    }

    pub fn model(&self, name: &str) -> Option<&ProviderModelConfig> {
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
pub struct ProviderModelConfig {
    pub name: String,
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
    pub provider: String,
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
        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
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
        if self.config_version != 2 {
            return Err(invalid(format!(
                "unsupported config_version {}; expected 2",
                self.config_version
            )));
        }
        self.server
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| invalid("server.listen must be an IP socket address"))?;
        if self.providers.is_empty() {
            return Err(invalid("providers must not be empty"));
        }
        if self.models.is_empty() {
            return Err(invalid("models must not be empty"));
        }
        self.affinity.validate().map_err(invalid)?;

        let mut provider_ids = HashSet::new();
        for (provider_index, provider) in self.providers.iter().enumerate() {
            let path = format!("providers[{provider_index}]");
            validate_id(&format!("{path}.id"), &provider.id)?;
            if !provider_ids.insert(provider.id.as_str()) {
                return Err(invalid(format!(
                    "{path}.id duplicates provider {}",
                    provider.id
                )));
            }
            validate_provider(&path, provider)?;
        }

        let providers = self
            .providers
            .iter()
            .map(|provider| (provider.id.as_str(), provider))
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
                    validate_target(&target_path, target, &providers)?;
                }
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn resolve_model(&self, name: &str) -> Option<&ServedModelConfig> {
        self.models.iter().find(|model| model.accepts_name(name))
    }
}

fn validate_provider(path: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
    let endpoint = provider.endpoint().ok_or_else(|| {
        invalid(format!(
            "{path}.endpoint is required for provider kind {}",
            provider.kind.as_str()
        ))
    })?;
    validate_endpoint(&format!("{path}.endpoint"), endpoint)?;
    if provider.credentials.is_empty() {
        return Err(invalid(format!("{path}.credentials must not be empty")));
    }
    let mut credential_ids = HashSet::new();
    for (index, credential) in provider.credentials.iter().enumerate() {
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
    if provider.models.is_empty() {
        return Err(invalid(format!("{path}.models must not be empty")));
    }
    let mut model_names = HashSet::new();
    for (index, model) in provider.models.iter().enumerate() {
        let model_path = format!("{path}.models[{index}]");
        validate_id(&format!("{model_path}.name"), &model.name)?;
        if !model_names.insert(model.name.as_str()) {
            return Err(invalid(format!(
                "{model_path}.name duplicates enabled model {}",
                model.name
            )));
        }
        if model.protocols.is_empty() {
            return Err(invalid(format!("{model_path}.protocols must not be empty")));
        }
        let mut protocols = HashSet::new();
        for protocol in &model.protocols {
            if !protocols.insert(*protocol) {
                return Err(invalid(format!(
                    "{model_path}.protocols contains duplicate {}",
                    protocol.as_str()
                )));
            }
            if provider
                .kind
                .fixed_protocol()
                .is_some_and(|fixed| fixed != *protocol)
            {
                return Err(invalid(format!(
                    "{model_path}.protocols must contain only {} for provider kind {}",
                    provider.kind.fixed_protocol().unwrap().as_str(),
                    provider.kind.as_str()
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
    providers: &HashMap<&str, &ProviderConfig>,
) -> Result<(), ConfigError> {
    let provider = providers.get(target.provider.as_str()).ok_or_else(|| {
        invalid(format!(
            "{path}.provider references missing provider {}",
            target.provider
        ))
    })?;
    if provider.credential(&target.credential).is_none() {
        return Err(invalid(format!(
            "{path}.credential references missing credential {} on provider {}",
            target.credential, target.provider
        )));
    }
    if provider.model(&target.model).is_none() {
        return Err(invalid(format!(
            "{path}.model references model {} not enabled on provider {}",
            target.model, target.provider
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
    let url =
        reqwest::Url::parse(endpoint).map_err(|_| invalid(format!("{path} is not a valid URL")))?;
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
            config_version: 2,
            server: ServerConfig {
                listen: "127.0.0.1:8080".into(),
                data_dir: "data".into(),
            },
            affinity: PrefixAffinityConfig::default(),
            providers: vec![
                ProviderConfig {
                    id: "go".into(),
                    kind: ProviderKind::OpenCodeGo,
                    endpoint: None,
                    credentials: vec![CredentialConfig {
                        id: "go-plan".into(),
                        api_key: "x".into(),
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
                    endpoint: None,
                    credentials: vec![CredentialConfig {
                        id: "deepseek-payg".into(),
                        api_key: "y".into(),
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
                            provider: "go".into(),
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
    fn rejects_empty_keys_without_exposing_secret_values() {
        let mut config = valid();
        config.providers[0].credentials[0].api_key.clear();
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
        config.providers[0].models[0].pricing = Some(ModelPricingConfig {
            cache_hit_input_usd_per_million: 1.0,
            cache_miss_input_usd_per_million: 2.0,
            output_usd_per_million: 3.0,
        });
        config.validate().unwrap();

        config.providers[0].models[0]
            .pricing
            .as_mut()
            .unwrap()
            .output_usd_per_million = f64::NAN;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("output_usd_per_million"));
        assert!(error.contains("finite non-negative"));
    }

    #[test]
    fn ollama_cloud_models_may_enable_chat_and_responses() {
        let mut config = valid();
        config.providers[0].kind = ProviderKind::OllamaCloud;
        config.providers[0].models[0].protocols =
            vec![Protocol::OpenAiChat, Protocol::OpenAiResponses];
        config.validate().unwrap();
    }

    #[test]
    fn kimi_code_has_its_own_chat_endpoint_and_rejects_other_egress_protocols() {
        assert_eq!(
            ProviderKind::KimiCode.default_endpoint(),
            Some("https://api.kimi.com/coding/v1")
        );
        assert_eq!(
            ProviderKind::KimiCode.fixed_protocol(),
            Some(Protocol::OpenAiChat)
        );

        let mut config = valid();
        config.providers[0].kind = ProviderKind::KimiCode;
        config.providers[0].models[0].name = "k3".into();
        config.models[0].layers[0].targets[0].model = "k3".into();
        config.validate().unwrap();

        config.providers[0].models[0].protocols = vec![Protocol::AnthropicMessages];
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("must contain only openai-chat"));
        assert!(error.contains("kimi-code"));
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
    fn parses_documented_v2_shape() {
        let config: Config = toml::from_str(
            r#"
config_version = 2
[server]
listen = "127.0.0.1:8080"
data_dir = "data"
[affinity]
checkpoint_bytes = 256
max_checkpoints_per_path = 1024
max_candidates_per_prefix = 4
max_leases = 2048
success_ttl_ms = 60000
[[providers]]
id = "go"
kind = "opencode-go"
[[providers.credentials]]
id = "go-a"
api_key = "secret"
[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat"]
pricing = { cache_hit_input_usd_per_million = 1.0, cache_miss_input_usd_per_million = 2.0, output_usd_per_million = 3.0 }
[[models]]
name = "deepseek-v4-flash-0731"
aliases = ["deepseek-v4-flash"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]
[[models.layers]]
name = "plan"
strategy = "prompt-prefix-affinity"
targets = [{ provider = "go", credential = "go-a", model = "deepseek-v4-flash" }]
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.affinity.checkpoint_bytes, 256);
        assert_eq!(config.affinity.max_checkpoints_per_path, 1024);
        assert_eq!(config.affinity.max_leases, 2048);
        assert_eq!(
            config.providers[0].models[0].pricing,
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
