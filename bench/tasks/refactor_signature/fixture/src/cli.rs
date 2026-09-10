use crate::config::{load, Config};

/// Loads the configuration named on the command line, or falls back to
/// `default_path` if `args` has nothing after the program name.
pub fn load_from_args(args: &[String], default_path: &str) -> Config {
    let path = args.get(1).map(String::as_str).unwrap_or(default_path);
    load(path)
}
