use confapp::main_app::{start_app, StartOutcome};
use confapp::server;
use confapp::worker;

fn write_temp(name: &str, contents: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "refactor_signature_callers_test_{name}_{}.conf",
        std::process::id()
    ));
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn server_start_returns_configured_port() {
    let path = write_temp("server_ok", "name=svc\nport=4321\n");
    let port = server::start(&path);
    assert_eq!(port, 4321);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn worker_strict_mode_true_for_prod_prefixed_name() {
    let path = write_temp("worker_prod", "name=prod-svc\nport=1\n");
    assert_eq!(worker::is_strict_mode(&path), true);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn worker_strict_mode_false_for_other_name() {
    let path = write_temp("worker_dev", "name=dev-svc\nport=1\n");
    assert_eq!(worker::is_strict_mode(&path), false);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn main_app_falls_back_on_load_error() {
    let outcome = start_app("/nonexistent/does-not-exist.conf", 7777);
    assert_eq!(outcome, StartOutcome::FellBack { port: 7777 });
}

#[test]
fn main_app_loads_successfully() {
    let path = write_temp("main_app_ok", "name=svc-e\nport=55\n");
    let outcome = start_app(&path, 7777);
    match outcome {
        StartOutcome::Loaded { config, port } => {
            assert_eq!(port, 55);
            assert_eq!(config.name, "svc-e");
        }
        StartOutcome::FellBack { .. } => panic!("expected Loaded, got FellBack"),
    }
    let _ = std::fs::remove_file(&path);
}
