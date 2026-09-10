use std::path::Path;

use crate::config::{load, LoadError, LoadOptions};

/// Starts a (simulated) server using the config at `path`, returning the
/// bound port.
pub fn start(path: &Path) -> Result<u16, LoadError> {
    let cfg = load(path, &LoadOptions::default())?;
    Ok(cfg.port)
}
