//! Versions and committed state.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A monotonically increasing state version. Starts at 0 for the initial state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version(pub u64);

/// A committed harness/agent state pair.
///
/// `harness` is written only by core's commit path (`pub(crate)` mutation).
/// `agent` is written only through `ValidatedPatch<S>`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Committed<H, S> {
    pub harness: H,
    pub agent: S,
}

/// Convenience alias for the common case: harness state paired with a skill workspace.
pub type CommittedState<W> = Committed<HarnessState, W>;

/// Harness-owned state: inbox, active tools, budgets, activation, step counter.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct HarnessState {
    /// User messages; unacked ones render in the prompt.
    pub inbox: Vec<InboxItem>,
    /// Tool names available to the current activation.
    pub active_tools: Vec<String>,
    pub budgets: Budgets,
    /// Current skill name.
    pub activation: String,
    /// Committed step counter.
    pub step: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    pub id: u64,
    pub text: String,
    pub acked: bool,
}

// Budgets lives in tokenizer.rs but is re-exported at crate root; imported here for HarnessState.
use crate::tokenizer::Budgets;
