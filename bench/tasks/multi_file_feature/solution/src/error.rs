use std::fmt;

/// Errors produced by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A `put` used a key that already exists.
    DuplicateKey(String),
    /// A tag name was empty.
    EmptyTag,
    /// A `delete` (or other keyed lookup that must exist) used a key that
    /// is not present.
    NotFound(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::DuplicateKey(key) => write!(f, "duplicate key: {key}"),
            StoreError::EmptyTag => write!(f, "tag must not be empty"),
            StoreError::NotFound(key) => write!(f, "key not found: {key}"),
        }
    }
}

impl std::error::Error for StoreError {}
