use std::path::Path;

use crate::config::{load, Config, LoadError, LoadOptions};

/// Loads the configuration named on the command line, or falls back to
/// `default_path` if `args` has nothing after the program name.
pub fn load_from_args(args: &[String], default_path: &str) -> Result<Config, LoadError> {
    let path = args.get(1).map(String::as_str).unwrap_or(default_path);
    load(Path::new(path), &LoadOptions::default())
}
