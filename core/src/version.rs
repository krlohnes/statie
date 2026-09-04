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
    /// Set when the most recent tool output overflowed the observation
    /// budget and had to be truncated; cleared back to `None` once a tool
    /// output fits. `#[serde(default)]` so state persisted before this
    /// field existed still deserializes.
    #[serde(default)]
    pub last_overflow: Option<String>,
    /// The currently pinned guide, if any (milestone 9). Harness-owned:
    /// patches cannot touch it. `#[serde(default)]` so state persisted
    /// before this field existed still deserializes.
    #[serde(default)]
    pub pinned: Option<Pinned>,
    /// The harness step at which the most recent green checkpoint
    /// (`core/src/green.rs`) was taken -- set by the session right when a
    /// `test` call reports `result: pass` (mirroring
    /// `Session::green_checkpoint`, but committed/visible to skills, unlike
    /// that in-memory-only field), and cleared back to `None` as soon as a
    /// later `test` result is a fail. Harness-owned: patches cannot touch
    /// it. Read by the coding skills' `implement` validation
    /// (`skills/src/coding/mod.rs`) to recognize a `done` sent right after
    /// verification passed, even while a plan step is still open.
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub green_step: Option<u64>,
    /// Whether the passing `test` run that set `green_step` had tests
    /// filtered out (its output contains "test(s) were filtered out by
    /// this command", the note `tools/src/test_tool.rs`'s `filtered_out_note`
    /// appends) -- `false` on a fail, and set at the same call site as
    /// `green_step` on every `test` call. Harness-owned: patches cannot
    /// touch it. Read by the `review` skill's `validate`
    /// (`skills/src/coding/mod.rs`) to reject an approval made on a
    /// filtered pass, since part of the suite never ran. `#[serde(default)]`
    /// so state persisted before this field existed still deserializes.
    #[serde(default)]
    pub green_filtered: bool,
    /// Number of tool calls that actually executed in the current
    /// activation: incremented on every executed `Action::Tool`, including
    /// a skipped-redundant call (7.7/7.8) and a paged read (7.8's special
    /// case), since both are recognized calls whose outcome is already
    /// known -- just not on a rejection (`Active::reject`), which never
    /// commits and runs nothing. Reset to 0 in `activate_inner` (a new
    /// activation starts with a clean slate). Harness-owned: patches cannot
    /// touch it. Read by the `review` skill's `validate`
    /// (`skills/src/coding/mod.rs`) to require at least one executed tool
    /// call before an approval (`done: true`) is accepted. `#[serde(default)]`
    /// so state persisted before this field existed still deserializes.
    #[serde(default)]
    pub tool_calls: u64,
    /// The `cmd` the most recent `test` call ran, as given in its args
    /// (`args.cmd`/`args.command`) -- `None` when that call had no `cmd`
    /// (auto-detected by `tools/src/test_tool.rs`) or when no `test` call
    /// has run yet. Set at the same call site as `green_step`, on every
    /// `test` call. Harness-owned: patches cannot touch it. Read by the
    /// `review` skill's `validate` (`skills/src/coding/mod.rs`) so a blind
    /// approval (`done: true`, no findings, no tool call this activation)
    /// can be converted into a substituted `test` call that reruns the same
    /// command implement last verified with, rather than a fresh
    /// auto-detected one. `#[serde(default)]` so state persisted before
    /// this field existed still deserializes.
    #[serde(default)]
    pub last_test_cmd: Option<String>,
}

/// A guide pinned into the prompt's Pinned section, with a step-counted
/// expiry. Set and refreshed only by the `guide` tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pinned {
    /// The pinned guide's name.
    pub name: String,
    /// The committed step at which this pin expires (silently, except for
    /// the `pinned:` line disappearing from the Harness View).
    pub expires_at_step: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    pub id: u64,
    pub text: String,
    pub acked: bool,
}

// Budgets lives in tokenizer.rs but is re-exported at crate root; imported here for HarnessState.
use crate::tokenizer::Budgets;
