//! Switches for the non-progress guards audited in statie-spec.md "10.
//! After it works: the rule audit" (milestone 12). Every boolean field
//! defaults to `true`, so a config with no `guards` section (or one missing
//! individual fields) behaves exactly as it did before this type existed --
//! except `done_stays_done`, which the first bench battery's audit turned
//! off by default (see its own doc below): the rule rejected the model
//! twelve times over for moving its "run cargo test" plan step back to
//! in-progress after an edit, until the retry cap parked it as a failure
//! with the crate already green, and the audit showed the switch off no
//! worse; and `situational_rules` (unit 26), which defaults to `false` --
//! an unproven, opt-in rendering change (localised per-step guidance
//! instead of the skill's whole rules block) a battery can compare against
//! today's behaviour, not yet a rule with a battery-backed default of its
//! own to keep. `review_rounds_cap` (unit 49) is not a switch -- it is a
//! count, default `3` (see its own doc below).

use serde::{Deserialize, Serialize};

/// One switch per audited rule, plus `review_rounds_cap` (unit 49), a count
/// rather than a switch. All boolean fields default `true` except
/// `done_stays_done` and `situational_rules` (see the module doc); flipping
/// one to `false` disables that rule and nothing else. See each field's doc
/// comment for what "off" means precisely, and `ARCHITECTURE.md` "Guards
/// against non-progress" for what each rule is guarding against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guards {
    /// Session rule 7.8: an identical read-only call since the last
    /// mutation is not re-run -- the patch commits, the call is skipped,
    /// and a `skipped_redundant` event is logged. Off: the call runs
    /// normally every time; no skip, no event.
    pub redundant_reads: bool,
    /// Consecutive read-only calls whose raw patch wrote nothing
    /// (`Skill::silent_read_cap`, tracked by `Session::silent_reads`). Off:
    /// the counter may still be maintained, but it never causes a
    /// rejection.
    pub silent_reads: bool,
    /// The per-path read cap (`Session::note_tool_call_for_read_guard`,
    /// four reads of one path with no edit in between). Off: no rejection
    /// and no note from this rule.
    pub read_guard: bool,
    /// Session rule 7.7: an exact repeat of the most recently run
    /// `shell`/`test` command, with nothing mutated since, is not re-run --
    /// the patch commits, the call is skipped, and the cached result is
    /// shown. Off: the command re-runs normally every time.
    pub redundant_verify: bool,
    /// `plan_rules` in the coding skills: a plan step that is `done` may
    /// not be moved back to `pending`/`in_progress`. Off: reopening is
    /// allowed. Defaults to `false` (unlike every other field here): the
    /// first bench battery's audit showed a local model reopening a `done`
    /// "run cargo test" step after an edit, having this rule reject it
    /// twelve times running until the retry cap parked the run as a
    /// failure with the crate already green -- the audit showed the switch
    /// off is no worse.
    pub done_stays_done: bool,
    /// The commitment read cap (`COMMITMENT_READ_CAP` = 2): reads against a
    /// set commitment beyond the cap are rejected and clear it. Off: reads
    /// while a commitment is set neither count toward a cap nor clear the
    /// commitment; instead the harness appends a short note to the
    /// observation saying the commitment is still pending.
    pub commitment_read_cap: bool,
    /// `CodingWorkspace::seen` (milestone 10): the harness writes it from
    /// `read_file`/`edit_file`/`write_file`/`revert_file` calls
    /// (`common_after_tool` in `skills/src/coding/mod.rs`). Off: the field
    /// is never written by the harness and stays empty for the life of the
    /// session -- a model patch touching it is still stripped the same as
    /// always (it remains harness-owned), there is just nothing for the
    /// harness itself to write there. Added for BACKLOG.md item 2: running
    /// the battery with this off isolates model-owned state (`files`,
    /// `recent_summary`, `plan`) from harness-owned state so the two can be
    /// measured against each other with the model, budgets, and window
    /// fixed.
    pub seen_map: bool,
    /// `CodingWorkspace::decision` (milestone 10 / repeated-failure think):
    /// the harness writes it from a `think` tool call's output and clears
    /// it when a `test` call passes (`common_after_tool`). Off: neither
    /// happens -- the field stays `null` for the life of the session. The
    /// `think` tool itself is unaffected: it still runs (including the
    /// automatic substitution in `implement_repeat_failure_thinks`) and its
    /// answer still reaches the model as the tool call's own observation;
    /// only the second, persistent copy in `decision` is switched off.
    /// Added for BACKLOG.md item 2, same rationale as `seen_map`.
    pub decision_field: bool,
    /// `CodingWorkspace::tests` (BACKLOG.md item 5): the harness writes it,
    /// replaced whole, from every successful `test` tool call
    /// (`common_after_tool`) -- the command, whether it passed, up to 8
    /// failing test names, and the first recognized error line. Off:
    /// `common_after_tool` writes no `tests` patch at all -- the field
    /// stays `null` for the life of the session. The field stays harness-
    /// owned either way (a model patch touching it is still stripped the
    /// same as always); this switch only controls whether the harness
    /// itself writes to it. Independent of `decision_field`: a `test`
    /// call's `decision`-clearing half (when `decision_field` is on) still
    /// applies regardless of this switch.
    pub tests_field: bool,
    /// `review`-only (item 12: grounded review findings): a send-back
    /// finding must cite a `path:line` the harness can check -- `path`
    /// present in the sandbox (`CodingWorkspace::files` or `::seen`) and
    /// `line` within the file's known line count. On, an ungrounded finding
    /// is dropped (with a note saying which ones and why), and a send-back
    /// left with no grounded findings at all is refused instead. Off
    /// restores the current behaviour exactly: every finding passes through
    /// unchanged, with no grounding check at all.
    pub grounded_findings: bool,
    /// Unit 26 (situational rules): `plan`/`implement`/`review`'s
    /// instructions (`skills/src/coding/mod.rs`'s `spec()`) render only the
    /// universal bullets (what holds in every situation), and each skill's
    /// `Skill::situational` returns `procedure::render(procedure::
    /// situation(cur))` -- localised, 2-hop guidance derived from harness-
    /// owned state (`skills/src/coding/procedure.rs`), rendered as its own
    /// `## Now` section (`core/src/session.rs`). Off (the default): the
    /// spec's instructions are byte-identical to before this switch
    /// existed (every bullet moved to `procedure.rs` stays in the spec
    /// text too, verbatim), and `situational` returns `None` -- no `## Now`
    /// section, nothing changes anywhere.
    pub situational_rules: bool,
    /// Unit 49 (cap the implement-review round trip): `review`-only. The
    /// number of times `review` may send work back to `implement`
    /// (non-empty `findings`) before the harness parks the step with
    /// `Action::AskUser` instead of handing off again. Counts activations of
    /// `review` (`CodingWorkspace::review_rounds`, harness-owned, written by
    /// `ReviewSkill::on_activate`): the first activation is round 1, so the
    /// default `3` allows three send-backs and parks on the fourth review's
    /// send-back. Evidence: build 32 gemma `rename_across_files` seed 2, 188
    /// steps, 148 activations -- `review` sent the same duplicated
    /// doc-comment finding back 75 times, and `implement` handed it straight
    /// back with an empty patch and no tool call every time, because
    /// re-activation resets `harness.tool_calls` to 0 and nothing counted
    /// the handoffs themselves.
    pub review_rounds_cap: u32,
}

impl Default for Guards {
    fn default() -> Self {
        Guards {
            redundant_reads: true,
            silent_reads: true,
            read_guard: true,
            redundant_verify: true,
            done_stays_done: false,
            commitment_read_cap: true,
            seen_map: true,
            decision_field: true,
            tests_field: true,
            grounded_findings: true,
            situational_rules: false,
            review_rounds_cap: 3,
        }
    }
}
