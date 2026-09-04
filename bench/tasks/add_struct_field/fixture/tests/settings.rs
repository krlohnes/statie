use add_struct_field::Settings;

#[test]
fn new_sets_retries_to_three() {
    let s = Settings::new("example.com", 443);
    assert_eq!(s.retries, 3);
}

#[test]
fn local_sets_retries_to_zero() {
    let s = Settings::local();
    assert_eq!(s.retries, 0);
}

#[test]
fn default_sets_retries_to_three() {
    let s = Settings::default();
    assert_eq!(s.retries, 3);
}

#[test]
fn new_still_sets_host_and_port() {
    let s = Settings::new("example.com", 443);
    assert_eq!(s.host, "example.com");
    assert_eq!(s.port, 443);
}
