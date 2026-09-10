use std::path::Path;

use crate::config::{load, LoadOptions};

/// Loads the config at each of `paths` and returns a one-line summary for
/// each ("`name`:`port`", or "error:`message`" if that path failed to
/// load), in the same order as `paths`.
pub fn summarize_all(paths: &[&Path]) -> Vec<String> {
    paths
        .iter()
        .map(|path| match load(path, &LoadOptions::default()) {
            Ok(cfg) => format!("{}:{}", cfg.name, cfg.port),
            Err(e) => format!("error:{e}"),
        })
        .collect()
}
