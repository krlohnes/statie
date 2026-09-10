use std::fmt;

/// Errors produced by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A `put` used a key that already exists.
    DuplicateKey(String),
    /// A tag name was empty.
    EmptyTag,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::DuplicateKey(key) => write!(f, "duplicate key: {key}"),
            StoreError::EmptyTag => write!(f, "tag must not be empty"),
        }
    }
}

impl std::error::Error for StoreError {}
