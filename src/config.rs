//! Configuration file loading.
//!
//! Looks for config.toml in the working directory (dev) or
//! $XDG_CONFIG_HOME/way-small/ and deserializes backend and socket options.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::{debug, info};

#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    pub backend: Option<String>,
    pub socket_path: Option<String>,
}

impl ConfigFile {
    pub fn load() -> Self {
        let config_paths = [
            Some(PathBuf::from("config.toml")),
            dirs::config_dir().map(|d| d.join("way-small/config.toml")),
        ];

        debug!("Looking for config in: {:?}", config_paths);

        for path in config_paths.into_iter().flatten() {
            if path.exists() {
                info!("Loading config from {}", path.display());
                match std::fs::read_to_string(&path) {
                    Ok(contents) => match toml::from_str::<ConfigFile>(&contents) {
                        Ok(config) => return config,
                        Err(e) => {
                            tracing::warn!("Failed to parse {}: {}", path.display(), e,);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("Failed to read {}: {}", path.display(), e);
                    }
                }
            }
        }

        Self::default()
    }
}
