//! Switches for the non-progress guards audited in statie-spec.md "10.
//! After it works: the rule audit" (milestone 12). Every field defaults to
//! `true`, so a config with no `guards` section (or one missing individual
//! fields) behaves exactly as it did before this type existed.

use serde::{Deserialize, Serialize};

/// One switch per audited rule. All default `true`; flipping one to `false`
/// disables that rule and nothing else. See each field's doc comment for
/// what "off" means precisely, and `ARCHITECTURE.md` "Guards against
/// non-progress" for what each rule is guarding against.
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
    /// allowed.
    pub done_stays_done: bool,
    /// The commitment read cap (`COMMITMENT_READ_CAP` = 2): reads against a
    /// set commitment beyond the cap are rejected and clear it. Off: reads
    /// while a commitment is set neither count toward a cap nor clear the
    /// commitment; instead the harness appends a short note to the
    /// observation saying the commitment is still pending.
    pub commitment_read_cap: bool,
}

impl Default for Guards {
    fn default() -> Self {
        Guards {
            redundant_reads: true,
            silent_reads: true,
            read_guard: true,
            redundant_verify: true,
            done_stays_done: true,
            commitment_read_cap: true,
        }
    }
}
