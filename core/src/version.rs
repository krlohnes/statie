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
    /// Number of successfully executed `edit_file`/`write_file`/
    /// `revert_file` calls since the most recent `test` call -- incremented
    /// on each such call, and reset to 0 on every `test` call (pass or
    /// fail), committed at the same site as `green_step`/`green_filtered`/
    /// `last_test_cmd` (`core/src/session.rs`). Harness-owned: patches
    /// cannot touch it. Read by the coding skills' `implement` validation
    /// (`skills/src/coding/mod.rs`) to tell a `done` sent with edits made
    /// since the last (or only) test run apart from a `done` sent with
    /// nothing changed since a prior failing run. Survives a handoff to a
    /// new activation, the same as `green_step`/`last_test_cmd` (unlike
    /// `tool_calls`, which resets on activation). `#[serde(default)]` so
    /// state persisted before this field existed still deserializes.
    #[serde(default)]
    pub edits_since_test: u32,
    /// The harness step at which the most recent `think` tool call executed
    /// (configured or not -- see `Session`'s `think` interception in
    /// `core/src/session.rs`), set at the same call site as `tool_calls`.
    /// `None` until one has. Harness-owned: patches cannot touch it.
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub last_think_step: Option<u64>,
    /// The harness step of the most recent `test` call whose failure was
    /// identical to the previous run's -- i.e. the call at which
    /// `Session::note_test_result` (`core/src/session.rs`) reported a
    /// repeat (`repeated_failure_count >= 2`). Set at the same `test` call
    /// site as `green_step`: `Some(step)` on a repeat, `None` on a pass, and
    /// `None` on a fail with a fresh (non-repeating) summary. Harness-owned:
    /// patches cannot touch it. Read by the coding skills' `implement`
    /// validation (`skills/src/coding/mod.rs`) to run `think` once, ahead of
    /// the model's own next edit, right after a failure has recurred
    /// identically -- on a trap fixture where the acting model was
    /// unanimous and wrong, it never called `think` on its own.
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub repeated_failure_step: Option<u64>,
    /// Whether a `Fixer` is attached to this session (`Session::
    /// with_fixer`, BACKLOG.md item 4) -- true exactly when `try_fixes` can
    /// actually propose and verify candidates, mirroring how `thinker`
    /// being `None` makes every `think` call report itself unconfigured.
    /// Read by the coding skills' `implement_repeat_failure_thinks`
    /// (`skills/src/coding/mod.rs`) to decide whether the repeated-failure
    /// trigger substitutes `try_fixes` (when true) or `think` (when false,
    /// today's behaviour). Not persisted across a store round-trip in the
    /// usual sense -- it is set fresh from the attached `Fixer` every time
    /// `HarnessState` is constructed for a new activation, the same as
    /// `active_tools`. `#[serde(default)]` so state persisted before this
    /// field existed still deserializes (as `false`, matching a session
    /// with no fixer attached).
    #[serde(default)]
    pub try_fixes_available: bool,
    /// The harness step at which the most recent `try_fixes` tool call
    /// executed (fixer configured or not, proposer error or not -- see
    /// `Session`'s `try_fixes` interception in `core/src/session.rs`), set
    /// at the same call site as `tool_calls`. `None` until one has.
    /// Harness-owned: patches cannot touch it. A `try_fixes` call also sets
    /// `last_think_step` to the same step, so the once-per-streak repeated-
    /// failure gate (`last_think_step` vs `repeated_failure_step`) applies
    /// whichever of `think`/`try_fixes` fired most recently.
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub last_try_fixes_step: Option<u64>,
    /// How many consecutive `ask_user` actions the model has sent, ending in
    /// this committed step, whose `message` contains no `?` -- i.e. an empty
    /// or non-question `ask_user`, the shape `skills/src/coding/mod.rs`'s
    /// `ask_user_empty_note`/`ask_user_statement_note` convert to
    /// `Action::Continue`. Set from the model's raw parsed action, BEFORE
    /// `Skill::validate` runs and can rewrite that action (`core/src/
    /// session.rs`'s `step`, right before committing `next_harness`): `0`
    /// unless this step's own action was a non-question `ask_user`, in which
    /// case it is the previous committed value plus one -- so it counts a
    /// streak the way `repeated_failure_step` counts a streak of identical
    /// test failures, not just "was the last one this shape". Any other
    /// action (a tool call, a real question containing `?`, `done`, `yield`,
    /// a delegate) resets it to `0`. Harness-owned: patches cannot touch it.
    /// Read by the coding skills' `implement` validation (`skills/src/
    /// coding/mod.rs`'s early-park trigger) to convert the SECOND
    /// consecutive non-question `ask_user` into a `think` call before the
    /// continue-repeat cap parks the run on the third (DECISIONS.md D4:
    /// six of nine parks in a 96-run study happened this way, before model
    /// call 15, with no test failure open, so `repeated_failure_step` never
    /// fires). `#[serde(default)]` so state persisted before this field
    /// existed still deserializes.
    #[serde(default)]
    pub consecutive_non_question_asks: u32,
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
