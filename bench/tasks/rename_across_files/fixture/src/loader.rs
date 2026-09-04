use crate::config::{parse_cfg, Config};

/// Loads a `Config` from a raw string, delegating to `parse_cfg`.
pub fn load_from_str(s: &str) -> Config {
    parse_cfg(s)
}
