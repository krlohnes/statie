/// A parsed application configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

/// Loads a `Config` from the file at `path`, which must contain lines of
/// the form `name=...` and `port=...`.
///
/// Panics if the file cannot be read, if a required key is missing, or if
/// `port` is not a valid `u16`.
pub fn load(path: &str) -> Config {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read config file {path:?}: {e}"));

    let mut name: Option<String> = None;
    let mut port: Option<u16> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed config line: {line:?}"));
        match key.trim() {
            "name" => name = Some(value.trim().to_string()),
            "port" => {
                port = Some(
                    value
                        .trim()
                        .parse()
                        .unwrap_or_else(|e| panic!("invalid port {value:?}: {e}")),
                )
            }
            other => panic!("unknown config key: {other:?}"),
        }
    }

    Config {
        name: name.unwrap_or_else(|| panic!("missing required key `name`")),
        port: port.unwrap_or_else(|| panic!("missing required key `port`")),
    }
}
