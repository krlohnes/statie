use crate::config::load;

/// Starts a (simulated) server using the config at `path`, returning the
/// bound port.
pub fn start(path: &str) -> u16 {
    let cfg = load(path);
    cfg.port
}
