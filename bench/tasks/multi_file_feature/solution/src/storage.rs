use std::collections::BTreeMap;

use crate::error::StoreError;

/// A record stored under a key: its value and the tags attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub value: String,
    pub tags: Vec<String>,
}

/// The backing key -> record map. Owns the actual data; `Index` only keeps
/// tag -> key lookups that must stay in sync with this.
#[derive(Debug, Default)]
pub struct Storage {
    records: BTreeMap<String, Record>,
}

impl Storage {
    pub fn new() -> Self {
        Storage {
            records: BTreeMap::new(),
        }
    }

    /// Inserts a new record under `key`. Fails if `key` is already present.
    pub fn put(&mut self, key: &str, value: &str, tags: &[String]) -> Result<(), StoreError> {
        if self.records.contains_key(key) {
            return Err(StoreError::DuplicateKey(key.to_string()));
        }
        self.records.insert(
            key.to_string(),
            Record {
                value: value.to_string(),
                tags: tags.to_vec(),
            },
        );
        Ok(())
    }

    /// Returns the record stored under `key`, if any.
    pub fn get(&self, key: &str) -> Option<&Record> {
        self.records.get(key)
    }

    /// Returns whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        self.records.contains_key(key)
    }

    /// Removes the record stored under `key` and returns it. Fails with
    /// `StoreError::NotFound` if `key` is not present.
    pub fn remove(&mut self, key: &str) -> Result<Record, StoreError> {
        self.records
            .remove(key)
            .ok_or_else(|| StoreError::NotFound(key.to_string()))
    }

    /// Returns the number of records currently stored.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// All keys currently stored, in sorted order.
    pub fn keys(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }
}
