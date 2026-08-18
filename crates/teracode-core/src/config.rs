use std::{fs, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AdapterKind, RetentionPolicy};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterTuning {
    pub adapter: AdapterKind,
    pub quality_tier: Option<u8>,
    pub speed_tier: Option<u8>,
    /// Higher values represent lower configured cost.
    pub cost_tier: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeraCodeConfig {
    pub provider_priority: Vec<AdapterKind>,
    pub adapter_tuning: Vec<AdapterTuning>,
    pub retention: RetentionPolicy,
}

impl Default for TeraCodeConfig {
    fn default() -> Self {
        Self {
            provider_priority: AdapterKind::ALL.to_vec(),
            adapter_tuning: AdapterKind::ALL
                .into_iter()
                .map(|adapter| AdapterTuning {
                    adapter,
                    quality_tier: None,
                    speed_tier: None,
                    cost_tier: None,
                })
                .collect(),
            retention: RetentionPolicy::KeepForever,
        }
    }
}

impl TeraCodeConfig {
    pub fn load_default() -> Result<(Self, PathBuf), ConfigError> {
        let directories = ProjectDirs::from("dev", "teracode", "TeraCode")
            .ok_or(ConfigError::NoApplicationDirectory)?;
        let path = directories.config_dir().join("config.json");
        if !path.exists() {
            return Ok((Self::default(), path));
        }
        let contents = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let config = serde_json::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        Ok((config, path))
    }

    pub fn tuning(&self, adapter: AdapterKind) -> Option<&AdapterTuning> {
        self.adapter_tuning
            .iter()
            .find(|tuning| tuning.adapter == adapter)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot determine the local application configuration directory")]
    NoApplicationDirectory,
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_do_not_invent_provider_rankings() {
        let config = TeraCodeConfig::default();
        assert!(config.adapter_tuning.iter().all(|tuning| {
            tuning.quality_tier.is_none()
                && tuning.speed_tier.is_none()
                && tuning.cost_tier.is_none()
        }));
        assert_eq!(config.retention, RetentionPolicy::KeepForever);
    }
}
