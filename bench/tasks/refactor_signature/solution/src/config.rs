use std::fmt;
use std::path::Path;

/// A parsed application configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

/// Options controlling how `load` behaves.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// If the file has no `port` key, use this instead of failing. `None`
    /// means a missing `port` key is an error (the default).
    pub default_port: Option<u16>,
}

/// Errors `load` can return.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Io(std::io::Error),
    /// A config line was not of the form `key=value`.
    Malformed(String),
    /// A key was not one of the keys `load` understands.
    UnknownKey(String),
    /// A required key was missing and no option covered it.
    MissingKey(String),
    /// `port`'s value could not be parsed as a `u16`.
    InvalidPort(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "failed to read config file: {e}"),
            LoadError::Malformed(line) => write!(f, "malformed config line: {line:?}"),
            LoadError::UnknownKey(key) => write!(f, "unknown config key: {key:?}"),
            LoadError::MissingKey(key) => write!(f, "missing required key `{key}`"),
            LoadError::InvalidPort(value) => write!(f, "invalid port {value:?}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Loads a `Config` from the file at `path`, which must contain lines of
/// the form `name=...` and `port=...`.
///
/// Returns `LoadError` if the file cannot be read, if a required key is
/// missing (unless `opts` supplies a fallback for it), or if `port` is not
/// a valid `u16`.
pub fn load(path: &Path, opts: &LoadOptions) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;

    let mut name: Option<String> = None;
    let mut port: Option<u16> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| LoadError::Malformed(line.to_string()))?;
        match key.trim() {
            "name" => name = Some(value.trim().to_string()),
            "port" => {
                let value = value.trim();
                port = Some(
                    value
                        .parse()
                        .map_err(|_| LoadError::InvalidPort(value.to_string()))?,
                )
            }
            other => return Err(LoadError::UnknownKey(other.to_string())),
        }
    }

    let port = match port.or(opts.default_port) {
        Some(p) => p,
        None => return Err(LoadError::MissingKey("port".to_string())),
    };

    Ok(Config {
        name: name.ok_or_else(|| LoadError::MissingKey("name".to_string()))?,
        port,
    })
}
