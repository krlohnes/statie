use confapp::cli;
use confapp::diagnostics;

fn write_temp(name: &str, contents: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "refactor_signature_cli_diag_test_{name}_{}.conf",
        std::process::id()
    ));
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn cli_load_from_args_uses_second_arg_when_present() {
    let path = write_temp("cli_arg", "name=svc-x\nport=10\n");
    let args = vec!["prog".to_string(), path.clone()];
    let cfg = cli::load_from_args(&args, "unused-default.conf");
    assert_eq!(cfg.name, "svc-x");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cli_load_from_args_falls_back_to_default_path() {
    let path = write_temp("cli_default", "name=svc-y\nport=11\n");
    let args = vec!["prog".to_string()];
    let cfg = cli::load_from_args(&args, &path);
    assert_eq!(cfg.name, "svc-y");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn diagnostics_summarize_all_reports_each_path_in_order() {
    let path_a = write_temp("diag_a", "name=a\nport=1\n");
    let path_b = write_temp("diag_b", "name=b\nport=2\n");
    let paths = vec![path_a.as_str(), path_b.as_str()];
    let summaries = diagnostics::summarize_all(&paths);
    assert_eq!(summaries, vec!["a:1".to_string(), "b:2".to_string()]);
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);
}
