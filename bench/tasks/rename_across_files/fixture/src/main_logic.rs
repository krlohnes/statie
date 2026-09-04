use crate::config::{parse_cfg, Config};

/// Loads a `Config`, falling back to a default when `raw` is `None`.
pub fn resolve_config(raw: Option<&str>) -> Config {
    match raw {
        Some(s) => parse_cfg(s),
        None => Config {
            name: "default".to_string(),
            value: 0,
        },
    }
}
