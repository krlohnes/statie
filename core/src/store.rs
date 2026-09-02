//! The `StateStore` trait: pluggable, version-checked persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::StoreError;
use crate::model::Action;
use crate::error::PatchError;
use crate::patch::{PatchStrategy, ValidatedPatch};
use crate::version::{CommittedState, HarnessState, Version};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggedPatch {
    pub version: Version,
    pub patch: Value,
    pub action: Action,
    pub harness: HarnessState,
}

/// A type-erased patch application function, so `replay` is callable
/// through `dyn StateStore<W>`.
pub type ApplyFn<'a, W> = &'a (dyn Fn(&W, &Value) -> Result<W, PatchError> + Sync);

/// Build an [`ApplyFn`]-compatible closure for strategy `P`.
pub fn apply_fn<W, P: PatchStrategy<W>>() -> impl Fn(&W, &Value) -> Result<W, PatchError> + Sync {
    move |cur: &W, raw: &Value| {
        let patch: P::Patch =
            serde_json::from_value(raw.clone()).map_err(|e| PatchError::Malformed(e.to_string()))?;
        P::apply(cur, &patch)
    }
}

#[async_trait]
pub trait StateStore<W>: Send + Sync {
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError>;

    /// Initializes the store at version 0.
    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError>;

    /// Atomic: persists `patch.next()` as agent, `harness` as harness, and
    /// appends `(patch.raw(), action)` to the patch log.
    async fn commit(
        &self,
        patch: ValidatedPatch<W>,
        harness: HarnessState,
        action: &Action,
        expect: Version,
    ) -> Result<Version, StoreError>;

    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError>;

    /// Reapply the patch log from the initial snapshot up to `to`, using
    /// `apply` (see [`apply_fn`]) to turn each logged patch into the next
    /// agent state. The harness region is taken from the logged entry.
    async fn replay(&self, to: Version, apply: ApplyFn<'_, W>)
        -> Result<CommittedState<W>, StoreError>;

    /// Overwrite the harness region only (used for inbox pushes and
    /// activation switches). Bumps no version.
    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError>;
}
