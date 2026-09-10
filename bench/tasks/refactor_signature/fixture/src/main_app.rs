use crate::config::{load, Config};

/// The outcome of starting the app: either it loaded a config and picked a
/// port, or loading failed and it fell back to `fallback_port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Loaded { config: Config, port: u16 },
    FellBack { port: u16 },
}

/// Starts the app from the config at `path`. `load` currently panics on
/// any problem, so this can only ever produce `StartOutcome::Loaded`; the
/// `FellBack` arm exists for the day `load` can fail without panicking.
pub fn start_app(path: &str, fallback_port: u16) -> StartOutcome {
    match std::panic::catch_unwind(|| load(path)) {
        Ok(cfg) => {
            let port = cfg.port;
            StartOutcome::Loaded { config: cfg, port }
        }
        Err(_) => StartOutcome::FellBack {
            port: fallback_port,
        },
    }
}
