use kvstore::error::StoreError;
use kvstore::Store;

#[test]
fn delete_removes_the_value() {
    let mut store = Store::new();
    store.put("a", "1", &["red"]).unwrap();
    store.delete("a").unwrap();
    assert_eq!(store.get("a"), None);
}

#[test]
fn delete_removes_key_from_tag_index() {
    let mut store = Store::new();
    store.put("a", "1", &["red"]).unwrap();
    store.put("b", "2", &["red"]).unwrap();

    store.delete("a").unwrap();

    let red = store.keys_with_tag("red");
    assert_eq!(red, vec!["b".to_string()]);
}

#[test]
fn delete_updates_the_record_count() {
    let mut store = Store::new();
    store.put("a", "1", &["red"]).unwrap();
    store.put("b", "2", &["blue"]).unwrap();
    assert_eq!(store.len(), 2);

    store.delete("a").unwrap();
    assert_eq!(store.len(), 1);
}

#[test]
fn delete_of_missing_key_returns_not_found() {
    let mut store = Store::new();
    let err = store.delete("missing").unwrap_err();
    assert_eq!(err, StoreError::NotFound("missing".to_string()));
}

#[test]
fn delete_then_put_same_key_succeeds() {
    let mut store = Store::new();
    store.put("a", "1", &["red"]).unwrap();
    store.delete("a").unwrap();
    store.put("a", "2", &["blue"]).unwrap();
    assert_eq!(store.get("a"), Some("2".to_string()));
    assert_eq!(store.keys_with_tag("red"), Vec::<String>::new());
    assert_eq!(store.keys_with_tag("blue"), vec!["a".to_string()]);
}

#[test]
fn delete_a_multitagged_key_clears_every_tag() {
    let mut store = Store::new();
    store.put("a", "1", &["red", "big"]).unwrap();
    store.delete("a").unwrap();
    assert_eq!(store.keys_with_tag("red"), Vec::<String>::new());
    assert_eq!(store.keys_with_tag("big"), Vec::<String>::new());
    assert_eq!(store.count_with_tag("red"), 0);
}
