use rename_across_files::{loader, parse_config, resolve_config, Config};

#[test]
fn parse_config_parses_name_and_value() {
    let cfg = parse_config("timeout=30");
    assert_eq!(
        cfg,
        Config {
            name: "timeout".to_string(),
            value: 30,
        }
    );
}

#[test]
fn loader_load_from_str_still_works() {
    let cfg = loader::load_from_str("retries=3");
    assert_eq!(
        cfg,
        Config {
            name: "retries".to_string(),
            value: 3,
        }
    );
}

#[test]
fn resolve_config_uses_default_when_none() {
    let cfg = resolve_config(None);
    assert_eq!(cfg.name, "default");
    assert_eq!(cfg.value, 0);
}

#[test]
fn resolve_config_parses_when_some() {
    let cfg = resolve_config(Some("level=5"));
    assert_eq!(
        cfg,
        Config {
            name: "level".to_string(),
            value: 5,
        }
    );
}
