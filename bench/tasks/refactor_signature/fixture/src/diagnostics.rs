use crate::config::load;

/// Loads the config at each of `paths` and returns a one-line summary for
/// each ("`name`:`port`"), in the same order as `paths`.
pub fn summarize_all(paths: &[&str]) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            let cfg = load(path);
            format!("{}:{}", cfg.name, cfg.port)
        })
        .collect()
}
