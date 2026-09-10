use crate::config::load;

/// Loads the config at `path` and reports whether the worker should run in
/// strict mode: strict whenever the configured name starts with `"prod-"`.
pub fn is_strict_mode(path: &str) -> bool {
    let cfg = load(path);
    cfg.name.starts_with("prod-")
}
