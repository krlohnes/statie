//! `statie-patch`: `PatchStrategy<S>` implementations for `statie-core`.
//!
//! Four strategies:
//! - [`Replace`]: whole-state replacement.
//! - [`JsonMergePatch`] (RFC 7386): the project default. Partial JSON merge.
//! - [`JsonPatch`] (RFC 6902): a list of JSON Patch operations.
//! - [`Typed`]: a list of strongly-typed, skill-defined operations.

mod json_patch_strategy;
mod merge;
mod replace;
mod typed;
mod util;

pub use json_patch_strategy::JsonPatch;
pub use merge::{JsonMergePatch, MergePatch};
pub use replace::Replace;
pub use typed::{Typed, TypedOp};

/// Shared test fixtures for the strategy unit tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct Nested {
        pub flag: bool,
        pub note: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct Ws {
        pub name: String,
        pub count: u64,
        pub tags: Vec<String>,
        pub nested: Nested,
    }

    impl Ws {
        pub fn sample() -> Self {
            Ws {
                name: "alice".to_string(),
                count: 1,
                tags: vec!["a".to_string(), "b".to_string()],
                nested: Nested {
                    flag: true,
                    note: Some("hi".to_string()),
                },
            }
        }
    }
}
