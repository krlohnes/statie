use crate::error::StoreError;
use crate::index::Index;
use crate::storage::{Record, Storage};

/// Returns the value stored under `key`, or `None` if there is no such key.
pub fn get_value(storage: &Storage, key: &str) -> Option<String> {
    storage.get(key).map(|record| record.value.clone())
}

/// Returns every key tagged with `tag`, in sorted order.
pub fn keys_with_tag(index: &Index, tag: &str) -> Vec<String> {
    index.keys_for_tag(tag)
}

/// Returns how many keys currently carry `tag`.
pub fn count_with_tag(index: &Index, tag: &str) -> usize {
    index.count_for_tag(tag)
}

/// Returns the total number of records in the store.
pub fn total_records(storage: &Storage) -> usize {
    storage.len()
}

/// Removes `key` from `storage`, then removes it from `index` under every
/// tag the removed record carried. Fails with `StoreError::NotFound` if
/// `key` was not present, leaving both `storage` and `index` untouched.
pub fn delete_key(
    storage: &mut Storage,
    index: &mut Index,
    key: &str,
) -> Result<Record, StoreError> {
    let record = storage.remove(key)?;
    for tag in &record.tags {
        index.remove(tag, key);
    }
    Ok(record)
}
