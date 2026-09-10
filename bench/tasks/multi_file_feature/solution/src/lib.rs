pub mod error;
pub mod index;
pub mod query;
pub mod storage;

use error::StoreError;
use index::Index;
use storage::Storage;

/// A small key-value store: `Storage` owns the records, `Index` keeps a
/// tag -> keys secondary index in sync with it, and `query` reads across
/// both. `Store` is the entry point that keeps the two halves coordinated.
#[derive(Debug, Default)]
pub struct Store {
    storage: Storage,
    index: Index,
}

impl Store {
    pub fn new() -> Self {
        Store {
            storage: Storage::new(),
            index: Index::new(),
        }
    }

    /// Inserts `value` under `key` with the given `tags`, updating both the
    /// storage and the tag index. Fails if `key` already exists or if any
    /// tag is empty.
    pub fn put(&mut self, key: &str, value: &str, tags: &[&str]) -> Result<(), StoreError> {
        if tags.iter().any(|t| t.is_empty()) {
            return Err(StoreError::EmptyTag);
        }
        let owned_tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        self.storage.put(key, value, &owned_tags)?;
        for tag in &owned_tags {
            self.index.add(tag, key);
        }
        Ok(())
    }

    /// Returns the value stored under `key`, if any.
    pub fn get(&self, key: &str) -> Option<String> {
        query::get_value(&self.storage, key)
    }

    /// Removes `key` from the store, along with every tag entry it held in
    /// the index. Fails with `StoreError::NotFound` if `key` is not
    /// present.
    pub fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        query::delete_key(&mut self.storage, &mut self.index, key)?;
        Ok(())
    }

    /// Returns every key currently tagged with `tag`.
    pub fn keys_with_tag(&self, tag: &str) -> Vec<String> {
        query::keys_with_tag(&self.index, tag)
    }

    /// Returns how many keys currently carry `tag`.
    pub fn count_with_tag(&self, tag: &str) -> usize {
        query::count_with_tag(&self.index, tag)
    }

    /// Returns the total number of records in the store.
    pub fn len(&self) -> usize {
        query::total_records(&self.storage)
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}
