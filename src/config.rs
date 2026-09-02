//! Configuration file loading.

use serde::Deserialize;
use std::path::PathBuf;
use tracing::{debug, info};

/// Application configuration
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    /// Which backend to use for compositor I/O
    pub backend: Option<String>,
    /// The unix socket name/path to use for the Wayland wire protocol
    pub socket_path: Option<String>,
}

impl ConfigFile {
    /// Load the configuration file from the current directory, fall
    /// back to the XDG .config path
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
