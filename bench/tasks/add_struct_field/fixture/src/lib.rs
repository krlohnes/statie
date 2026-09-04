/// Connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub host: String,
    pub port: u16,
}

impl Settings {
    /// Creates settings pointed at `host:port`.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Settings {
            host: host.into(),
            port,
        }
    }

    /// Creates settings pointed at `127.0.0.1:0`.
    pub fn local() -> Self {
        Settings {
            host: "127.0.0.1".to_string(),
            port: 0,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings::new("localhost", 8080)
    }
}
