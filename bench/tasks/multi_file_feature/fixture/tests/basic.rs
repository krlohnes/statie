use kvstore::Store;

#[test]
fn put_then_get_returns_value() {
    let mut store = Store::new();
    store.put("a", "1", &["x"]).unwrap();
    assert_eq!(store.get("a"), Some("1".to_string()));
}

#[test]
fn get_of_missing_key_is_none() {
    let store = Store::new();
    assert_eq!(store.get("nope"), None);
}

#[test]
fn duplicate_put_fails() {
    let mut store = Store::new();
    store.put("a", "1", &["x"]).unwrap();
    let err = store.put("a", "2", &["y"]).unwrap_err();
    assert_eq!(
        err,
        kvstore::error::StoreError::DuplicateKey("a".to_string())
    );
}

#[test]
fn keys_with_tag_finds_all_matches() {
    let mut store = Store::new();
    store.put("a", "1", &["red"]).unwrap();
    store.put("b", "2", &["red", "blue"]).unwrap();
    store.put("c", "3", &["blue"]).unwrap();

    let mut red = store.keys_with_tag("red");
    red.sort();
    assert_eq!(red, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn len_counts_records() {
    let mut store = Store::new();
    assert_eq!(store.len(), 0);
    store.put("a", "1", &["x"]).unwrap();
    store.put("b", "2", &["x"]).unwrap();
    assert_eq!(store.len(), 2);
}

#[test]
fn empty_tag_is_rejected() {
    let mut store = Store::new();
    let err = store.put("a", "1", &[""]).unwrap_err();
    assert_eq!(err, kvstore::error::StoreError::EmptyTag);
}
