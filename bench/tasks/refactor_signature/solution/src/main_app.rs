use std::path::Path;

use crate::config::{load, Config, LoadOptions};

/// The outcome of starting the app: either it loaded a config and picked a
/// port, or loading failed and it fell back to `fallback_port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Loaded { config: Config, port: u16 },
    FellBack { port: u16 },
}

/// Starts the app from the config at `path`, falling back to
/// `fallback_port` if loading fails.
pub fn start_app(path: &Path, fallback_port: u16) -> StartOutcome {
    match load(path, &LoadOptions::default()) {
        Ok(cfg) => {
            let port = cfg.port;
            StartOutcome::Loaded { config: cfg, port }
        }
        Err(_) => StartOutcome::FellBack {
            port: fallback_port,
        },
    }
}
