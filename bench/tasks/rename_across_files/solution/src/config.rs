/// A parsed configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub value: i64,
}

/// Parses a `name=value` line into a `Config`.
///
/// Panics if `s` is not of the form `name=value` or if `value` is not a
/// valid `i64`.
pub fn parse_config(s: &str) -> Config {
    let (name, value) = s
        .split_once('=')
        .unwrap_or_else(|| panic!("expected `name=value`, got {s:?}"));
    Config {
        name: name.trim().to_string(),
        value: value.trim().parse().expect("value should be an integer"),
    }
}
