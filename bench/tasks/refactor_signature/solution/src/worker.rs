use std::path::Path;

use crate::config::{load, LoadError, LoadOptions};

/// Loads the config at `path` and reports whether the worker should run in
/// strict mode: strict whenever the configured name starts with `"prod-"`.
pub fn is_strict_mode(path: &Path) -> Result<bool, LoadError> {
    let cfg = load(path, &LoadOptions::default())?;
    Ok(cfg.name.starts_with("prod-"))
}
