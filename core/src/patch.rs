//! Patch typestate: `RawPatch → CheckedPatch<S> → ValidatedPatch<S>` (A2, A3,
//! A6, B1).

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::PatchError;
use crate::version::Version;

/// The parsed `patch` field: syntactically schema-shaped JSON, not yet
/// applied.
#[derive(Debug, Clone)]
pub struct RawPatch(pub Value);

/// Pluggable state-update strategy. `Patch` is the wire type the model is
/// constrained to (its `JsonSchema` is what gets sent to the decoder).
pub trait PatchStrategy<S>: Send + Sync + 'static {
    type Patch: Serialize + DeserializeOwned + schemars::JsonSchema + Send + Sync;

    /// e.g. "json-merge-patch (RFC 7386)"
    fn name() -> &'static str;
    /// One paragraph for the system prompt.
    fn describe() -> &'static str;
    fn apply(cur: &S, patch: &Self::Patch) -> Result<S, PatchError>;
}

/// Zero-sized witness: the patch JSON was deserialized into `P::Patch` and
/// successfully applied. Private constructor.
pub struct SchemaChecked(());

/// Zero-sized witness: the prompt that produced this output was within
/// budget. Private constructor.
pub struct WithinBudget(());

/// Zero-sized witness: every unacked inbox id is present in `ack_inbox`.
/// Private constructor — see `check_inbox`.
pub struct InboxAcked(());

impl InboxAcked {
    pub(crate) fn new() -> Self {
        InboxAcked(())
    }
}

impl WithinBudget {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        WithinBudget(())
    }
}

/// A patch after strategy application and schema check. `next` is the
/// candidate agent state; nothing has committed yet.
pub struct CheckedPatch<S> {
    raw: Value,
    next: S,
    version: Version,
    _w: SchemaChecked,
}

impl<S> CheckedPatch<S> {
    pub(crate) fn new(raw: Value, next: S, version: Version) -> Self {
        CheckedPatch {
            raw,
            next,
            version,
            _w: SchemaChecked(()),
        }
    }

    pub fn next(&self) -> &S {
        &self.next
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Exposed for the loop unit's B1 version-threading checks.
    #[allow(dead_code)]
    pub(crate) fn version(&self) -> Version {
        self.version
    }

    pub(crate) fn into_parts(self) -> (Value, S, Version) {
        (self.raw, self.next, self.version)
    }
}

/// A committable patch. Not `Clone`; only `StateStore::commit` consumes it.
/// The only way to produce one is the default validation path (`CheckedPatch`
/// → skill rules → stamped).
pub struct ValidatedPatch<S> {
    raw: Value,
    next: S,
    version: Version,
}

impl<S> ValidatedPatch<S> {
    /// Direct constructor for callers (e.g. `/edit` user patches) that
    /// already hold a checked `(raw, next, version)` triple without going
    /// through `CheckedPatch`. Unused until the Session/Active loop unit
    /// lands; kept here per the architecture's typestate contract.
    #[allow(dead_code)]
    pub(crate) fn new(raw: Value, next: S, version: Version) -> Self {
        ValidatedPatch { raw, next, version }
    }

    pub(crate) fn from_checked(checked: CheckedPatch<S>) -> Self {
        let (raw, next, version) = checked.into_parts();
        ValidatedPatch { raw, next, version }
    }

    /// The version this patch was validated against.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The new agent state (stores persist this).
    pub fn next(&self) -> &S {
        &self.next
    }

    /// The patch JSON (stores log this).
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub fn into_parts(self) -> (Value, S, Version) {
        (self.raw, self.next, self.version)
    }
}

impl<S: std::fmt::Debug> std::fmt::Debug for ValidatedPatch<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedPatch")
            .field("raw", &self.raw)
            .field("next", &self.next)
            .field("version", &self.version)
            .finish()
    }
}
