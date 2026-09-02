//! statie-store: pluggable persistence backends for `statie-core`'s
//! `StateStore` trait.

mod jsonl;
mod memory;
mod sqlite;
#[cfg(test)]
mod test_support;

pub use jsonl::JsonlLog;
pub use memory::InMemoryStore;
pub use sqlite::SqliteStore;
