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
    /// How many consecutive committed steps, ending in this one, had the
    /// harness-internal `Action::Continue` (`core/src/model.rs`) as their
    /// FINAL action -- i.e. what actually committed, after `Skill::validate`
    /// has run and possibly rewritten the model's raw action, not the raw
    /// action itself (contrast `consecutive_non_question_asks`, which is
    /// read BEFORE `validate`). Set at the same commit site
    /// (`core/src/session.rs`'s `step`, right before committing
    /// `next_harness`): the previous committed value plus one when the
    /// FINAL action being committed is `Action::Continue`; any other action
    /// resets it to `0`. Harness-owned: patches cannot touch it. Read by
    /// `skills/src/coding/mod.rs`'s `premature_done_note` to tell a first
    /// premature `done` from a repeat: bench short_fixer_b22 trap_rounding:
    /// seven plan-walk runs across four batteries, one landed -- the first
    /// refusal's note offered marking the step done as a way to move on,
    /// and the model took it literally, sending `done` again with the
    /// status change and no tool call, three to twelve times running, until
    /// the non-progress cap parked it; a repeat refusal now drops the
    /// status change from the patch and demands a real tool call instead.
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub consecutive_continues: u32,
    /// The normalized failure summary (see `core/src/session.rs`'s
    /// `normalize_failure_summary`) of the most recent FAILING `test` call,
    /// capped at 600 characters. Set at the same `test` call site as
    /// `repeated_failure_step`: `Some(summary)` on a fail, cleared back to
    /// `None` on a pass. This is a fact about the sandbox, not about the
    /// current activation, so it deliberately survives `activate`/
    /// `activate_forced` (both branches of `activate_inner`) -- unlike
    /// `Session::last_test_failure`, which DOES reset every activation
    /// because it only backs the repeated-failure counter. The two are
    /// allowed to diverge across a re-activation for exactly this reason;
    /// see `last_test_failure`'s doc. Harness-owned: patches cannot touch
    /// it. Read by `execute_try_fixes`
    /// (`core/src/session.rs`) as the source of truth for both the "no
    /// failing test on record" check and the `FixInput.failure` text handed
    /// to the fixer, and by the coding skills'
    /// `implement_repeat_failure_thinks` (`skills/src/coding/mod.rs`) via
    /// `classify_failure`/`repeat_failure_question` to shape the
    /// substituted `think` question to the kind of failure (compile error,
    /// assertion, panic, timeout) instead of asking every repeat the same
    /// arithmetic-trace question (DECISIONS.md D9, BACKLOG.md item 18).
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes.
    #[serde(default)]
    pub last_failure_summary: Option<String>,
    /// Mirrors `Session::last_skip_key`/`skip_repeat_count`
    /// (`core/src/session.rs`), harness-owned so a skill's `validate` can
    /// see it: `Some(SkippedRead { .. })` on every `skipped_redundant` step
    /// (7.7/7.8's "do NOT run the tool" path in `Active::execute`),
    /// recording that step's `(tool, read_call_key)` and the consecutive
    /// count (`Session::skip_repeat_count`, so always `>= 1` whenever this
    /// is `Some`); cleared back to `None` on any step that is not a skip AND
    /// is not a `think` step (`core/src/session.rs`'s `is_think_action`,
    /// unit 23's correction) -- the same reset rule `last_skip_key` follows
    /// (`Active::step`, right alongside that field's own reset). A `think`
    /// step (model-chosen or substituted by `implement_repeated_skip_
    /// thinks`) leaves this field exactly as it was: a think between two
    /// identical skips must not clear it, or the model could alternate
    /// skip/think forever without ever reaching `CONTINUE_REPEAT_CAP`. Read
    /// by the coding skills'
    /// `implement_repeated_skip_thinks` (`skills/src/coding/mod.rs`) to
    /// substitute a `think` call, once, the second time the model asks to
    /// re-read something already skipped -- unlike `last_skip_key`, which
    /// only ever compares to the SAME step's fresh key (so it cannot tell
    /// `validate` "a skip happened", only core's own loop guard can act on
    /// it), this field survives into the committed state a skill's
    /// `validate` actually sees. `#[serde(default)]` so state persisted
    /// before this field existed still deserializes.
    #[serde(default)]
    pub last_skip: Option<SkippedRead>,
    /// Mirrors `Session::last_tool_error_key`/`tool_error_repeat_count`
    /// (`core/src/session.rs`), harness-owned so a skill's `validate` can
    /// see it -- the same treatment `last_skip` gets for the repeated-skip
    /// guard, unit 23 part (b): `Some(RepeatedToolError { .. })` on every
    /// `tool_error` System entry (the generic tool-execution error path in
    /// `Active::execute`, or the `think`/`try_fixes` interceptions' own
    /// invalid-argument/no-failing-test tool errors), recording that step's
    /// `(tool, args, reason)` and the consecutive count (`Session::
    /// tool_error_repeat_count`, so always `>= 1` whenever this is `Some`);
    /// cleared back to `None` on any step that is not a `tool_error` AND is
    /// not a `think` step (`core/src/session.rs`'s `is_think_action`, unit
    /// 23's correction) -- the same reset rule `last_tool_error_key`
    /// follows (`Active::step`, right alongside that field's own reset),
    /// and on activation, the same as `last_skip`. A `think` step
    /// (model-chosen or substituted by `implement_repeated_error_thinks`)
    /// leaves this field exactly as it was: without this exemption the
    /// substituted think would itself clear the field, letting the model
    /// alternate error/think/error/think forever with every repeat looking
    /// like a fresh occurrence, never reaching `CONTINUE_REPEAT_CAP`. `args`
    /// is the committed action's own tool-call
    /// arguments (`Action::Tool { args, .. }`), not read back from the
    /// `tool_error` entry's payload (which carries only `name`/`reason`) --
    /// every `tool_error` entry is pushed from within a tool-call dispatch
    /// path, so the committed action is always `Action::Tool` whenever this
    /// is set. Read by the coding skills' `implement_repeated_error_thinks`
    /// (`skills/src/coding/mod.rs`) to substitute a `think` call, once, the
    /// second time the model repeats the SAME failing tool call. `#[serde(
    /// default)]` so state persisted before this field existed still
    /// deserializes.
    #[serde(default)]
    pub last_tool_error: Option<RepeatedToolError>,
    /// Unit 25 part 3: one "stalled on P" streak spanning BOTH a refused
    /// `skipped_redundant` read of a path and a failed `tool_error` from an
    /// edit tool (`edit_file`/`write_file`/`revert_file`) on the SAME path,
    /// in either order -- a model that alternates re-reading a file and
    /// failing to edit it is going back and forth on that one path, even
    /// though neither `last_skip`'s nor `last_tool_error`'s own streak
    /// (each keyed on an identical call/shape recurring) ever sees the same
    /// shape twice in a row. Written by core (`Active::step`, right
    /// alongside the `last_skip`/`last_tool_error` updates) whenever a skip
    /// or an edit-tool error lands on a path: `Some(Stall { path, count,
    /// step })` with `count` incremented from the previous stall when it
    /// named the SAME path and no executed (non-error, non-skip, non-think)
    /// step has happened since; started fresh at `count: 1` otherwise.
    /// Cleared back to `None` by an executed step on a DIFFERENT path (or
    /// one that names no path), but NOT by a `think` step (the same
    /// exemption `last_skip`/`last_tool_error` get) and NOT by a skip or
    /// edit-tool error on the SAME path (that advances the streak instead).
    /// `#[serde(default)]` so state persisted before this field existed
    /// still deserializes. Read by the coding skills' `implement_stall_
    /// thinks` (`skills/src/coding/mod.rs`) to substitute a `think` call at
    /// `count >= 2` (`STALLED_PATH_QUESTION`), and by core's own
    /// `park_non_progress_cap` path to park at `count >= 6`.
    #[serde(default)]
    pub last_stall: Option<Stall>,
    /// Unit 26 (situational rules): the name of the most recently
    /// SUCCESSFULLY executed tool call this activation -- e.g.
    /// `"read_file"`, `"edit_file"`, `"test"`, `"think"`, `"try_fixes"` --
    /// set at each of the three success-commit sites that already
    /// increment `tool_calls` inline (`Active::execute`'s generic success
    /// path, `execute_think`, `execute_try_fixes`; `core/src/session.rs`),
    /// right alongside that increment. Deliberately NOT set on a
    /// `skipped_redundant` call (`record_tool_call`'s three callers) or a
    /// `tool_error` -- those are `last_skip`/`last_tool_error`'s job, and a
    /// skip/error is not "the tool ran", it's the harness declining to run
    /// it or the tool itself failing. Never cleared back to `None` (unlike
    /// `last_skip`/`last_tool_error`/`last_stall`, which reset on an
    /// unrelated step) -- there is always a well-defined "most recent
    /// successful tool call" once one has happened, and nothing needs it to
    /// go stale. `None` until the first tool call succeeds this
    /// activation. Harness-owned: patches cannot touch it. Read by
    /// `skills/src/coding/procedure.rs`'s `situation()` to distinguish the
    /// `read`/`search`/`edit_ok`/`test_pass`/`test_fail_*` nodes (paired
    /// with `cur.agent.tests` for the test outcome) from one another when
    /// none of `last_skip`/`last_tool_error`/`last_stall`/`last_think_step`
    /// already pins the situation. `#[serde(default)]` so state persisted
    /// before this field existed still deserializes.
    #[serde(default)]
    pub last_tool: Option<String>,
    /// Unit 27 ("no green cap"): the count of `test` tool runs whose result
    /// was a failure since the last PASSING `test` run in this activation --
    /// incremented on every failing `test` call, at the same call site as
    /// `green_step`/`repeated_failure_step` (`core/src/session.rs`), and
    /// reset to `0` both on a passing `test` call and on a fresh activation
    /// (`activate_inner`), mirroring `last_think_step`'s own per-activation
    /// reset. Harness-owned: patches cannot touch it. At the failing `test`
    /// call that brings this count to `NO_GREEN_PARK_AT`
    /// (`core/src/session.rs`), core parks through `park_non_progress_cap`
    /// with the shape `"no green in {NO_GREEN_PARK_AT} test runs"`. Read by
    /// the coding skills' `implement_no_green_thinks`
    /// (`skills/src/coding/mod.rs`) to substitute a `think` call once this
    /// reaches `NO_GREEN_THINK_AT`. `#[serde(default)]` so state persisted
    /// before this field existed still deserializes.
    #[serde(default)]
    pub fails_since_green: u32,
}

/// One skipped-redundant read, recorded on `HarnessState::last_skip`. See
/// that field's doc for when it is set/cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedRead {
    /// The tool name of the skipped call (e.g. `"read_file"`, `"search"`).
    pub tool: String,
    /// The call's canonical key, `core/src/session.rs`'s `read_call_key`
    /// (tool name + canonical JSON of its args) -- used to tell whether a
    /// later call is the SAME skipped call recurring.
    pub key: String,
    /// Consecutive count of this same skip, mirroring `Session::
    /// skip_repeat_count` at the step that set this value -- always `>= 1`.
    pub count: u32,
    /// The harness step at which this skip was recorded.
    pub step: u64,
}

/// One repeated identical-SHAPE `tool_error`, recorded on `HarnessState::
/// last_tool_error`. See that field's doc for when it is set/cleared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepeatedToolError {
    /// The tool name the error came from (e.g. `"edit_file"`, `"think"`).
    /// Unit 39: for the `"not a tool"` class (see `shape` below) this is
    /// NOT a stable identity for the streak -- it is only the LATEST
    /// rejected call's own `name` (possibly empty), which may differ from
    /// the `name` any earlier call in the same streak carried, since the
    /// class groups every call that named no usable tool regardless of what
    /// (if anything) it put in `name`.
    pub tool: String,
    /// The failing call's own arguments, exactly as the committed action
    /// carried them -- compared against a later `Action::Tool { name, args
    /// }` to tell whether it is the SAME call recurring.
    pub args: serde_json::Value,
    /// The FULL error reason string of the LATEST (most recent) error in
    /// this streak, as reported on the `tool_error` System entry's own
    /// payload -- unit 25: this is the exact text, not the shape, since the
    /// substituted `think` question (`REPEATED_ERROR_QUESTION`,
    /// `skills/src/coding/mod.rs`) quotes it verbatim.
    pub reason: String,
    /// Unit 25: `error_shape(reason)` -- the SHAPE the streak identity and
    /// comparison actually key on (`core/src/session.rs`'s `error_shape`),
    /// so two errors of the same kind but naming a different line number or
    /// file still count as the SAME streak. Read by the coding skills'
    /// `implement_repeated_error_thinks` (`skills/src/coding/mod.rs`) to
    /// match on shape rather than identical args. Unit 39: a `tool_rejected`
    /// entry whose reason says the call named no usable tool at all
    /// (`core/src/session.rs`'s `is_not_a_tool_rejection`) is keyed as the
    /// fixed class `("<not a tool>", "not a tool")` instead -- this field is
    /// then exactly `"not a tool"`, and the streak this record's `count`
    /// advances spans every such call regardless of what `name` (if any)
    /// each one carried; `tool` above still records only the latest one.
    pub shape: String,
    /// Consecutive count of this same identical-shape `(tool, shape)`
    /// error, mirroring `Session::tool_error_repeat_count` at the step that
    /// set this value -- always `>= 1`.
    pub count: u32,
    /// The harness step at which this error was recorded.
    pub step: u64,
}

/// One "stalled on P" streak, recorded on `HarnessState::last_stall`. See
/// that field's doc for when it is set/incremented/cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Stall {
    /// The path the model is going back and forth on.
    pub path: String,
    /// Consecutive count of skip-or-edit-error events on this path with no
    /// executed step on it in between -- always `>= 1`.
    pub count: u32,
    /// The harness step at which this stall was last recorded.
    pub step: u64,
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
