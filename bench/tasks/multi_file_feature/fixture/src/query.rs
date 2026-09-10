use crate::index::Index;
use crate::storage::Storage;

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
