use std::path::Path;

use confapp::config::{load, Config, LoadError, LoadOptions};

fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "refactor_signature_load_test_{name}_{}.conf",
        std::process::id()
    ));
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn load_returns_ok_config_for_valid_file() {
    let path = write_temp("ok", "name=svc-a\nport=8080\n");
    let opts = LoadOptions::default();
    let cfg = load(&path, &opts).unwrap();
    assert_eq!(
        cfg,
        Config {
            name: "svc-a".to_string(),
            port: 8080,
        }
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_returns_err_for_missing_file() {
    let path = Path::new("/nonexistent/does-not-exist.conf");
    let opts = LoadOptions::default();
    let err = load(path, &opts).unwrap_err();
    assert!(matches!(err, LoadError::Io(_)));
}

#[test]
fn load_returns_err_for_missing_key() {
    let path = write_temp("missing_key", "name=svc-b\n");
    let opts = LoadOptions::default();
    let err = load(&path, &opts).unwrap_err();
    assert!(matches!(err, LoadError::MissingKey(ref k) if k == "port"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_applies_default_port_option_when_port_missing_and_allowed() {
    let path = write_temp("default_port", "name=svc-c\n");
    let opts = LoadOptions {
        default_port: Some(9000),
        ..LoadOptions::default()
    };
    let cfg = load(&path, &opts).unwrap();
    assert_eq!(cfg.port, 9000);
    assert_eq!(cfg.name, "svc-c");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_returns_err_for_invalid_port() {
    let path = write_temp("bad_port", "name=svc-d\nport=notanumber\n");
    let opts = LoadOptions::default();
    let err = load(&path, &opts).unwrap_err();
    assert!(matches!(err, LoadError::InvalidPort(_)));
    let _ = std::fs::remove_file(&path);
}
