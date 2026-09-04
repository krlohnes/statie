use crate::config::{parse_config, Config};

/// Loads a `Config` from a raw string, delegating to `parse_config`.
pub fn load_from_str(s: &str) -> Config {
    parse_config(s)
}
