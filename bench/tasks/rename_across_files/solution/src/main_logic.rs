use crate::config::{parse_config, Config};

/// Loads a `Config`, falling back to a default when `raw` is `None`.
pub fn resolve_config(raw: Option<&str>) -> Config {
    match raw {
        Some(s) => parse_config(s),
        None => Config {
            name: "default".to_string(),
            value: 0,
        },
    }
}
