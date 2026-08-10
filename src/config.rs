use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

pub const LOGICAL_MODEL: &str = "deepseek-v4-flash-0731";
pub const UPSTREAM_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Deserialize)]
pub struct Config {
    pub config_version: u32,
    pub server: ServerConfig,
    pub model: ModelConfig,
    pub providers: ProvidersConfig,
}

#[derive(Clone, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct ModelConfig {
    pub logical_name: String,
}

#[derive(Clone, Deserialize)]
pub struct ProvidersConfig {
    pub opencode_go: ProviderConfig,
    pub deepseek: ProviderConfig,
}

#[derive(Clone, Deserialize)]
pub struct ProviderConfig {
    pub endpoint: String,
    pub api_key: String,
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
        if self.config_version != 1 {
            return Err(ConfigError::Invalid(format!(
                "unsupported config_version {}; expected 1",
                self.config_version
            )));
        }
        self.server
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| {
                ConfigError::Invalid("server.listen must be an IP socket address".into())
            })?;
        if self.model.logical_name != LOGICAL_MODEL {
            return Err(ConfigError::Invalid(format!(
                "model.logical_name must be {LOGICAL_MODEL}"
            )));
        }
        validate_provider("providers.opencode_go", &self.providers.opencode_go)?;
        validate_provider("providers.deepseek", &self.providers.deepseek)?;
        Ok(())
    }
}

fn validate_provider(name: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
    if provider.api_key.trim().is_empty() {
        return Err(ConfigError::Invalid(format!("{name}.api_key is empty")));
    }
    if provider.model != UPSTREAM_MODEL {
        return Err(ConfigError::Invalid(format!(
            "{name}.model must be {UPSTREAM_MODEL}"
        )));
    }
    let url = reqwest::Url::parse(&provider.endpoint)
        .map_err(|_| ConfigError::Invalid(format!("{name}.endpoint is not a valid URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::Invalid(format!(
            "{name}.endpoint must be an absolute HTTP(S) URL"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        Config {
            config_version: 1,
            server: ServerConfig {
                listen: "127.0.0.1:8080".into(),
                data_dir: "data".into(),
            },
            model: ModelConfig {
                logical_name: LOGICAL_MODEL.into(),
            },
            providers: ProvidersConfig {
                opencode_go: ProviderConfig {
                    endpoint: "https://opencode.ai/zen/go/v1".into(),
                    api_key: "x".into(),
                    model: UPSTREAM_MODEL.into(),
                },
                deepseek: ProviderConfig {
                    endpoint: "https://api.deepseek.com".into(),
                    api_key: "y".into(),
                    model: UPSTREAM_MODEL.into(),
                },
            },
        }
    }

    #[test]
    fn validates_fixed_model_boundary() {
        let mut config = valid();
        config.providers.deepseek.model = "deepseek-v4-flash-free".into();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains(UPSTREAM_MODEL)
        );
    }

    #[test]
    fn rejects_empty_keys() {
        let mut config = valid();
        config.providers.opencode_go.api_key.clear();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("api_key")
        );
    }
}
