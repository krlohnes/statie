//! The coding skill set: `router`, `plan`, `implement`, `review`, sharing a
//! single [`CodingWorkspace`] (milestone 6, statie-spec.md §5, §10).
//!
//! Activation flow: `router` decides where to go next (`plan`, `implement`,
//! or `review`); `plan` explores the repo and writes a step list, handing
//! off to `implement`; `implement` works the plan and hands off to
//! `review`; `review` either sends work back to `implement` (non-empty
//! `findings`) or marks the workspace `done`.

#[cfg(test)]
mod tests;

pub mod procedure;

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use statie_core::{
    Commitment, CommittedState, HarnessState, NO_GREEN_THINK_AT, ParsedOutput, Patch, PatchError,
    Report, Skill, SkillSpec, ValidatedStep, parse_read_header, validate_default,
};
use statie_patch::JsonMergePatch;

// `FileNote` moved to `statie-core` (milestone 9, shared with `Report`);
// re-exported here so existing `statie_skills::FileNote` imports keep
// working.
pub use statie_core::FileNote;

/// Status of one [`PlanStep`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Not started yet.
    #[default]
    Pending,
    /// Currently being worked.
    InProgress,
    /// Finished.
    Done,
    /// Deliberately not going to be done (explain why in the plan text or `recent_summary`).
    Skipped,
}

/// One step of the plan, in execution order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanStep {
    /// Stable id for this step, unique within `plan`. Do not reuse an id
    /// after removing a step.
    pub id: u32,
    /// A single concrete, checkable action (at most one sentence).
    pub text: String,
    /// Current status of this step. Defaults to `pending`: the merge-patch
    /// schema strips `required` inside the patch, so a schema-constrained
    /// model may send a new step as `{id, text}` alone.
    #[serde(default)]
    pub status: StepStatus,
}

/// Shared workspace for the coding skill set (router/plan/implement/review).
///
/// This is the *only* memory that survives across steps and across skill
/// handoffs: keep `recent_summary`, `files`, `plan`, and `findings` current
/// enough that the next step (possibly a different skill) can pick up
/// without re-deriving context. Full step-by-step history is not in the
/// prompt, but it is searchable with the `recall` tool.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodingWorkspace {
    /// The user's task, verbatim, as it appeared in the inbox. Set once
    /// (when empty) by whichever skill first sees it; never edited after.
    #[serde(default)]
    pub task: String,
    /// Ordered plan steps. Empty until `plan` has run. Merged by `id`: a
    /// patch entry with an existing `id` updates just that step (send only
    /// the fields you're changing, e.g. `{"id": 2, "status": "done"}`);
    /// entries you omit are kept unchanged, and a new `id` is appended.
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    /// Manifest of files touched or judged relevant so far, each with a
    /// one-line note. Merged by `path`: a patch entry with an existing
    /// `path` updates just that entry; entries you omit are kept, and a new
    /// `path` is appended.
    #[serde(default)]
    pub files: Vec<FileNote>,
    /// Concrete review findings. Empty means the work has been reviewed and
    /// approved as-is.
    #[serde(default)]
    pub findings: Vec<String>,
    /// One short paragraph maintained by the model describing what it did
    /// last. This is the only memory of recent activity across steps and
    /// across handoffs to another skill -- keep it current. Hard cap 1200
    /// characters; longer values are rejected.
    #[serde(default)]
    pub recent_summary: String,
    /// Kept by the harness from your previous reasoning. Read it; you
    /// cannot patch it.
    #[serde(default)]
    pub notes: String,
    /// Name of the skill to hand off to next: `router`, `plan`,
    /// `implement`, or `review`. Set this and use action `done` when this
    /// skill's part of the work is finished; leave it `null` (or equal to
    /// this skill's own name) to keep working in the current skill.
    #[serde(default)]
    pub next_skill: Option<String>,
    /// True once the whole task is complete and verified (only `review`
    /// sets this, and only with empty `findings`).
    #[serde(default)]
    pub done: bool,
    /// Reports from subagents you delegated to, keyed by child name;
    /// written by the harness, never by you.
    #[serde(default)]
    pub reports: BTreeMap<String, Report>,
    /// The mutating call you will make next. Set it before you read toward
    /// an edit; the harness holds you to it: while it is set you may read
    /// the named file at most twice, then you must make the call or change
    /// `next` and say why in `recent_summary`.
    #[serde(default)]
    pub next: Option<Commitment>,
    /// Kept by the harness: for each file, its length and the line ranges
    /// you have read and edited, with the step of the last change. Read it
    /// before re-reading a file; you cannot patch it.
    #[serde(default)]
    pub seen: Vec<SeenFile>,
    /// The reasoning subagent's most recent conclusion, from your last
    /// `think` call. Read it and act on it; you cannot patch it. Cleared by
    /// the harness when a test passes.
    #[serde(default)]
    pub decision: Option<Decision>,
    /// The harness's own record of the most recent `test` call, replaced
    /// whole on every one (BACKLOG.md item 5). Read it; you cannot patch
    /// it.
    #[serde(default)]
    pub tests: Option<TestRecord>,
}

/// One entry of `CodingWorkspace::seen` (milestone 10): what the harness
/// has recorded about a single file from `read_file`/`edit_file`/
/// `write_file`/`revert_file` calls, so the model doesn't have to
/// re-derive it (and doesn't have to re-read a file from the top to find
/// out what it already read) after working-set eviction, a handoff, or a
/// resume.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SeenFile {
    /// Path to the file, relative to the sandbox root.
    pub path: String,
    /// The file's length in lines, as of the last time the harness learned
    /// it (a `read_file` header, or a `write_file`/`revert_file` that
    /// reported a line count). `0` if never learned.
    #[serde(default)]
    pub lines: u32,
    /// Line ranges (1-based, inclusive, `[start, end]`) read so far,
    /// merged and sorted; adjacent/overlapping ranges are coalesced into
    /// one.
    #[serde(default)]
    pub read: Vec<[u32; 2]>,
    /// Line ranges (1-based, inclusive, `[start, end]`) edited so far,
    /// merged and sorted the same way as `read`.
    #[serde(default)]
    pub edited: Vec<[u32; 2]>,
    /// The harness step of the most recent `read_file` of this path, or
    /// `None` if it has never been read (or was last touched by a
    /// `write_file`/`revert_file`, both of which clear this back to
    /// `None` along with `read`). `edit_file` leaves this alone --
    /// comparing it against `step` (the most recent change of ANY kind)
    /// is how `write_file_confirmed_by_seen` tells whether the last
    /// change to this entry was a read (this equals `step`) or an edit
    /// (this is older than `step`, or `None`).
    #[serde(default)]
    pub read_step: Option<u64>,
    /// The harness step of the most recent change recorded for this entry.
    #[serde(default)]
    pub step: u64,
}

/// One entry of `CodingWorkspace::decision`: the reasoning subagent's most
/// recent conclusion from a `think` tool call, written by the harness (see
/// `common_after_tool`'s handling of the `think` tool) and cleared when a
/// `test` call passes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Decision {
    /// The question the model asked `think`.
    pub question: String,
    /// The subagent's conclusion (truncated to 1200 chars with a trailing
    /// " …[truncated]" marker if longer).
    pub answer: String,
    /// Harness step at which it was recorded.
    pub step: u64,
}

/// One entry of `CodingWorkspace::tests` (BACKLOG.md item 5): the harness's
/// own record of the most recent `test` tool call, written by
/// `common_after_tool` and replaced whole (no merging) on every successful
/// `test` call -- a passing run still writes one (`pass: true`, empty
/// `failing`, `first_error: None`) so the model can see the last green
/// command and step.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TestRecord {
    /// The command that ran, from the call's own `cmd` arg (`command` is
    /// also accepted, matching `tools/src/test_tool.rs`'s `TestArgs`
    /// alias). Empty when the call omitted it and let the tool
    /// auto-detect one -- `common_after_tool` reads only the call's own
    /// args, not the tool's `command: ...` output header.
    pub cmd: String,
    /// Whether the run passed (`statie_core::is_pass(output)`).
    pub pass: bool,
    /// Failing test names, in output order, deduped, at most 8.
    #[serde(default)]
    pub failing: Vec<String>,
    /// The first recognized error line, trimmed and capped at 200 chars
    /// (with a trailing `…` when cut).
    #[serde(default)]
    pub first_error: Option<String>,
    /// Harness step at which it was recorded.
    pub step: u64,
}

const KNOWN_SKILLS: [&str; 4] = ["router", "plan", "implement", "review"];

/// Rules shared by every coding skill: `recent_summary` length cap,
/// `next_skill` must name a known skill, `next_skill` must be one of this
/// skill's own `allowed_transitions()` (or its own name / unset), and plan
/// step ids must be unique. `plan` and `files` are merged per entry by
/// `id`/`path` (see [`CodingWorkspace`]'s field docs), so a partial array in
/// the patch no longer drops the entries it omits.
fn common_rules(
    skill_name: &str,
    allowed_transitions: &[&str],
    _cur: &CommittedState<CodingWorkspace>,
    next: &CodingWorkspace,
) -> Result<(), PatchError> {
    if next.recent_summary.len() > 1200 {
        return Err(PatchError::RuleViolation(format!(
            "recent_summary must be at most 1200 characters, got {}",
            next.recent_summary.len()
        )));
    }

    if let Some(target) = next.next_skill.as_deref() {
        if !KNOWN_SKILLS.contains(&target) {
            return Err(PatchError::RuleViolation(format!(
                "next_skill must be one of {KNOWN_SKILLS:?}, got {target:?}"
            )));
        }
        if target != skill_name && !allowed_transitions.contains(&target) {
            return Err(PatchError::RuleViolation(format!(
                "skill `{skill_name}` may not hand off to `{target}`; \
                 allowed transitions are {allowed_transitions:?}"
            )));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for step in &next.plan {
        if !seen.insert(step.id) {
            return Err(PatchError::RuleViolation(format!(
                "duplicate plan step id {}",
                step.id
            )));
        }
    }

    Ok(())
}

/// `handoff`/`is_terminal` shared by every coding skill.
fn common_handoff(name: &str, ws: &CodingWorkspace) -> Option<String> {
    ws.next_skill
        .as_deref()
        .filter(|n| *n != name)
        .map(String::from)
}

fn common_is_terminal(ws: &CodingWorkspace) -> bool {
    ws.done
}

/// Item 29: which coding skill has `tool` in its own `tools()`, for
/// `Skill::skill_with_tool`'s shared mapping across `RouterSkill`,
/// `PlanSkill`, `ImplementSkill`, and `ReviewSkill`. Checked in order --
/// `implement` first, since every tool it alone has (`edit_file`,
/// `write_file`, `revert_file`, `test`, `try_fixes`, `shell`, `probe`) is
/// the common case a rejected call in `plan`/`router`/`review` needs
/// pointed at -- then `review`, for any tool only `review` has (today,
/// none: every tool `review` carries is also in `plan`'s list, so this arm
/// never actually matches, but it stays as the general rule rather than a
/// hardcoded `implement`-or-nothing check). `None` for a name no skill has,
/// including the empty string (a rejected call with no `name` at all is
/// never "the tool that skill has", it's a missing name -- see
/// `core/src/session.rs`'s empty-name observation text, which is handled
/// as its own case rather than routed through this function).
pub fn coding_skill_with_tool(tool: &str) -> Option<&'static str> {
    if tool.is_empty() {
        return None;
    }
    const IMPLEMENT_ONLY: &[&str] = &[
        "edit_file",
        "write_file",
        "revert_file",
        "test",
        "try_fixes",
        "shell",
        "probe",
    ];
    if IMPLEMENT_ONLY.contains(&tool) {
        return Some("implement");
    }
    const REVIEW_ONLY: &[&str] = &[];
    if REVIEW_ONLY.contains(&tool) {
        return Some("review");
    }
    None
}

/// `on_activate` shared by every coding skill (`Skill::on_activate`,
/// `core/src/skill.rs`): a handoff is requested by setting `next_skill`
/// (paired with action `done`); the session consumes it by activating the
/// named skill next, but nothing else clears the field on the newly
/// committed workspace. Left in place, the stale value can be misread by
/// this activation's own bare-`done`/handoff logic (e.g. `implement_
/// bare_done`'s `next_skill.is_some()` check) as a handoff already named --
/// eleven bench-battery rejections traced to exactly this. Clears it
/// unconditionally whenever set: whatever `next_skill` said before this
/// activation started is meaningless now (either it named THIS skill, the
/// ordinary "no handoff yet" resting state, or it named the handoff that
/// was just consumed to get here) -- nothing else about the workspace is
/// touched.
fn common_on_activate(ws: &CodingWorkspace) -> Option<Patch> {
    if ws.next_skill.is_some() {
        Some(serde_json::json!({"next_skill": null}))
    } else {
        None
    }
}

/// `commitment` shared by `plan`/`implement`/`review`: the workspace's
/// `next` field, verbatim.
fn common_commitment(ws: &CodingWorkspace) -> Option<Commitment> {
    ws.next.clone()
}

/// `harness_owned_fields` shared by every coding skill: `notes` (maintained
/// by the harness's `Maintainer` call after each step, see statie-spec.md
/// "9. Context maintenance"), `reports` (written by `absorb_report` after
/// a `delegate` action), `seen` (milestone 10, written by `Skill::
/// after_tool` after a `read_file`/`edit_file`/`write_file`/`revert_file`
/// call), `decision` (written by `Skill::after_tool` from a `think`
/// call's output, cleared on a passing `test`; see `common_after_tool`),
/// and `tests` (BACKLOG.md item 5, written by `Skill::after_tool` on every
/// successful `test` call) are never model-patchable. Checked uniformly by
/// `validate_default` against every action.
fn common_harness_owned_fields() -> Vec<String> {
    vec![
        "notes".to_string(),
        "reports".to_string(),
        "seen".to_string(),
        "decision".to_string(),
        "tests".to_string(),
    ]
}

/// `notes` shared by every coding skill: the harness-maintained notes text,
/// or `None` when empty (nothing to render).
fn common_notes(ws: &CodingWorkspace) -> Option<String> {
    if ws.notes.is_empty() {
        None
    } else {
        Some(ws.notes.clone())
    }
}

/// `Skill::situational` shared by `plan`/`implement`/`review` (unit 26,
/// `guards.situational_rules`): `enabled = false` (the switch off) always
/// returns `None` -- no `## Now` section, identical to today's prompt.
/// `enabled = true` returns `procedure::render(procedure::situation(cur))`,
/// with an empty rendered string (a node with no outgoing edges, e.g.
/// `read`/`search`/`test_pass`) folded to `None` the same way
/// `PromptBuilder::situational` already treats an empty string -- so a
/// skill's own `None`-vs-`Some("")` distinction never has to be made twice.
fn common_situational(enabled: bool, cur: &CommittedState<CodingWorkspace>) -> Option<String> {
    if !enabled {
        return None;
    }
    let text = procedure::render(procedure::situation(cur));
    if text.is_empty() { None } else { Some(text) }
}

/// Tools `next.tool` may name: the mutating calls a commitment is allowed to
/// bind to.
const MUTATING_TOOLS: [&str; 5] = ["edit_file", "write_file", "revert_file", "shell", "test"];

// -----------------------------------------------------------------------
// `seen` (milestone 10): `Skill::after_tool` for `plan`/`implement`/
// `review`, shared via `common_after_tool`.
// -----------------------------------------------------------------------

/// At most this many `SeenFile` entries are kept; the entry with the
/// smallest `step` (least recently touched) is evicted first when a new
/// path would exceed the cap.
const SEEN_CAP: usize = 12;

/// Merges `[start, end]` (1-based, inclusive) into `ranges`, coalescing
/// with any range it touches or overlaps, and keeps `ranges` sorted by
/// start. Tolerates unsorted input (the whole vector is re-sorted before
/// merging) and disjoint ranges (appended as their own entry).
fn merge_range(ranges: &mut Vec<[u32; 2]>, new: [u32; 2]) {
    ranges.push(new);
    ranges.sort_unstable_by_key(|r| r[0]);
    let mut merged: Vec<[u32; 2]> = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        match merged.last_mut() {
            // Touching (adjacent, r[0] == last_end + 1) or overlapping
            // (r[0] <= last_end + 1 covers both, since r[0] is unsigned and
            // sorted by start).
            Some(last) if r[0] <= last[1].saturating_add(1) => {
                last[1] = last[1].max(r[1]);
            }
            _ => merged.push(r),
        }
    }
    *ranges = merged;
}

/// Finds the `SeenFile` entry for `path` in `seen`, or inserts a fresh one
/// (evicting the entry with the smallest `step` first if `seen` is already
/// at `SEEN_CAP` and `path` is not already present) and returns a mutable
/// reference to it.
fn seen_entry<'a>(seen: &'a mut Vec<SeenFile>, path: &str) -> &'a mut SeenFile {
    if let Some(idx) = seen.iter().position(|f| f.path == path) {
        return &mut seen[idx];
    }
    if seen.len() >= SEEN_CAP {
        let evict_idx = seen
            .iter()
            .enumerate()
            .min_by_key(|(_, f)| f.step)
            .map(|(i, _)| i)
            .expect("seen.len() >= SEEN_CAP > 0");
        seen.remove(evict_idx);
    }
    seen.push(SeenFile {
        path: path.to_string(),
        ..Default::default()
    });
    seen.last_mut().expect("just pushed")
}

/// BACKLOG.md item 15: whether `seen` already establishes that a
/// `write_file` on `path` is safe to allow past `tools/src/write_file.rs`'s
/// whole-file-rewrite line cap -- the harness has recorded a `read_file` of
/// the *whole* file (every line from 1 to the entry's `lines`, via one or
/// several coalesced `read` ranges) since the last time any part of it was
/// edited. That is: `read` (coalesced) covers 1..=lines, AND either
/// `edited` is empty (never edited this session) or `read_step ==
/// Some(entry.step)` -- the entry's most recent change of any kind was
/// that full read, so it came after whatever edit(s) preceded it. An
/// edit-then-full-reread of the same file is confirmed by the second half
/// of that OR: the read is the latest change, even though `edited` itself
/// is left non-empty (`edit_file` never clears it). A further edit after
/// the read moves `step`/`entry.step` past `read_step` again, so it goes
/// back to needing a fresh full read.
pub fn write_file_confirmed_by_seen(seen: &[SeenFile], path: &str) -> bool {
    let Some(entry) = seen.iter().find(|f| f.path == path) else {
        return false;
    };
    if !entry.edited.is_empty() && entry.read_step != Some(entry.step) {
        return false;
    }
    if entry.lines == 0 {
        // A `lines: 0` entry means the length was never learned (or the
        // file is empty) -- there is no target to check `read` coverage
        // against, so this can't be a confirmed full read.
        return false;
    }
    ranges_cover(&entry.read, entry.lines)
}

/// Whether the coalesced, sorted-by-construction `ranges` (see
/// `merge_range`, which every writer of `SeenFile::read`/`edited` goes
/// through) cover every line from 1 to `lines` inclusive with no gap.
/// Empty `ranges` never covers a positive `lines`.
fn ranges_cover(ranges: &[[u32; 2]], lines: u32) -> bool {
    let mut next_needed: u32 = 1;
    for r in ranges {
        if r[0] > next_needed {
            return false;
        }
        next_needed = next_needed.max(r[1].saturating_add(1));
        if next_needed > lines {
            return true;
        }
    }
    next_needed > lines
}

/// Parses `edit_file`'s success output header for the `(lines a-b)` range
/// of the region it just changed. Handles both header shapes
/// `tools/src/edit_file.rs` emits (see that module):
/// - `"edited <path>: replaced 1 occurrence (lines a-b)"` / `"... deleted N
///   line(s) (lines a-b)"` -- the range is read straight from the
///   parenthesized suffix.
/// - `"edited <path>: inserted N line(s) after|before line X"` -- no
///   explicit range in the text, so it's derived from `N` and `X`: an
///   `after` insert lands at `[X+1, X+N]`; a `before` insert lands at
///   `[X-N, X-1]` (saturating so an insert before line 1 never underflows).
///
/// Tolerant of a leading `note: ...; ` prefix (the whitespace-tolerant-
/// match and repeated-anchor notes edit_file sometimes prepends).
/// `write_file`/`revert_file` never emit this shape, so this parser is
/// specific to `edit_file`.
fn parse_edit_header(text: &str) -> Option<[u32; 2]> {
    let first_line = text.lines().next()?;
    let header = match first_line.split_once("edited ") {
        Some((_, rest)) => rest,
        None => return None,
    };

    if let Some(open) = header.rfind('(')
        && let Some(close_rel) = header[open..].find(')')
    {
        let inner = &header[open + 1..open + close_rel];
        if let Some(rest) = inner.strip_prefix("lines ")
            && let Some((a, b)) = rest.split_once('-')
        {
            let a: u32 = a.trim().parse().ok()?;
            let b: u32 = b.trim().parse().ok()?;
            if a <= b {
                return Some([a, b]);
            }
        }
    }

    // Insert shape: "...: inserted N line(s) after|before line X"
    let (_, rest) = header.split_once(": inserted ")?;
    let (n_str, rest) = rest.split_once(" line(s) ")?;
    let n: u32 = n_str.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let (dir, rest) = rest.split_once(" line ")?;
    let x: u32 = rest.split_whitespace().next()?.parse().ok()?;
    match dir {
        "after" => Some([x + 1, x + n]),
        "before" => Some([x.saturating_sub(n).max(1), x.saturating_sub(1).max(1)]),
        _ => None,
    }
}

/// Parses `revert_file`'s success output for the line count in `"reverted
/// <path> to its content from before this session's first edit (N
/// lines)"` (`tools/src/revert_file.rs`). Returns `None` for the "removed
/// <path>; it did not exist before this session" shape (nothing to record
/// a line count for -- the caller drops the entry instead).
fn parse_revert_lines(text: &str) -> Option<u32> {
    let first_line = text.lines().next()?;
    let open = first_line.rfind('(')?;
    let close_rel = first_line[open..].find(')')?;
    let inner = &first_line[open + 1..open + close_rel];
    let n_str = inner
        .strip_suffix(" lines")
        .or_else(|| inner.strip_suffix(" line"))?;
    n_str.trim().parse().ok()
}

/// Counts lines the way `str::lines()` would (one per `\n`, plus one more
/// for a final non-empty line with no trailing newline) -- mirrors
/// `tools/src/write_file.rs`'s/`revert_file.rs`'s `bytecount_lines`, used
/// here to derive `write_file`'s line count from its `content` arg (the
/// tool's own output only reports bytes written, not lines).
fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count();
    let n = if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    };
    n as u32
}

/// Longer than this, a `think` answer is truncated with a trailing
/// `" …[truncated]"` marker (see `Decision::answer`'s doc).
const DECISION_ANSWER_CAP: usize = 1200;

/// Truncation marker appended to an over-cap `think` answer.
const DECISION_TRUNCATED_MARKER: &str = " …[truncated]";

/// Truncates `answer` to `DECISION_ANSWER_CAP` characters (char-boundary
/// safe) with a trailing `DECISION_TRUNCATED_MARKER` if it's longer;
/// returned unchanged otherwise.
fn truncate_decision_answer(answer: &str) -> String {
    if answer.chars().count() <= DECISION_ANSWER_CAP {
        return answer.to_string();
    }
    let truncated: String = answer.chars().take(DECISION_ANSWER_CAP).collect();
    format!("{truncated}{DECISION_TRUNCATED_MARKER}")
}

/// Parses a `think` tool's success output (`"question: <question>\nanswer:\n
/// <answer text>"`, see this crate's `think` handling in `common_after_tool`)
/// into `(question, answer)`. Returns `None` for the two non-conclusion
/// shapes (a leading "`think` is not configured" or "think failed:" text)
/// or anything else that doesn't match the expected header.
fn parse_think_output(output: &str) -> Option<(String, String)> {
    let question = output.strip_prefix("question: ")?;
    let (question, rest) = question.split_once("\nanswer:\n")?;
    Some((question.to_string(), rest.to_string()))
}

/// Parses a `try_fixes` tool's output (`core/src/session.rs`'s
/// `execute_try_fixes`/`finish_try_fixes`) into a `decision`-shaped
/// `(question, answer)`: `question` is `"try_fixes: <cmd>"`, `<cmd>`
/// extracted from the first line's backtick-quoted command (present in
/// both the winner shape, "...candidate N passed `<cmd>` and was
/// applied", and the none-passed shape, "...none passed `<cmd>`");
/// `answer` is the output with the winner's diff block removed -- every
/// line between the "applied: <path> — <rationale>" line and the next
/// "losers:" line (or, with no losers, the end of the text), since the
/// diff itself can be up to 2000 chars and `decision` need only carry the
/// verdict, not the patch. Returns `None` when the first line carries no
/// backtick-quoted command (the "not configured"/"no failing test"/"no
/// candidates"/"try_fixes failed: ..." shapes, none of which name a
/// command in backticks) -- those leave `decision` untouched, the same as
/// `think`'s own not-configured/failed shapes leave it untouched via
/// `parse_think_output` returning `None`.
fn parse_try_fixes_output(output: &str) -> Option<(String, String)> {
    let first_line = output.lines().next()?;
    // Only the two real-verdict shapes start with "try_fixes: <N> candidate"
    // (winner: "try_fixes: N candidates; candidate M passed `<cmd>` and was
    // applied"; none-passed: "try_fixes: N candidates; none passed
    // `<cmd>`"). The "not configured"/"no failing test"/"no candidates"/
    // "try_fixes failed: ..." shapes all start with "try_fixes: " too but
    // never with a digit right after, and some carry unrelated backtick-
    // quoted text of their own (e.g. "`try_fixes` is not configured: set a
    // `fixer` section...") that a bare "first backtick pair" scan would
    // misread as a command -- so this checks the shape before ever looking
    // for a backtick.
    let rest = first_line.strip_prefix("try_fixes: ")?;
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }

    let start = first_line.find('`')?;
    let after_backtick = &first_line[start + 1..];
    let end = after_backtick.find('`')?;
    let cmd = &after_backtick[..end];
    if cmd.is_empty() {
        return None;
    }
    let question = format!("try_fixes: {cmd}");

    // Drop every line between "applied: ..." and the next "losers:" line
    // (or end of text) -- the diff block, present only in the winner shape.
    let mut answer_lines: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in output.lines() {
        if line.starts_with("applied: ") {
            answer_lines.push(line);
            skipping = true;
            continue;
        }
        if skipping {
            if line == "losers:" {
                skipping = false;
            } else {
                continue;
            }
        }
        answer_lines.push(line);
    }
    Some((question, answer_lines.join("\n")))
}

/// At most this many failing test names are kept by [`failing_test_names`]
/// (`CodingWorkspace::tests`'s `failing`, BACKLOG.md item 5).
const FAILING_NAMES_CAP: usize = 8;

/// Extracts failing test names from a `test` tool's output (`tools/src/
/// test_tool.rs`'s `format_result`, so cargo's own noise-stripping and
/// failure-summary extraction have already run), in first-appearance
/// order, deduped, capped at [`FAILING_NAMES_CAP`]. Recognizes:
/// - cargo: `test <name> ... FAILED` lines, name as printed (e.g.
///   `tests::adds`); the `failures:` section's plain `    <name>` list
///   lines name the same tests, so they're skipped rather than duplicated
///   (any line already collected, or that looks like the `failures:`
///   header itself, is not treated as a second name source).
/// - Python `unittest`: `FAIL: <name> (<module.Class>)` / `ERROR: <name>
///   (<module.Class>)` -> `<module.Class>.<name>`.
/// - `pytest`: `FAILED <path>::<name>` -> `<path>::<name>`.
///
/// An all-passing run (no recognized failure line) returns an empty vec.
fn failing_test_names(output: &str) -> Vec<String> {
    fn push(name: String, seen: &mut std::collections::HashSet<String>, names: &mut Vec<String>) {
        if names.len() >= FAILING_NAMES_CAP {
            return;
        }
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    let mut in_failures_list = false;
    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed == "failures:" {
            // Cargo's `failures:\n    name\n    name\n\ntest result: ...`
            // section repeats names already collected from `... FAILED`
            // lines above -- track it so the plain-name lines inside are
            // skipped, not treated as a second (duplicate) source.
            in_failures_list = true;
            continue;
        }
        if in_failures_list {
            if trimmed.is_empty() || trimmed.starts_with("test result:") {
                in_failures_list = false;
            }
            // Either way, this line is part of (or the boundary of) the
            // `failures:` list -- never a new source of names.
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("test ")
            && let Some(name) = rest.strip_suffix(" ... FAILED")
        {
            push(name.to_string(), &mut seen, &mut names);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("FAILED ") {
            // pytest: `FAILED <path>::<name>[ - <reason>]`
            let name = rest.split(" - ").next().unwrap_or(rest).trim();
            if !name.is_empty() {
                push(name.to_string(), &mut seen, &mut names);
            }
            continue;
        }

        for prefix in ["FAIL: ", "ERROR: "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                // unittest: `FAIL: <name> (<module.Class>)` / `ERROR:
                // <name> (<module.Class>)` -> `<module.Class>.<name>`.
                if let Some(open) = rest.rfind(" (")
                    && rest.ends_with(')')
                {
                    let name = rest[..open].trim();
                    let module_class = &rest[open + 2..rest.len() - 1];
                    if !name.is_empty() && !module_class.is_empty() {
                        push(format!("{module_class}.{name}"), &mut seen, &mut names);
                    }
                }
                break;
            }
        }
    }

    names
}

/// Longer than this, [`first_error_line`]'s result is truncated with a
/// trailing `…` (`CodingWorkspace::tests`'s `first_error`, BACKLOG.md item
/// 5).
const FIRST_ERROR_CAP: usize = 200;

/// Markers [`first_error_line`] recognizes, checked against a trimmed
/// line's start.
const FIRST_ERROR_MARKERS: [&str; 9] = [
    "error[",
    "error:",
    "thread '",
    "panicked at",
    "assertion",
    "AssertionError",
    "Error:",
    "FAILED",
    "FAIL:",
];

/// Finds the first line of `output` (after trimming) that starts with one
/// of [`FIRST_ERROR_MARKERS`] or `Traceback`, trims it, and caps it at
/// [`FIRST_ERROR_CAP`] chars (with a trailing `…` when cut). `None` if no
/// line matches (e.g. an all-passing run, or a shape with no recognized
/// error line at all).
fn first_error_line(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        let matches = trimmed.starts_with("Traceback")
            || FIRST_ERROR_MARKERS.iter().any(|m| trimmed.starts_with(m));
        if !matches {
            continue;
        }
        if trimmed.chars().count() <= FIRST_ERROR_CAP {
            return Some(trimmed.to_string());
        }
        let capped: String = trimmed.chars().take(FIRST_ERROR_CAP).collect();
        return Some(format!("{capped}…"));
    }
    None
}

/// `Skill::after_tool` shared by `plan`/`implement`/`review`: maintains
/// `CodingWorkspace::seen` from a successfully executed `read_file`,
/// `edit_file`, `write_file`, or `revert_file` call; `CodingWorkspace::
/// decision` from a `think` call (set), a `try_fixes` call (set, BACKLOG.md
/// item 4 -- see `parse_try_fixes_output`; a `try_fixes` whose losers
/// survive in `decision` are not retried the way a `think` conclusion
/// isn't) or a passing `test` call (cleared); and `CodingWorkspace::tests`
/// from every `test` call (replaced whole, BACKLOG.md item 5). Any other
/// tool (`search`, `list_files`, `shell`, `recall`, `filter_last_tool_call`)
/// returns `None` -- nothing to record.
///
/// `seen_enabled`/`decision_enabled`/`tests_enabled` are `guards.seen_map`/
/// `guards.decision_field`/`guards.tests_field` (`core/src/guards.rs`;
/// BACKLOG.md items 2 and 5), threaded in from each skill's own field of
/// the same name (see `PlanSkill::state_fields` and its siblings).
/// `seen_enabled` off: the `read_file`/`edit_file`/`write_file`/
/// `revert_file` branches below all return `None` instead of a `seen`
/// patch -- the field is never written and stays whatever it already was
/// (empty, for a session that never had the switch on). `decision_enabled`
/// off: the `think` branch returns `None` instead of setting `decision`
/// (the `think` tool still ran and its answer already reached the model as
/// that call's own observation -- only the persistent copy is skipped),
/// and the `test`-pass branch contributes no `decision: null` key instead
/// of clearing it (nothing to clear -- it was never set). `tests_enabled`
/// off: a `test` call writes no `tests` patch at all; the `decision`-
/// clearing half of a `test` call is independent of it and still applies
/// when `decision_enabled` is on.
// Three switches ride beside the five call facts; a struct for them would
// cost more than it saves at the fourteen call sites.
#[allow(clippy::too_many_arguments)]
fn common_after_tool(
    ws: &CodingWorkspace,
    step: u64,
    name: &str,
    args: &serde_json::Value,
    output: &str,
    seen_enabled: bool,
    decision_enabled: bool,
    tests_enabled: bool,
) -> Option<Patch> {
    if name == "think" {
        if !decision_enabled {
            return None;
        }
        let (question, answer) = parse_think_output(output)?;
        return Some(serde_json::json!({
            "decision": {
                "question": question,
                "answer": truncate_decision_answer(&answer),
                "step": step,
            }
        }));
    }

    if name == "try_fixes" {
        if !decision_enabled {
            return None;
        }
        let (question, answer) = parse_try_fixes_output(output)?;
        return Some(serde_json::json!({
            "decision": {
                "question": question,
                "answer": truncate_decision_answer(&answer),
                "step": step,
            }
        }));
    }

    if name == "test" {
        let mut patch = serde_json::Map::new();

        if decision_enabled && ws.decision.is_some() && statie_core::is_pass(output) {
            patch.insert("decision".to_string(), serde_json::Value::Null);
        }

        if tests_enabled {
            // Unit 23: when the call's own args carried no `cmd`/`command`
            // (auto-detected by `tools/src/test_tool.rs`), fall back to
            // parsing it back out of the tool's own `command: <cmd>` first
            // line (`statie_core::detected_test_command`, the same function
            // `core/src/session.rs` uses to populate `harness.last_test_cmd`)
            // so this record's `cmd` is never empty after a run that printed
            // a command.
            let cmd = args
                .get("cmd")
                .or_else(|| args.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| statie_core::detected_test_command(output))
                .unwrap_or_default();
            let pass = statie_core::is_pass(output);
            let (failing, first_error) = if pass {
                (Vec::new(), None)
            } else {
                (failing_test_names(output), first_error_line(output))
            };
            patch.insert(
                "tests".to_string(),
                serde_json::json!({
                    "cmd": cmd,
                    "pass": pass,
                    "failing": failing,
                    "first_error": first_error,
                    "step": step,
                }),
            );
        }

        if patch.is_empty() {
            return None;
        }
        return Some(serde_json::Value::Object(patch));
    }

    if !seen_enabled {
        return None;
    }

    let mut seen = ws.seen.clone();

    match name {
        "read_file" => {
            let header = parse_read_header(output)?;
            let path = args.get("path").and_then(serde_json::Value::as_str)?;
            let entry = seen_entry(&mut seen, path);
            entry.lines = header.total as u32;
            merge_range(&mut entry.read, [header.from as u32, header.to as u32]);
            entry.step = step;
            entry.read_step = Some(step);
        }
        "edit_file" => {
            let range = parse_edit_header(output)?;
            let path = args.get("path").and_then(serde_json::Value::as_str)?;
            let entry = seen_entry(&mut seen, path);
            merge_range(&mut entry.edited, range);
            entry.step = step;
        }
        "write_file" => {
            let path = args.get("path").and_then(serde_json::Value::as_str)?;
            // The tool's own output only reports bytes written ("wrote N
            // bytes to <path>", `tools/src/write_file.rs`), never a line
            // count, so N is derived from the call's own `content` arg
            // when present; `[[1, 0]]` ("whole file, length unknown") is
            // the fallback the spec calls for when it isn't.
            let content = args.get("content").and_then(serde_json::Value::as_str);
            let entry = seen_entry(&mut seen, path);
            match content {
                Some(c) => {
                    let lines = count_lines(c);
                    entry.lines = lines;
                    entry.edited = vec![[1, lines]];
                }
                None => {
                    entry.edited = vec![[1, 0]];
                }
            }
            entry.read.clear();
            entry.read_step = None;
            entry.step = step;
        }
        "revert_file" => {
            let path = args.get("path").and_then(serde_json::Value::as_str)?;
            if output.trim_start().starts_with("removed ") {
                // The file never existed before this session: drop the
                // entry outright, there is nothing left to describe.
                seen.retain(|f| f.path != path);
            } else {
                let lines = parse_revert_lines(output);
                let entry = seen_entry(&mut seen, path);
                if let Some(n) = lines {
                    entry.lines = n;
                }
                entry.edited.clear();
                entry.read.clear();
                entry.read_step = None;
                entry.step = step;
            }
        }
        _ => return None,
    }

    Some(serde_json::json!({ "seen": seen }))
}

/// Shared by `plan`/`implement`/`review`'s `validate` overrides, called
/// before `strip_malformed_next`/`validate_default` run: a model patch
/// touching a harness-owned key (`notes`, `reports`, `seen` -- see
/// `common_harness_owned_fields`) used to have the WHOLE step rejected by
/// `validate_default`'s owned-field check, discarding any real progress the
/// same patch carried (a plan step marked done, a file note, `recent_
/// summary`) and burning a retry, even though the model sees these fields
/// in its own workspace view and so predictably echoes or edits them back
/// (a bench battery run saw this twice in one run, losing a retry each
/// time). The project's rule since run 26 is commit-and-note, not
/// reject-the-lot (see `strip_malformed_next` and everything below it for
/// the same treatment applied to other shapes) -- so this strips each
/// harness-owned key present in the RAW patch object before
/// `validate_default`'s comparison ever runs, and returns one note per key
/// removed for the caller to attach the same way `strip_malformed_next`
/// does.
///
/// "Present in the raw patch" (key membership), not "differs from the
/// committed value": `validate_default`'s own check compares values so a
/// patch strategy that always carries every key (e.g. `ReplaceStrategy`)
/// isn't punished for merely echoing a harness-owned field back unchanged,
/// but this function runs on the merge-patch shape these three skills
/// actually use (`JsonMergePatch`), where a harness-owned key appears in
/// the raw object only when the model wrote it -- so presence here already
/// means "the model tried to patch this", no value comparison needed.
/// Removing the key outright (rather than overwriting it with the
/// committed value) leaves the merge to `validate_default` producing the
/// same committed value for that field either way, since an absent merge-
/// patch key means "leave unchanged".
///
/// Returns the (possibly empty) list of notes, one per removed key, each
/// reading exactly: "`<field>` is kept by the harness; that part of your
/// patch was dropped and the rest was kept." Order follows
/// `harness_owned_fields()`'s own order. Operator patches
/// (`validate_user_patch`) never call this -- the operator and the
/// maintainer are trusted writers of these fields, and an operator typing a
/// harness-owned field by hand is a mistake worth stopping, not silently
/// dropping.
fn strip_harness_owned(
    owned_fields: &[String],
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(obj) = out.patch_mut().0.as_object_mut() else {
        return notes;
    };
    for field in owned_fields {
        if obj.remove(field.as_str()).is_some() {
            notes.push(format!(
                "`{field}` is kept by the harness; that part of your patch was dropped and the \
                 rest was kept."
            ));
        }
    }
    notes
}

/// Shared by `plan`/`implement`/`review`'s `validate` overrides, called
/// before `validate_default` runs: a commitment is the mutating call the
/// model is bound to make next, not a look, so a post-patch `next` that is
/// malformed in any of the ways below never lands -- the `"next"` key is
/// removed from the raw patch (`out.patch_mut()`) so the merge leaves the
/// *committed* `next` exactly as it already was (an existing commitment
/// survives; if there was none, there still is none), and a one-line note is
/// returned for the caller to attach to `ValidatedStep` via
/// `with_notes`/`push_note` (the model reads it in its next observation --
/// see `core/src/session.rs`'s "10b. Validation notes"). Checked in order,
/// returning the first that applies:
///
/// 1. `next.tool` names a tool outside `MUTATING_TOOLS` (e.g. `search`).
///    This closes the original deadlock (statie-spec.md "10. After it
///    works: the rule audit") where the model set `next.tool = "read_file"`:
///    nothing rejected that at commit time, so the harness went on to cap
///    reads on the committed path at 2 while its own message told the model
///    to "make that call now" -- a call the model could never legally make,
///    since the committed tool was itself a read. A live run then showed the
///    opposite failure mode: the model legitimately wanted to name its next
///    *look* in `next` (nine rejections in one run for `next.tool =
///    "search"`), so the harness fix moved from rejecting the step to
///    silently dropping the offending `next` with a note instead (`next` is
///    a change, not a look -- reads need no `next` at all).
/// 2. `next.what` is empty/whitespace: a commitment with no one-line
///    description of the change is as useless to hold the model to as no
///    commitment at all.
/// 3. `next.tool` is `edit_file`/`write_file` and `next.path` is
///    missing/empty: those two tools touch one named file, so the
///    commitment cannot be resolved without it (`shell`/`test` need no
///    single path).
///
/// Cases 2 and 3 used to reject the whole step (rejection loop, not a
/// dropped slot). A live run parked as a failure when a local model sent
/// `next = {"tool":"test","path":"..."}` (no `what`) four times running,
/// rejected each time until the retry cap -- a rejection loop over a missing
/// one-liner is a worse outcome than a dropped slot, so these two now strip
/// and note as well, the same as case 1.
///
/// A patch that fails to deserialize/apply is left alone here (`None`): it
/// falls straight through to `validate_default`, which reports the real
/// error. A `next` that is well-formed, or that isn't touched by this patch
/// at all, is also left alone.
fn strip_malformed_next(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let merged = serde_json::from_value::<
        <JsonMergePatch<CodingWorkspace> as statie_core::PatchStrategy<CodingWorkspace>>::Patch,
    >(out.patch().0.clone())
    .ok()
    .and_then(|patch| {
        <JsonMergePatch<CodingWorkspace> as statie_core::PatchStrategy<CodingWorkspace>>::apply(
            cur, &patch,
        )
        .ok()
    })?;

    let commitment = merged.next.as_ref()?;

    let reason = if !MUTATING_TOOLS.contains(&commitment.tool.as_str()) {
        Reason::NonMutating(commitment.tool.clone())
    } else if commitment.what.trim().is_empty() {
        Reason::EmptyWhat
    } else if matches!(commitment.tool.as_str(), "edit_file" | "write_file")
        && commitment
            .path
            .as_deref()
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
    {
        Reason::MissingPath(commitment.tool.clone())
    } else {
        return None;
    };

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("next");
    }

    Some(match (&reason, &cur.next) {
        (Reason::NonMutating(tool), Some(open)) => format!(
            "`next` = {tool} ignored: a commitment is a change, not a look; your open \
             commitment is still `{} {}` ({}). Reads need no `next`.",
            open.tool,
            open.path.as_deref().unwrap_or("(no path)"),
            open.what
        ),
        (Reason::NonMutating(tool), None) => format!(
            "`next` = {tool} ignored: a commitment is a change, not a look. Send reads with no \
             `next`; set `next` only to the edit_file/write_file/shell/test call you will make."
        ),
        (Reason::EmptyWhat, Some(_)) => {
            "`next` ignored: `next.what` must say in one line what the call will do, e.g. \
             {\"tool\":\"edit_file\",\"path\":\"<file>\",\"what\":\"<one line>\"}. Your open \
             commitment is unchanged."
                .to_string()
        }
        (Reason::EmptyWhat, None) => {
            "`next` ignored: `next.what` must say in one line what the call will do, e.g. \
             {\"tool\":\"edit_file\",\"path\":\"<file>\",\"what\":\"<one line>\"}. Reads need \
             no `next`; set it when you know the change you will make."
                .to_string()
        }
        (Reason::MissingPath(tool), Some(_)) => format!(
            "`next` ignored: `next.path` is required when `next.tool` is `{tool}`. Your open \
             commitment is unchanged."
        ),
        (Reason::MissingPath(tool), None) => format!(
            "`next` ignored: `next.path` is required when `next.tool` is `{tool}`. Reads need \
             no `next`; set it when you know the change you will make."
        ),
    })
}

/// Which of `strip_malformed_next`'s three checks fired, carrying the tool
/// name where the note text needs it.
enum Reason {
    NonMutating(String),
    EmptyWhat,
    MissingPath(String),
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_repeated_skip_thinks`] fires (unit 21): a second refused
/// re-read (7.7/7.8's `skipped_redundant` guard, `core/src/session.rs`) of
/// the SAME call, with no `think` in between, gets a substituted `think`
/// call instead of letting core's own `skipped_redundant` observation repeat
/// a third time. `{path}` is filled from the call's `path` argument, or the
/// tool name when the call has none (e.g. a pathless `search`).
pub const REPEATED_SKIP_QUESTION: &str = "You asked to read {path} again, but its contents are \
     already in your working set above and nothing has changed. Either state the exact next \
     edit (path, old text, new text) that moves the plan forward, or name a different file or \
     range you actually need and why.";

/// `implement`-only (BACKLOG.md item 21): the second time in a row the model
/// asks to re-read the SAME thing core's `skipped_redundant` guard (7.7/7.8,
/// `Active::execute` in `core/src/session.rs`) already refused to re-run,
/// this substitutes a `think` call instead of letting the model repeat the
/// identical skip a third time (`CONTINUE_REPEAT_CAP`, `core/src/session.rs`,
/// would otherwise park the run on that third repeat). Mirrors
/// `implement_repeat_failure_thinks`'s once-per-streak shape, but keyed off
/// `HarnessState::last_skip` instead of `repeated_failure_step`.
///
/// Fires when all of these hold:
/// - `out.action()` is `Action::Tool { name, args }` with `name` one of
///   `read_file`/`search`/`list_files` -- a `shell` read has already been
///   rewritten into `read_file` by `shell_read_substitution`, which runs
///   before this in `ImplementSkill::validate`, so a `shell` action reaching
///   here is never a plain read and is left alone.
/// - `cur.harness.last_skip` (`core/src/version.rs`) is `Some(skip)` with
///   `skip.count >= 1`, `skip.tool == name`, and `skip.key` equal to this
///   call's own `read_call_key(name, args)` (`statie_core::read_call_key`,
///   made `pub` for exactly this) -- the SAME call the guard just skipped,
///   computed the same way core does so the two can never drift apart.
/// - No `think` has run since that skip: `cur.harness.last_think_step` is
///   `None`, or `Some(t)` with `t < skip.step`.
///
/// A first skip is left alone (core's own `skipped_redundant` observation is
/// enough the first time); a skip of a DIFFERENT call, or any skip once a
/// `think` has already run since it, is also left alone.
///
/// Effect: same substitution mechanism as `implement_repeat_failure_thinks`
/// (`substitute_think`), replacing the action with `Action::Tool { name:
/// "think", args: {"question": q} }` where `q` is `REPEATED_SKIP_QUESTION`
/// with `{path}` filled from `args.path` (or `name` when the call has no
/// `path` argument). The model's action was already a tool call, so there is
/// no `next_skill` to strip (`strip_next_skill_if_ask_user: false`).
fn implement_repeated_skip_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    const READ_TOOLS: &[&str] = &["read_file", "search", "list_files"];

    let statie_core::Action::Tool { name, args } = out.action() else {
        return None;
    };
    if !READ_TOOLS.contains(&name.as_str()) {
        return None;
    }

    let skip = cur.harness.last_skip.as_ref()?;
    if skip.count < 1 || skip.tool != *name {
        return None;
    }
    let key = statie_core::read_call_key(name, args);
    if skip.key != key {
        return None;
    }
    if let Some(think_step) = cur.harness.last_think_step
        && think_step >= skip.step
    {
        return None;
    }

    let path = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(name.as_str());
    let question = REPEATED_SKIP_QUESTION.replace("{path}", path);
    let note = format!(
        "you asked to re-read {path}, which the harness already refused to re-run (nothing has \
         changed since your last read), so it asked the reasoning subagent instead (its \
         conclusion is below, and in `decision`). Act on it."
    );

    Some(substitute_think(out, &question, note, false))
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_repeated_error_thinks`] fires (unit 23 part (b)): the SAME
/// tool call fails a second time in a row with the identical reason core's
/// own repeated-tool-error guard (`HarnessState::last_tool_error`,
/// `core/src/session.rs`) already tracks. `{tool}` is filled from that
/// error's own tool name, `{reason}` from its reason string, cut to 300
/// characters (`cap_reason`, mirroring the redundant-verification guard's
/// own cap on a long tool-error message).
pub const REPEATED_ERROR_QUESTION: &str = "Your last `{tool}` call failed with: {reason}. \
     Repeating the same call will fail the same way. Using the working set above, state a \
     different exact next action: for an edit, the path, the old text as it appears in the \
     file, and the new text; otherwise the file or range to read and why.";

/// `implement`-only (unit 23 part (b), reshaped by unit 25, corrected after
/// review): substitutes a `think` call in place of a tool call that is
/// either the EXACT SAME failing call repeating, or the same KIND of call
/// recurring after it has already failed with the same error shape twice.
/// A corrected call -- same tool, different args, the FIRST occurrence of
/// its error shape -- is left alone to run: the harness told the model to
/// re-read and fix the call, so the corrected retry must actually execute,
/// not be diverted to `think`. Mirrors [`implement_repeated_skip_thinks`]'s
/// once-per-streak shape and wiring (run right after it in
/// `ImplementSkill::validate`), but keyed off `HarnessState::last_tool_error`
/// instead of `HarnessState::last_skip`.
///
/// Fires when all of these hold:
/// - `out.action()` is `Action::Tool { name, args }`.
/// - `cur.harness.last_tool_error` (`core/src/version.rs`) is `Some(err)`
///   with `err.tool == name` -- a call to a DIFFERENT tool is left alone (it
///   cannot be the same kind of call recurring).
/// - EITHER `err.args == args` (the identical call repeating, item 23's
///   original rule) OR `err.count >= 2` (this error SHAPE has already
///   repeated once -- the model has had a chance to correct and has not, so
///   even a differently-shaped retry of the same kind of call is diverted).
///   A call with different args whose shape has failed only ONCE so far
///   (`err.count < 2`) is left alone to run: that is the corrected retry the
///   `REPEATED_ERROR_QUESTION`/prior error message asked for.
/// - No `think` has run since that error: `cur.harness.last_think_step` is
///   `None`, or `Some(t)` with `t < err.step` -- the same `last_think_step`
///   rule [`implement_repeated_skip_thinks`] uses.
///
/// A first error is left alone (core's own repeated-tool-error guard already
/// prefixes the observation the second time -- see that guard's doc,
/// `core/src/session.rs`); a call to a DIFFERENT tool after an error, a
/// corrected call (different args, shape failed only once), or the SAME
/// tool once a `think` has already run since it, is also left alone.
///
/// Effect: same substitution mechanism as [`implement_repeated_skip_thinks`]
/// (`substitute_think`), replacing the action with `Action::Tool { name:
/// "think", args: {"question": q} }` where `q` is [`REPEATED_ERROR_QUESTION`]
/// with `{tool}`/`{reason}` filled from `err.tool`/`err.reason` -- the FULL
/// text of the latest error (cut to 300 characters via `cap_reason`), not
/// its shape, so the model still sees the exact failing message. The
/// model's action was already a tool call, so there is no `next_skill` to
/// strip (`strip_next_skill_if_ask_user: false`), the same as the skip
/// trigger.
/// Cuts `text` to at most 300 characters (unit 23 part (b)'s brief: "reason
/// cut to 300 characters"), with no truncation marker appended -- unlike
/// [`truncate_decision_answer`], which is capping a `think` ANSWER for
/// storage in `decision`, this is capping a tool-error REASON string being
/// substituted into a question sent to the reasoning subagent, so the exact
/// cap point doesn't need to be visible to the model.
fn cap_reason_to_300_chars(text: &str) -> String {
    const REASON_CAP: usize = 300;
    if text.chars().count() <= REASON_CAP {
        text.to_string()
    } else {
        text.chars().take(REASON_CAP).collect()
    }
}

fn implement_repeated_error_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let statie_core::Action::Tool { name, args } = out.action() else {
        return None;
    };

    let err = cur.harness.last_tool_error.as_ref()?;
    if err.tool != *name {
        return None;
    }
    // The identical call recurring (item 23) always fires; otherwise the
    // same error SHAPE must have already repeated once (`count >= 2`) --
    // a corrected call (different args, first occurrence of this shape)
    // is left alone to run so the model's fix actually takes effect.
    if *args != err.args && err.count < 2 {
        return None;
    }
    if let Some(think_step) = cur.harness.last_think_step
        && think_step >= err.step
    {
        return None;
    }

    let reason = cap_reason_to_300_chars(&err.reason);
    let question = REPEATED_ERROR_QUESTION
        .replace("{tool}", &err.tool)
        .replace("{reason}", &reason);
    let note = format!(
        "your last `{}` call failed with: {reason}. Repeating it will fail the same way, so the \
         harness asked the reasoning subagent instead (its conclusion is below, and in \
         `decision`). Act on it.",
        err.tool
    );

    Some(substitute_think(out, &question, note, false))
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_stall_thinks`] fires (unit 25 part 3): the model has been
/// going back and forth on ONE path, alternating a refused re-read and a
/// failed edit (`HarnessState::last_stall`, `core/src/version.rs`), for two
/// or more consecutive stall events. `{path}` is filled from the stall's own
/// `path`.
pub const STALLED_PATH_QUESTION: &str = "You have been going back and forth on {path}: reading \
     it again and editing it have both failed. Using the working set above, state the one exact \
     edit (old text as it appears, new text) that moves the plan forward, or say which other \
     file the change belongs in.";

/// `implement`-only (unit 25 part 3): the second (and every later) stall
/// event on the SAME path -- `HarnessState::last_stall.count >= 2`, core's
/// own stalled-path guard (`core/src/session.rs`) having already recorded
/// that a refused re-read and a failed edit have landed on the same path
/// with no executed step on it in between -- substitutes a `think` call
/// instead of letting the model keep alternating skip/error on that path.
/// Mirrors [`implement_repeated_skip_thinks`]/[`implement_repeated_error_
/// thinks`]'s once-per-streak shape and wiring (run right after the error
/// trigger in `ImplementSkill::validate`), but keyed off `HarnessState::
/// last_stall` instead of `last_skip`/`last_tool_error`.
///
/// Fires when all of these hold:
/// - `cur.harness.last_stall` (`core/src/version.rs`) is `Some(stall)` with
///   `stall.count >= 2`.
/// - `out.action()` is STILL `Action::Tool { name, args }` for a tool in
///   `EDIT_TOOL_NAMES`/`READ_TOOL_NAMES` (this module's own copies of
///   `core/src/session.rs`'s `EDIT_TOOLS`/read-tool set) naming `stall.path`
///   in `args.path` -- i.e. nothing upstream in this same `validate` call
///   (`implement_repeated_skip_thinks`/`implement_repeated_error_thinks`,
///   which run right before this) has ALREADY substituted this step's
///   action for `think`. Both of those triggers can satisfy their own
///   condition on the very same step a stall reaches `count >= 2` (a second
///   identical-shape edit error on a path is simultaneously the SECOND
///   stall event on that path), and having already run first in `validate`'s
///   order, they leave `out.action()` as `Action::Tool { name: "think", .. }`
///   -- which names neither tool set, so this check alone is enough to skip
///   a redundant second substitution on the same step without needing to
///   know which (if either) of the other two triggers fired.
/// - No `think` has run since that stall event: `cur.harness.last_think_step`
///   is `None`, or `Some(t)` with `t < stall.step` -- the same
///   `last_think_step` rule the other two triggers use (D19: "a `think`
///   step keeps the stall count", so this gate suppresses a SECOND
///   substitution for the same stall event, not the streak as a whole).
///
/// Effect: same substitution mechanism as the other two triggers
/// (`substitute_think`), replacing the action with `Action::Tool { name:
/// "think", args: {"question": q} }` where `q` is [`STALLED_PATH_QUESTION`]
/// with `{path}` filled from `stall.path`. The model's action was already a
/// tool call, so there is no `next_skill` to strip
/// (`strip_next_skill_if_ask_user: false`), the same as the other two
/// triggers.
fn implement_stall_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    const STALL_TOOLS: &[&str] = &[
        "read_file",
        "search",
        "list_files",
        "edit_file",
        "write_file",
        "revert_file",
    ];

    let stall = cur.harness.last_stall.as_ref()?;
    if stall.count < 2 {
        return None;
    }
    let statie_core::Action::Tool { name, args } = out.action() else {
        return None;
    };
    if !STALL_TOOLS.contains(&name.as_str()) {
        return None;
    }
    if args.get("path").and_then(serde_json::Value::as_str) != Some(stall.path.as_str()) {
        return None;
    }
    if let Some(think_step) = cur.harness.last_think_step
        && think_step >= stall.step
    {
        return None;
    }

    let path = &stall.path;
    let question = STALLED_PATH_QUESTION.replace("{path}", path);
    let note = format!(
        "you have been going back and forth on {path} (a re-read refused, an edit failed, or \
         both), so the harness asked the reasoning subagent instead (its conclusion is below, \
         and in `decision`). Act on it."
    );

    Some(substitute_think(out, &question, note, false))
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_no_green_thinks`] fires (unit 27, "no green cap"): the run
/// has failed `test` `{n}` times in a row since its last pass without ever
/// getting back to green, so the harness asks whether the PLAN itself --
/// not just the next edit -- is wrong. `{n}` is filled from
/// `HarnessState::fails_since_green` (`core/src/version.rs`).
pub const NO_GREEN_QUESTION: &str = "{n} test runs have failed since the last pass. Is the plan \
     itself wrong? Name the one smallest change that could make the failing test pass, or say \
     which plan step should be dropped.";

/// `implement`-only (unit 27, "no green cap"): once
/// `HarnessState::fails_since_green` (`core/src/version.rs`) reaches
/// `NO_GREEN_THINK_AT`, with no `think` since the last failing `test`'s
/// step, substitutes a `think` call asking whether the plan itself is
/// wrong, instead of letting the model keep editing blind. Run right after
/// [`implement_stall_thinks`] in `ImplementSkill::validate`: unlike the
/// other `think` triggers (each keyed on a specific streak shape --
/// identical skip, identical-shape tool error, a stalled path), this one is
/// keyed purely on the raw count of failing `test` runs since the last
/// pass, so it can fire on ANY action shape, including one none of the
/// other triggers would recognize (e.g. a fresh, non-repeating edit that
/// still hasn't gotten the suite green in 12 tries) -- but is explicitly
/// left out when the action is itself `test` or `think`, so the model can
/// still verify its own fix or think on its own without the harness
/// stepping on it.
///
/// Fires when all of these hold:
/// - `cur.harness.fails_since_green >= NO_GREEN_THINK_AT`.
/// - `cur.agent.tests` (`CodingWorkspace::tests`, the harness's own record
///   of the most recent `test` call, `common_after_tool`) is `Some(rec)`
///   with `rec.pass == false` -- the last recorded `test` run is the
///   failure this count is tracking, and its own `step` anchors the
///   `last_think_step` comparison below (mirroring `implement_repeated_
///   error_thinks`'s use of `err.step`/`implement_stall_thinks`'s use of
///   `stall.step`).
/// - No `think` has run since that failing test's step:
///   `cur.harness.last_think_step` is `None`, or `Some(t)` with `t <
///   rec.step`.
/// - `out.action()` is NOT `Action::Tool { name: "test" | "think", .. }` --
///   the model verifying its own fix, or thinking on its own, is left
///   alone.
///
/// Effect: same substitution mechanism as the other triggers
/// (`substitute_think`), replacing the action with `Action::Tool { name:
/// "think", args: {"question": q} }` where `q` is [`NO_GREEN_QUESTION`]
/// with `{n}` filled from `fails_since_green`. The model's action was
/// already set (a tool call, `done`, or `ask_user`), so there is no
/// `next_skill` to strip (`strip_next_skill_if_ask_user: false`), the same
/// as the other `think` triggers wired alongside it.
fn implement_no_green_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if cur.harness.fails_since_green < NO_GREEN_THINK_AT {
        return None;
    }
    let rec = cur.agent.tests.as_ref()?;
    if rec.pass {
        return None;
    }
    if let Some(think_step) = cur.harness.last_think_step
        && think_step >= rec.step
    {
        return None;
    }
    let is_test_or_think = matches!(
        out.action(),
        statie_core::Action::Tool { name, .. } if matches!(name.as_str(), "test" | "think")
    );
    if is_test_or_think {
        return None;
    }

    let n = cur.harness.fails_since_green;
    let question = NO_GREEN_QUESTION.replace("{n}", &n.to_string());
    let note = format!(
        "{n} test runs have failed since the last pass, so the harness asked the reasoning \
         subagent whether the plan itself is wrong (its conclusion is below, and in \
         `decision`). Act on it."
    );

    Some(substitute_think(out, &question, note, false))
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_repeat_failure_thinks`] fires for an [`FailureKind::Assertion`]
/// failure -- unchanged from before [`classify_failure`] existed, kept under
/// this name since `skills/tests/think_trigger.rs` and
/// `skills/src/coding/tests.rs` reference it directly.
pub const REPEAT_FAILURE_QUESTION: &str = "The test failed the same way after my last edit. \
     Trace the failing test's input through the code with exact arithmetic, checking every \
     intermediate value against what the test expects: which function is actually wrong, and \
     what is the exact change to make? If the values are not obvious, use `probe` to print them \
     instead of computing them.";

/// DECISIONS.md D9 / BACKLOG.md item 18: which shape a test failure takes,
/// used to pick the [`repeat_failure_question`] asked when
/// [`implement_repeat_failure_thinks`] fires, instead of asking every repeat
/// the same arithmetic-trace question written for a rounding-trap assertion.
/// A calibration run showed that question asked about a Rust compile error:
/// four `think` calls at 263 seconds each, three of them empty after
/// exhausting a 32k window, tracing arithmetic that did not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    CompileError,
    Assertion,
    Panic,
    Timeout,
    Other,
}

/// Classifies a normalized failure summary (`core/src/session.rs`'s
/// `normalize_failure_summary`, as stored in
/// `HarnessState::last_failure_summary`) into a [`FailureKind`], so
/// [`repeat_failure_question`] can pick a question that fits the failure.
///
/// Order matters: a compile error wins over everything else (a build that
/// doesn't compile makes any assertion/panic text in the same summary
/// irrelevant), then a timeout, then an assertion, then a bare panic/
/// exception; anything matching none of those is `Other`.
pub fn classify_failure(summary: &str) -> FailureKind {
    let is_rustc_compile_error = summary.contains("error[E")
        || (summary
            .lines()
            .any(|l| l.trim_start().starts_with("error:"))
            && (summary.contains("-->") || summary.contains("could not compile")));
    let is_python_compile_error = [
        "SyntaxError",
        "NameError",
        "ImportError",
        "ModuleNotFoundError",
        "IndentationError",
    ]
    .iter()
    .any(|needle| summary.contains(needle));
    if is_rustc_compile_error || is_python_compile_error {
        return FailureKind::CompileError;
    }

    if summary.contains("timed out") || summary.contains("timeout") {
        return FailureKind::Timeout;
    }

    // A digit must appear SOMEWHERE in the summary for any of these to
    // count as an assertion -- a bare "assertion failed: a == b" panic
    // message with no values in it is a panic, not a traceable assertion
    // (see `classify_failure_treats_a_digit_free_assertion_panic_as_panic_
    // not_assertion` in `tests.rs`).
    let has_digit = summary.chars().any(|c| c.is_ascii_digit());
    let is_assertion = has_digit
        && (summary.contains("assertion")
            || summary.contains("AssertionError")
            || summary.contains("assert_eq")
            || summary.contains("left:")
            || summary.contains("right:")
            || summary.contains("expected"));
    if is_assertion {
        return FailureKind::Assertion;
    }

    if summary.contains("panicked at")
        || summary.contains("Traceback")
        || summary.contains("Exception")
    {
        return FailureKind::Panic;
    }

    FailureKind::Other
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_repeat_failure_thinks`] fires, shaped to `kind` (DECISIONS.md
/// D9, BACKLOG.md item 18): each caps the expected answer shape so the
/// reasoning subagent doesn't spend its window tracing values a compile
/// error, panic, or timeout gives it no reason to trace.
pub fn repeat_failure_question(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::CompileError => {
            "The build failed the same way after my last edit. Read the first error only: name \
             the file and line it points at, say in one sentence what the compiler is objecting \
             to, and give the one edit (old text and new text) that fixes it. Do not trace \
             values or reason about the tests."
        }
        FailureKind::Assertion => REPEAT_FAILURE_QUESTION,
        FailureKind::Panic => {
            "The test failed the same way after my last edit. Name the line that panics or \
             raises, the input that reaches it, and the one edit that stops it. Do not trace \
             arithmetic unless the message contains numbers."
        }
        FailureKind::Timeout => {
            "The test timed out the same way after my last edit. Name the loop or wait that \
             does not finish and the condition that never becomes true, then the one edit that \
             fixes it."
        }
        FailureKind::Other => {
            "The test failed the same way after my last edit. Read the failure summary once, \
             name the single most likely cause in one sentence, and give the one edit to make \
             next."
        }
    }
}

/// `implement`-only: a bench trap fixture showed the acting model unanimous
/// and wrong -- every edit it tried addressed the wrong function, the test
/// failed identically after each one, and the model never called `think` on
/// its own to break out of the loop. Telling it to "think harder" via a note
/// does not work (the repeated-failure guard's own note in `Session::
/// note_test_result`, `core/src/session.rs`, already says as much); running
/// the reasoning subagent for it, the same way `implement_no_green_runs_test`
/// runs `test` for it, does.
///
/// A first live battery run against the trap fixture showed a second stuck
/// shape the original (edit-only) trigger never caught: the model re-ran the
/// failing test five times running with nothing edited in between, instead
/// of editing again -- so the trigger, keyed on the next `edit_file`/
/// `write_file`, never fired at all. A third shape observed in the same
/// battery: with a repeat open, the model sent `ask_user` with no `?` (a
/// statement of intent, not a question) instead of either editing or
/// thinking -- `ask_user_empty_note`/`ask_user_statement_note` would
/// otherwise just convert that into a `Continue` note and leave the loop
/// unbroken. This function now fires on all three shapes, with the same
/// `repeated_failure_step`/`last_think_step` gating and the same `think`
/// substitution, differing only in which action it fires on and the note's
/// wording (each explains what was replaced and why).
///
/// Called in `ImplementSkill::validate` right after `strip_malformed_next`
/// and before the `done`-handling chain (`green_done_note`/
/// `implement_bare_done`/`implement_no_green_runs_test`/
/// `premature_done_note`, the latter of which falls through to
/// `ask_user_empty_note`/`ask_user_statement_note` for a non-`Done` action):
/// those all key off action `Done` or `AskUser`, so running this function
/// first means its `think` substitution is always seen before the
/// done/ask_user conversions -- in particular, an `ask_user` this function
/// replaces is a `Tool { name: "think", .. }` by the time
/// `premature_done_note` runs, so the empty-ask/statement-ask conversions
/// never see the original `ask_user` and cannot re-convert it.
///
/// Fires when both of these hold:
/// - `cur.harness.repeated_failure_step` (`core/src/version.rs`) is
///   `Some(t)`: the most recent `test` call failed identically to the one
///   before it.
/// - Either `cur.harness.last_think_step` is `None` (no `think` call has run
///   at all since that repeat was detected), or it is `Some(s)` with `s <
///   t` AND `cur.agent.seen` (`CodingWorkspace::seen`) has some entry whose
///   `edited` is non-empty with `step > s` -- i.e. the model has actually
///   edited something since that last `think` call. `last_think_step` is
///   set unconditionally by every `think` call, configured or not
///   (`core/src/session.rs`'s `execute_think`), so without the edit check
///   this fired again on every later stuck action in the same streak: a
///   second live battery showed the model re-run the same failing test five
///   times running with nothing edited in between, and each repeat re-armed
///   the trigger (`last_think_step` stayed behind `repeated_failure_step`
///   forever) even though the reasoning subagent had already answered once
///   and nothing new had happened for it to say anything different about --
///   all five calls came back empty (see `ModelThinker::think`'s
///   `done_reason: "length"` handling). Requiring an edit since the last
///   `think` means a bare repeat of the same stuck action gets left alone
///   once one `think` call has already run for it.
///
/// ...and `out.action()` is one of:
/// - `Action::Tool { name, .. }` with `name` one of `edit_file`/
///   `write_file` -- the model is about to make another blind edit.
/// - `Action::Tool { name: "test", .. }` with `cur.harness.edits_since_test
///   == 0` -- the model is re-running the same failing test with nothing
///   edited since (seen in the live battery: five identical `test` calls
///   running, no edit between any of them). `edits_since_test > 0` (a real
///   edit happened since the last `test`) is left alone; that is
///   `implement_no_green_runs_test`'s and the model's own territory, not a
///   stuck shape.
/// - `Action::AskUser { message }` where `message` contains no `?` -- the
///   same shape `ask_user_empty_note`/`ask_user_statement_note` convert to a
///   `Continue` note, except a repeated failure is open, so a reasoning
///   call is more useful than a note the model has already shown it does
///   not act on.
///
/// A `think` call, a read, a `test` with edits since the last one, a real
/// question to the user, or `done` are all left alone; `plan`/`review` never
/// call this function at all.
///
/// Effect: the action is replaced with `Action::Tool { name: "try_fixes",
/// args: {} }` when `cur.harness.try_fixes_available` (a `Fixer` is
/// attached, BACKLOG.md item 4), else `Action::Tool { name: "think", args:
/// {"question": q} }` where `q` is `repeat_failure_question(classify_failure(
/// cur.harness.last_failure_summary))` (DECISIONS.md D9, BACKLOG.md item 18)
/// -- `REPEAT_FAILURE_QUESTION` only when that failure classifies as
/// `FailureKind::Assertion`; a compile error, panic, or timeout gets a
/// question shaped to it instead. For the edit and `test` shapes, the raw
/// patch is left exactly as the model sent it
/// (the model's action was already a tool call, so there is no `done`/
/// `next_skill` to strip); for the `ask_user` shape, `next_skill` is
/// stripped from the raw patch first, the same as `ask_user_empty_note`/
/// `ask_user_statement_note` do, since a stray `next_skill` left over from
/// an `ask_user` step would make `core/src/session.rs`'s step 7.5 reject
/// the now-non-`Done` step. The session runs this call like any other
/// (`core/src/session.rs`'s `Active::execute` intercepts `name == "think"`/
/// `name == "try_fixes"` before dispatch): a `think` call writes
/// `CodingWorkspace::decision` (`common_after_tool`'s `think` handling)
/// from its answer, and a `try_fixes` call either applies a verified fix
/// outright (bumping `edits_since_test`) or falls back to its own `think`
/// call when none pass (writing `decision` the same way) -- so the model's
/// NEXT step sees the subagent's conclusion in both the tool observation
/// and (when it came from `think`, directly or as `try_fixes`'s fallback)
/// `decision`, and can act on it instead of repeating the same wrong edit,
/// the same idle test rerun, or the same unanswerable non-question a third
/// time.
///
/// A patch that fails to deserialize/apply is not relevant here (this
/// function never inspects the merged workspace) -- always returns a note
/// when the conditions above hold.
fn implement_repeat_failure_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    enum Shape {
        Edit,
        IdleTest,
        NonQuestionAsk,
    }

    let shape = match out.action() {
        statie_core::Action::Tool { name, .. } if name == "edit_file" || name == "write_file" => {
            Shape::Edit
        }
        statie_core::Action::Tool { name, .. }
            if name == "test" && cur.harness.edits_since_test == 0 =>
        {
            Shape::IdleTest
        }
        statie_core::Action::AskUser { message } if !message.contains('?') => Shape::NonQuestionAsk,
        _ => return None,
    };

    let repeat_step = cur.harness.repeated_failure_step?;
    if let Some(think_step) = cur.harness.last_think_step {
        if think_step >= repeat_step {
            return None;
        }
        // A `think` call has already run since the repeat was detected (but
        // before it), so only re-arm once the model has actually edited
        // something since that call -- otherwise this is the same stuck
        // action the subagent already answered once, and calling it again
        // would just ask the same question (see the doc comment above).
        let edited_since = cur
            .agent
            .seen
            .iter()
            .any(|f| f.step > think_step && !f.edited.is_empty());
        if !edited_since {
            return None;
        }
    }

    let note = match shape {
        Shape::Edit => "your last edit did not change the test failure, so the harness asked the \
             reasoning subagent first (its conclusion is below, and in `decision`). Act on \
             it: if it names a different cause, fix that; if it agrees with you, make your \
             edit."
            .to_string(),
        Shape::IdleTest => {
            "you re-ran the test with nothing edited, so the harness asked the reasoning \
             subagent instead (its conclusion is below, and in `decision`). Act on it."
                .to_string()
        }
        Shape::NonQuestionAsk => {
            "you had nothing to ask and a repeated test failure is open, so the harness asked \
             the reasoning subagent (its conclusion is below, and in `decision`). Act on it."
                .to_string()
        }
    };

    // BACKLOG.md item 4: with a `Fixer` attached (`Session::with_fixer`,
    // `HarnessState::try_fixes_available`), the trigger samples and
    // verifies candidate fixes instead of a single reasoning trace: it
    // applies a verified fix outright and falls back to `think` itself
    // when none pass (`core::Session::execute_try_fixes`). Without a fixer
    // the substitution is `think`, as before.
    if cur.harness.try_fixes_available {
        if matches!(shape, Shape::NonQuestionAsk)
            && let Some(obj) = out.patch_mut().0.as_object_mut()
        {
            obj.remove("next_skill");
        }
        out.set_action(statie_core::Action::Tool {
            name: "try_fixes".to_string(),
            args: serde_json::json!({}),
        });
        return Some(note);
    }

    let question = repeat_failure_question(classify_failure(
        cur.harness.last_failure_summary.as_deref().unwrap_or(""),
    ));
    Some(substitute_think(
        out,
        question,
        note,
        matches!(shape, Shape::NonQuestionAsk),
    ))
}

/// The question the harness asks `think` on the model's behalf when
/// [`implement_early_park_thinks`] fires.
pub const EARLY_PARK_QUESTION: &str = "You sent ask_user twice with nothing to ask. Given the \
     task, the plan, and the last tool output, what is the next concrete tool call and why? Name \
     the tool, its arguments, and what you expect to see.";

/// `implement`-only, `ImplementSkill::validate`'s sibling of
/// `implement_repeat_failure_thinks`: DECISIONS.md D4's park-rate study over
/// 96 bench runs found 6 of 9 parks happen before model call 15, most of
/// them the model sending an empty or non-question `ask_user` three times
/// running until `CONTINUE_REPEAT_CAP` (`core/src/session.rs`) parks the
/// run -- with no test failure ever open, so `implement_repeat_failure_thinks`
/// never sees it (that trigger requires `cur.harness.repeated_failure_step`
/// to be `Some`). This trigger catches the SECOND consecutive non-question
/// `ask_user` instead of waiting for a third, substituting `think` the same
/// way, so the model has the reasoning subagent's answer in hand before the
/// cap would otherwise park it on the very next repeat.
///
/// Fires when all of these hold:
/// - `out.action()` is `Action::AskUser { message }` with no `?` in
///   `message` -- the same shape `ask_user_empty_note`/
///   `ask_user_statement_note` convert to `Continue`, and
///   `implement_repeat_failure_thinks` converts to `think` when a repeat is
///   open. This function runs only when that one does NOT fire (see
///   `ImplementSkill::validate`'s ordering), so the two never both act on
///   the same step.
/// - `cur.harness.consecutive_non_question_asks >= 1`: the PREVIOUS
///   committed step was already a non-question `ask_user` (`core/src/
///   session.rs`'s `step`, which increments this counter from the model's
///   raw parsed action before `validate` runs) -- so this step is the
///   second in the streak, one short of the cap's third-strike park.
/// - No `think` has run since the last tool call: `cur.harness.
///   last_think_step` is `None`, or `Some(s)` with `s` at or before the
///   step of the most recent entry in `cur.agent.seen` (the harness's
///   per-path tool-call record, `CodingWorkspace::seen`) -- i.e. every
///   `seen` entry's `step` is `<= s`. Unlike `implement_repeat_failure_
///   thinks` (which requires a fresh EDIT since the last `think`), any
///   recorded tool call re-arms this trigger, since there is no repeated
///   test failure here for an edit to have failed to fix -- the shape this
///   catches is "nothing to ask" twice running with no tool call in
///   between at all, and a tool call in between is exactly what breaks that
///   streak (see `HarnessState::consecutive_non_question_asks`'s own doc).
///   `seen` only records `read_file`/`edit_file`/`write_file`/
///   `revert_file` (`common_after_tool`'s `think`-less branch), so a `test`
///   or a prior `think` call between the two asks would not appear there --
///   but a `test`/`think` in between also means `consecutive_non_question_
///   asks` itself would already be back at `0` (`core/src/session.rs`
///   resets it on any non-ask_user action), so the `>= 1` gate above already
///   excludes that case; the `seen` check here exists purely to enforce
///   "once per streak" against the `think` call THIS function itself just
///   substituted, the same role `implement_repeat_failure_thinks`'s
///   `last_think_step` check plays.
///
/// Effect: same substitution mechanism as `implement_repeat_failure_thinks`
/// (`substitute_think`), replacing the action with `Action::Tool { name:
/// "think", args: {"question": EARLY_PARK_QUESTION} }` and stripping a
/// stray `next_skill` from the raw patch. The substituted `think` step
/// itself sets `last_think_step` (`core/src/session.rs`'s `execute_think`),
/// so the trigger cannot fire again until a real tool call happens and adds
/// a fresh `seen` entry after it.
fn implement_early_park_thinks(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let is_non_question_ask = matches!(
        out.action(),
        statie_core::Action::AskUser { message } if !message.contains('?')
    );
    if !is_non_question_ask {
        return None;
    }

    if cur.harness.consecutive_non_question_asks < 1 {
        return None;
    }

    // "Once per streak": a `think` call already ran and no tool call has
    // landed since it (no `seen` entry past that step) -- calling `think`
    // again would just ask the same question with nothing new to answer.
    if let Some(think_step) = cur.harness.last_think_step {
        let tool_call_since = cur.agent.seen.iter().any(|f| f.step > think_step);
        if !tool_call_since {
            return None;
        }
    }

    let note = "you sent ask_user with nothing to ask twice in a row, so the harness asked the \
         reasoning subagent before a third repeat could park the run (its conclusion is below, \
         and in `decision`). Act on it."
        .to_string();

    Some(substitute_think(out, EARLY_PARK_QUESTION, note, true))
}

/// Unit 20: `try_fixes` is only useful once a failing `test` run is on
/// record for it to work from (`core/src/session.rs`'s `execute_try_fixes`
/// otherwise logs a `tool_error` and replies "no failing test to fix; run
/// test first" -- the harness now avoids ever sending it that call in the
/// first place). `ImplementSkill::tools()` cannot gate on workspace state:
/// `Skill::tools(&self) -> Vec<String>` takes no `cur`/workspace parameter
/// and is genuinely static (see `core/src/skill.rs`), so the gate lives
/// here instead, in `validate`.
///
/// Fires when the model's action is `Action::Tool { name: "try_fixes", .. }`
/// and `cur.agent.tests` is `None` or `Some(TestRecord { pass: true, .. })`
/// -- `tests` is harness-owned (`common_harness_owned_fields`) and replaced
/// whole on every `test` call (`common_after_tool`), so `cur.agent.tests`
/// (the already-committed record) is authoritative here; there is no
/// model-patchable copy to race against. Nothing fires when a failing
/// `tests` record exists (`Some(TestRecord { pass: false, .. })`):
/// `try_fixes` passes through unchanged.
///
/// Effect: same substitution mechanism as `implement_repeat_failure_thinks`
/// (`substitute_action`), replacing the action with `Action::Tool { name:
/// "test", args: {} }` (an auto-detected test run, same shape `implement_
/// no_green_runs_test` substitutes) and returning the note. This is a
/// substitution, not a rejection, so it does not count as a tool error.
fn implement_try_fixes_needs_failing_test(
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
    cur: &CommittedState<CodingWorkspace>,
) -> Option<String> {
    let is_try_fixes = matches!(
        out.action(),
        statie_core::Action::Tool { name, .. } if name == "try_fixes"
    );
    if !is_try_fixes {
        return None;
    }

    let has_failing_test = matches!(cur.agent.tests, Some(TestRecord { pass: false, .. }));
    if has_failing_test {
        return None;
    }

    let note = "`try_fixes` needs a failing test to work from, so the harness ran `test` instead."
        .to_string();

    Some(substitute_action(
        out,
        statie_core::Action::Tool {
            name: "test".to_string(),
            args: serde_json::json!({}),
        },
        note,
    ))
}

/// Shared substitution mechanism for both `implement_repeat_failure_thinks`
/// and `implement_early_park_thinks`: replaces `out`'s action with `Action::
/// Tool { name: "think", args: {"question": question} }` and returns `note`
/// as the step's attached note -- the same path both triggers use, so a
/// substituted call is logged, prompted, and picked up by `common_after_tool`
/// (writing `CodingWorkspace::decision`) identically no matter which trigger
/// fired it.
///
/// `strip_next_skill_if_ask_user` is `true` for the callers substituting an
/// `ask_user` (empty or non-question): a stray `next_skill` left over from
/// that `ask_user` would otherwise make `core/src/session.rs`'s step 7.5
/// reject the now-non-`Done` step, the same reason `ask_user_empty_note`/
/// `ask_user_statement_note` strip it. The edit/idle-test shapes pass
/// `false`: the model's action was already a tool call, so there is no
/// `next_skill` to strip.
fn substitute_think(
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
    question: &str,
    note: String,
    strip_next_skill_if_ask_user: bool,
) -> String {
    if strip_next_skill_if_ask_user && let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("next_skill");
    }

    substitute_action(
        out,
        statie_core::Action::Tool {
            name: "think".to_string(),
            args: serde_json::json!({"question": question}),
        },
        note,
    )
}

/// Shared mechanism behind every action substitution in this module
/// (`substitute_think`'s two callers, and [`shell_read_as_read_file`]'s
/// wiring into `PlanSkill`/`ImplementSkill`/`ReviewSkill::validate`):
/// replaces `out`'s action with `action` and returns `note` as the step's
/// attached note, so every substitution is logged, prompted, and picked up
/// by `common_after_tool` identically no matter which trigger fired it or
/// what action it substitutes in.
fn substitute_action(
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
    action: statie_core::Action,
    note: String,
) -> String {
    out.set_action(action);
    note
}

/// The equivalent `read_file` call [`shell_read_as_read_file`] recognises a
/// `shell` command as. Mirrors `tools::read_file::ReadFileArgs`'s shape
/// (`path`/`offset`/`limit`), but is defined locally here rather than
/// depending on the `statie-tools` crate (which itself depends on
/// `statie-skills` for nothing, but pulling in a whole crate for one args
/// struct would be a heavier dependency than this module needs) -- the
/// fields serialise to exactly the JSON `read_file`'s schema expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFileArgs {
    pub path: String,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

/// The note text [`shell_read_as_read_file`]'s substitution attaches,
/// filled in with the exact `shell` command the model sent (BACKLOG.md
/// item 16).
fn shell_read_substitution_note(cmd: &str) -> String {
    format!(
        "`shell {cmd}` was a plain read, so the harness ran it as `read_file` instead: the \
         read pages, lands in `seen`, and counts for the read guards. Use `read_file` directly \
         next time."
    )
}

/// Strips one layer of matching single or double quotes from `s`, if
/// present at both ends. Used for a quoted path/`sed` program.
fn strip_quotes(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

/// Whether `path` is safe to treat as a single plain file argument: no
/// glob metacharacter, and doesn't start with `-` (which would be an
/// option, not a path -- e.g. `cat -- -weird` or a flag this parser
/// doesn't otherwise recognise).
fn is_plain_path(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('-') && !path.contains(['*', '?', '['])
}

/// Recognises ONLY a single-file, pipe-free, redirect-free shell read
/// command (BACKLOG.md item 16), after trimming `cmd`, and returns the
/// `read_file` call it is equivalent to. Anything else -- a pipeline, a
/// redirect, a second path, a glob, an in-place edit, a non-read command --
/// returns `None`.
///
/// Recognised forms:
/// - `cat <path>`, `cat -n <path>`, `nl <path>` -- plain read (no
///   offset/limit).
/// - `head <path>` -- limit 10 (the harness cannot know the file's length
///   ahead of time, so this maps to `head`'s own default of 10 lines from
///   the start).
/// - `head -n N <path>` / `head -N <path>` -- offset 1, limit N.
/// - `tail <path>`, `tail -n N <path>` / `tail -N <path>` -- plain read (no
///   offset/limit): the skill cannot know the file's length, so there is no
///   way to compute the offset `tail`'s "last N lines" would need.
/// - `sed -n 'A,Bp' <path>` / `sed -n "A,Bp" <path>` / `sed -n A,Bp <path>`
///   -- offset A, limit B-A+1.
///
/// The path may be single- or double-quoted; it must not contain a glob
/// (`*`, `?`, `[`), must not start with `-`, and `cmd` as a whole must
/// contain none of `|`, `>`, `<`, `;`, `&&`, `||`, `$(`, a backtick, or a
/// second path-shaped token.
pub fn shell_read_as_read_file(cmd: &str) -> Option<ReadFileArgs> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    // Reject anything that could chain commands, redirect, substitute, or
    // otherwise not be a single flat invocation, before tokenising at all.
    if cmd.contains(['|', '>', '<', ';', '`'])
        || cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains("$(")
    {
        return None;
    }

    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let (prog, rest) = tokens.split_first()?;

    // Normalises a bare token into its path (stripping one layer of
    // matching quotes), rejecting anything glob-shaped or option-shaped.
    fn path_token(tok: &str) -> Option<String> {
        let inner = strip_quotes(tok).unwrap_or(tok);
        if is_plain_path(inner) {
            Some(inner.to_string())
        } else {
            None
        }
    }

    match *prog {
        "cat" => {
            // `cat <path>` or `cat -n <path>`: exactly one path token,
            // optionally preceded by `-n`.
            let path_args: Vec<&str> = match rest {
                [only] => vec![only],
                ["-n", only] => vec![only],
                _ => return None,
            };
            let path = path_token(path_args[0])?;
            Some(ReadFileArgs {
                path,
                offset: None,
                limit: None,
            })
        }
        "nl" => {
            let [only] = rest else { return None };
            let path = path_token(only)?;
            Some(ReadFileArgs {
                path,
                offset: None,
                limit: None,
            })
        }
        "tail" => {
            // Any of `tail <path>` / `tail -n N <path>` / `tail -N <path>`:
            // the harness cannot know the file's length, so every shape
            // maps to a plain read (no offset/limit) rather than guessing.
            match rest {
                [only] => {
                    let path = path_token(only)?;
                    Some(ReadFileArgs {
                        path,
                        offset: None,
                        limit: None,
                    })
                }
                ["-n", n, only] => {
                    n.parse::<u64>().ok()?;
                    let path = path_token(only)?;
                    Some(ReadFileArgs {
                        path,
                        offset: None,
                        limit: None,
                    })
                }
                [flag, only] if flag.starts_with('-') => {
                    flag[1..].parse::<u64>().ok()?;
                    let path = path_token(only)?;
                    Some(ReadFileArgs {
                        path,
                        offset: None,
                        limit: None,
                    })
                }
                _ => None,
            }
        }
        "head" => match rest {
            [only] => {
                let path = path_token(only)?;
                Some(ReadFileArgs {
                    path,
                    offset: None,
                    limit: Some(10),
                })
            }
            ["-n", n, only] => {
                let limit: u64 = n.parse().ok()?;
                let path = path_token(only)?;
                Some(ReadFileArgs {
                    path,
                    offset: Some(1),
                    limit: Some(limit),
                })
            }
            [flag, only] if flag.starts_with('-') => {
                let limit: u64 = flag[1..].parse().ok()?;
                let path = path_token(only)?;
                Some(ReadFileArgs {
                    path,
                    offset: Some(1),
                    limit: Some(limit),
                })
            }
            _ => None,
        },
        "sed" => {
            // `sed -n 'A,Bp' <path>` / `sed -n "A,Bp" <path>` / `sed -n
            // A,Bp <path>`: exactly `-n`, the range program, then one path
            // token.
            let (prog_tok, only) = match rest {
                ["-n", prog_tok, only] => (*prog_tok, *only),
                _ => return None,
            };
            let path = path_token(only)?;
            let inner = strip_quotes(prog_tok).unwrap_or(prog_tok);
            let inner = inner.strip_suffix('p')?;
            let (a_str, b_str) = inner.split_once(',')?;
            let a: u64 = a_str.trim().parse().ok()?;
            let b: u64 = b_str.trim().parse().ok()?;
            if b < a {
                return None;
            }
            Some(ReadFileArgs {
                path,
                offset: Some(a),
                limit: Some(b - a + 1),
            })
        }
        _ => None,
    }
}

/// `plan`/`implement`/`review`-shared trigger (BACKLOG.md item 16): when
/// `out`'s action is `Tool { name: "shell", args }` and `args.cmd` (the
/// `shell` tool's arg name, `tools/src/shell.rs`'s `ShellArgs::cmd`) parses
/// with [`shell_read_as_read_file`], replaces the action with the
/// equivalent `Tool { name: "read_file", args: {path, offset?, limit?} }`
/// (omitting `offset`/`limit` keys entirely when `None`, rather than
/// sending them as JSON `null` -- `read_file`'s own `Option<usize>` fields
/// default the same way either way, but omitting matches what a model
/// calling `read_file` directly would send) and returns the substitution
/// note. Runs before every other trigger in each skill's `validate` (the
/// think substitutions all require an edit/idle-test/ask_user shape a
/// `shell` action never has, so ordering relative to them does not matter
/// in practice; this still runs first so a substituted `read_file` call is
/// visible to every guard downstream, e.g. `Skill::silent_read_cap`, the
/// same as one the model sent directly).
///
/// Any other `shell` command, or any other action entirely, is left
/// untouched (`None`) -- most notably `cargo test`/`cargo build` and every
/// other non-read shell invocation, which must keep running as `shell`.
fn shell_read_substitution(out: &mut statie_core::ParsedOutput<CodingWorkspace>) -> Option<String> {
    let statie_core::Action::Tool { name, args } = out.action() else {
        return None;
    };
    if name != "shell" {
        return None;
    }
    let cmd = args
        .get("cmd")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let read_args = shell_read_as_read_file(&cmd)?;

    let mut read_file_args = serde_json::Map::new();
    read_file_args.insert(
        "path".to_string(),
        serde_json::Value::String(read_args.path),
    );
    if let Some(offset) = read_args.offset {
        read_file_args.insert("offset".to_string(), serde_json::json!(offset));
    }
    if let Some(limit) = read_args.limit {
        read_file_args.insert("limit".to_string(), serde_json::json!(limit));
    }

    let note = shell_read_substitution_note(&cmd);
    Some(substitute_action(
        out,
        statie_core::Action::Tool {
            name: "read_file".to_string(),
            args: serde_json::Value::Object(read_file_args),
        },
        note,
    ))
}

/// `plan`/`implement`/`review`-shared trigger (BACKLOG.md item 15): when
/// `out`'s action is `Tool { name: "write_file", args }`, first strips any
/// `confirmed_full_read` key the model itself sent in `args` -- that flag is
/// harness-set only (`tools/src/write_file.rs`'s `WriteFileArgs`, hidden
/// from the model's schema with `#[schemars(skip)]`); the model must never
/// be able to claim the confirmation on its own, so whatever it sent there
/// is discarded before anything else looks at `args`. Then, if [`write_file_
/// confirmed_by_seen`] says `cur.agent.seen` already shows a full, recent
/// read of the target path, sets `args.confirmed_full_read = true` so the
/// tool call skips its whole-file-rewrite line cap. Silent either way -- no
/// note is attached; when the harness does NOT set the flag and the file is
/// over the cap, `write_file`'s own refusal text explains the rule directly
/// to the model.
///
/// A `write_file` call with no `path` (or a non-object `args`) is left
/// alone -- that is a malformed call the tool itself will reject with its
/// own error, not this function's to diagnose.
fn write_file_auto_confirm(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) {
    let statie_core::Action::Tool { name, args } = out.action() else {
        return;
    };
    if name != "write_file" {
        return;
    }

    let confirmed = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|p| write_file_confirmed_by_seen(&cur.agent.seen, p));

    let statie_core::Action::Tool { name, mut args } = out.action().clone() else {
        unreachable!("checked above")
    };
    if let serde_json::Value::Object(map) = &mut args {
        map.remove("confirmed_full_read");
        if confirmed {
            map.insert(
                "confirmed_full_read".to_string(),
                serde_json::Value::Bool(true),
            );
        }
    }
    out.set_action(statie_core::Action::Tool { name, args });
}

/// Applies `out`'s raw patch to `cur` the same way `strip_malformed_next`
/// peeks at the merged workspace: a patch that fails to deserialize/apply
/// returns `None`, leaving the caller to fall through to `validate_default`
/// for the real error.
fn merged_workspace(
    cur: &CodingWorkspace,
    out: &statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<CodingWorkspace> {
    serde_json::from_value::<
        <JsonMergePatch<CodingWorkspace> as statie_core::PatchStrategy<CodingWorkspace>>::Patch,
    >(out.patch().0.clone())
    .ok()
    .and_then(|patch| {
        <JsonMergePatch<CodingWorkspace> as statie_core::PatchStrategy<CodingWorkspace>>::apply(
            cur, &patch,
        )
        .ok()
    })
}

/// Whether applying `out`'s raw patch to `cur` (via `merged_workspace`)
/// yields a non-empty `plan`. Shared by `PlanSkill::validate`'s `is_done`
/// block (unit 22) and its `ask_user`-with-a-written-plan hand-off (unit
/// 30): both need the same peek at the merged plan before deciding how to
/// treat the step's action. `None` (patch fails to deserialize/apply) is
/// distinct from `Some(false)` (patch applies cleanly to an empty plan) --
/// callers that only care about the non-empty case can collapse the two
/// with `== Some(true)`; the `is_done` block still needs to keep them
/// separate so a malformed patch falls through to `validate_default`.
fn merged_plan_nonempty(
    cur: &CodingWorkspace,
    out: &statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<bool> {
    merged_workspace(cur, out).map(|next| !next.plan.is_empty())
}

/// Whether `merged.next_skill` names a real handoff for `skill_name`: `Some`
/// naming any OTHER skill counts; `None`, or `Some` naming `skill_name`
/// itself, does not -- a self-handoff is never valid (`common_handoff`
/// already filters it out of `Skill::handoff`'s own result), so a stale
/// `next_skill` left over from the handoff that landed THIS activation
/// (`Skill::on_activate` normally clears it, but a hook error or a caller
/// that bypasses it could still leave one in place) must not be misread as
/// "a handoff is already named" by conversions like `implement_bare_done`.
///
/// When `merged.next_skill` is the stale self-referential case, also strips
/// `next_skill` from `out`'s raw patch if it carries one there too, so a
/// step this fires for doesn't accidentally commit the very value it just
/// decided to ignore.
fn has_named_handoff(
    skill_name: &str,
    merged: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> bool {
    match merged.next_skill.as_deref() {
        Some(target) if target != skill_name => true,
        Some(_) => {
            if let Some(obj) = out.patch_mut().0.as_object_mut() {
                obj.remove("next_skill");
            }
            false
        }
        None => false,
    }
}

/// The harness has learned that rejecting a step throws away the state
/// progress in its patch and burns one of three retries, and that a small
/// local model then flails: `done`/`ask_user` sent in a shape that can
/// never be the right move (a plan step still open, no question actually
/// asked) used to reject the whole step -- discarding a patch that may
/// carry real progress (e.g. the step it just finished marking `done`) and
/// spending a retry the model had no way to spend well. These two checks
/// instead commit the patch, run no tool, and tell the model in one line
/// what is still open via the harness-internal `Action::Continue` (see
/// `core/src/model.rs`'s doc on that variant and `core/src/session.rs`'s
/// handling of it). Shared by `implement`'s and `review`'s `validate`
/// (case b only, for `review` -- its `done` semantics are unaffected);
/// `plan` also gets case b, on top of its own existing bare-`done`
/// injection.
///
/// Checked in order, returning the first that applies:
///
/// a. `done` (regardless of `next_skill`) while the *merged* (post-patch)
///    plan still has a step that is `pending` or `in_progress`, and the
///    merged workspace is not itself terminal (`done: true`) -- only
///    called for skills where this applies (`implement`; not `plan`, whose
///    own bare-`done` handling covers its semantics, and not `review`,
///    whose `done` means something else). `next_skill` is stripped from
///    the raw patch first so the handoff does not silently land once the
///    action is no longer `Done`. Points at the first open step in plan
///    order. When the merged plan has NO open step, this check does not
///    fire -- `done` falls through to the existing terminal/handoff rules.
///
///    For `implement`, this case no longer has top priority: it is called
///    AFTER `implement_no_green_runs_test` now (see that function's doc),
///    which substitutes a real `test` call ahead of this note whenever
///    verification is unverified (`green_step: None`) and either something
///    was edited since the last test or none has ever run -- an open plan
///    step no longer blocks that substitution. This case (a) still fires,
///    for `implement`, only in the one shape `implement_no_green_runs_test`
///    leaves alone: `green_step: None`, an open step, a FAILING run as the
///    latest, and NOTHING edited since -- there the open step is real work,
///    since re-running the identical failing command would tell the model
///    nothing new.
/// b. `ask_user` with an empty/whitespace `message` -- nothing was
///    actually asked, so parking the session on it would just stall.
///
/// Returns `None` when neither applies (the action is left alone).
fn premature_done_note(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if matches!(out.action(), statie_core::Action::Done) {
        let merged = merged_workspace(cur, out)?;
        if !merged.done
            && let Some(open) = merged
                .plan
                .iter()
                .find(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress))
        {
            let note = format!(
                "plan step {} is still open: {}. If that step's work is already done, set its \
                 status to done in your patch and run `test`; if not, make the change with \
                 edit_file/write_file, then `test`. `done` is for a finished plan; your patch \
                 was kept.",
                open.id, open.text
            );
            if let Some(obj) = out.patch_mut().0.as_object_mut() {
                obj.remove("next_skill");
            }
            return Some(note);
        }
    }
    ask_user_empty_note(out)
}

/// Live run 59: `implement` handed off to `review` right after its own
/// `test` call failed (a non-exhaustive match) -- the merged plan had no
/// open step, so neither `green_done_note` nor `premature_done_note`'s case
/// (a) fired, and the handoff landed with a tree that does not compile.
/// `review` then ran the unfiltered test, saw it fail, and approved anyway.
///
/// The bench battery's implement-side sibling of
/// `review_blind_approval_runs_test`: three of four bench misses were
/// `implement` sending `done` where it should have run `test` -- the
/// note-only conversion this replaces (`implement_no_green_note`, telling
/// the model to "Run `test` now") told it so three times running, and the
/// repeat cap parked the run anyway. Telling this model to run the test
/// does not work; running it for it does, the same lesson
/// `review_blind_approval_runs_test` already applied on the review side.
///
/// A later live run showed the priority below was itself wrong: the model
/// sent `done` with its "run cargo test" plan step still open AND its edits
/// unverified (`edits_since_test > 0`) -- `premature_done_note`'s case (a)
/// fired first (open step), converting to a `Continue` note that just told
/// the model to run the test itself. It didn't; the note repeated three
/// times and the repeat cap parked the session as a question. Running the
/// test is the right next move regardless of what plan step is open, so
/// this function is now called in `ImplementSkill::validate` BEFORE
/// `premature_done_note`, and its own open-step check is gone from the
/// test-substitution branch below: an open step no longer blocks it.
/// `premature_done_note` keeps priority only in the one case where running
/// the test again would tell the model nothing new -- see that case below.
///
/// Called in `ImplementSkill::validate` AFTER `green_done_note` and
/// `implement_bare_done`: only reached when the action is still `Done`
/// after both of those, i.e. `cur.harness.green_step` is `None` (they both
/// require `green_step.is_some()`) or, for `green_done_note`, the merged
/// plan had no open step to rewrite and `implement_bare_done` didn't apply
/// either. Fires when `cur.harness.green_step` is `None`: the most recent
/// `test` run anywhere in the session failed, or none has ever run, AND the
/// merged workspace is not itself terminal (`done: true`). Two cases:
///
/// - `cur.harness.edits_since_test > 0` (something was edited since the
///   last/only test run) OR `cur.harness.last_test_cmd.is_none()` (no
///   `test` call has ever run this session, so there is nothing to point
///   the model back at as "the failure to go fix"): the action is replaced
///   with `Action::Tool { name: "test", args }`, `args = {"cmd": cmd}`
///   when `cur.harness.last_test_cmd` is `Some`, else `{}` (auto-detect).
///   `"done"` and `"next_skill"` are stripped from the raw patch, the same
///   "leave the committed field alone" treatment
///   `review_blind_approval_runs_test` gives an approval it substitutes a
///   test for. This mirrors `review_blind_approval_runs_test` exactly: the
///   harness runs the tests itself rather than just asking the model to.
///   Fires regardless of whether the merged plan has an open step: running
///   the test is the right next move whatever is open, and the substituted
///   call's own observation says what it says.
/// - Otherwise (a failing run is the latest, AND nothing has been edited
///   since): running the same failing command again would tell the model
///   nothing new, so this function does not touch the step (`None`) when
///   the merged plan has an open step -- the open step is real work, so
///   `premature_done_note`'s case (a) keeps its priority and converts with
///   the "still open" note instead. With no open step, this falls back to
///   the old behaviour -- `next_skill` stripped, action converted to
///   `Continue` with the unchanged note -- so the repeat cap still catches
///   a model that will not edit before trying `done` again.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `premature_done_note`/`validate_default`, which
/// report the real error.
fn implement_no_green_runs_test(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    if cur.harness.green_step.is_some() {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if merged.done {
        return None;
    }
    let has_open_step = merged
        .plan
        .iter()
        .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress));

    let should_run_test = cur.harness.edits_since_test > 0 || cur.harness.last_test_cmd.is_none();

    if should_run_test {
        // Fires even with an open plan step: running the test is the right
        // next move whatever is open (see this function's doc).
        if let Some(obj) = out.patch_mut().0.as_object_mut() {
            obj.remove("done");
            obj.remove("next_skill");
        }
        let args = match &cur.harness.last_test_cmd {
            Some(cmd) => serde_json::json!({"cmd": cmd}),
            None => serde_json::json!({}),
        };
        out.set_action(statie_core::Action::Tool {
            name: "test".to_string(),
            args,
        });

        return Some(
            "you sent `done` without a passing test run, so the harness ran the tests for you \
             (below). If they pass, send `done` again; if they fail, fix what they report first."
                .to_string(),
        );
    }

    if has_open_step {
        // The latest test failed and nothing has been edited since: the
        // open step is real work, and `premature_done_note`'s case (a)
        // keeps priority over this fallback (see this function's doc).
        return None;
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("next_skill");
    }

    Some(
        "the most recent `test` run failed, or none has run yet; handing off needs a passing \
         run. Run `test` now; if it fails, fix what it reports before `done`."
            .to_string(),
    )
}

/// A live run showed `premature_done_note`'s case (a) steering the model the
/// wrong way right after verification passed: the target test passed and
/// the green checkpoint fired, the model sent `done` (the right move), but
/// its plan still had the "run the test" step open, so case (a) converted
/// it to `Continue` with "make the change with a tool call, then `test`" --
/// which sent the model off to make an unrelated change that then broke the
/// checkpointed state. `test` having just reported `result: pass`
/// (`cur.harness.green_step.is_some()`, committed by the session right when
/// that happens -- see `core/src/session.rs`'s green-checkpoint bookkeeping
/// in `Active::execute`) means the work IS done as of this step; a `done`
/// sent right after should hand off, not be redirected into more edits.
///
/// Called before `premature_done_note` in `implement`'s `validate`: when
/// `cur.harness.green_step` is set and the action is `done` (any
/// `next_skill`, or none), this fires INSTEAD of case (a) -- it never falls
/// through to it. Rewrites the raw patch so every plan step that is
/// `pending`/`in_progress` in the *merged* (model's own patch applied)
/// workspace becomes `done` (merging the model's own patch first, so an
/// entry it already sent for a given step is preserved and only its
/// `status` is forced to `done`), injects `next_skill: "review"` if the
/// model didn't set one, and returns a note recording what happened. Once
/// every step reads `done`, `implement`'s own "cannot hand off to review
/// while a plan step is pending or in_progress" rule (`ImplementSkill::
/// rules`) passes naturally against the rewritten patch.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `premature_done_note`/`validate_default`, which
/// report the real error. Not called for `ask_user` at all -- case (b) of
/// `premature_done_note` is untouched by this function.
fn green_done_note(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let green_step = cur.harness.green_step?;
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    let open_ids: Vec<u32> = merged
        .plan
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress))
        .map(|s| s.id)
        .collect();
    if open_ids.is_empty() {
        // Nothing to rewrite -- the ordinary handoff rules already accept
        // this `done` (or `premature_done_note`'s own checks, if any other
        // reason to convert applies, e.g. an empty ask_user can't co-occur
        // with `done` anyway).
        return None;
    }

    let obj = out.patch_mut().0.as_object_mut()?;
    let plan_entry = obj
        .entry("plan")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let serde_json::Value::Array(plan_entries) = plan_entry else {
        // The model sent a non-array `plan` (malformed) -- leave it to
        // `validate_default` to report the real error.
        return None;
    };
    for id in &open_ids {
        if let Some(existing) = plan_entries.iter_mut().find(|e: &&mut serde_json::Value| {
            e.get("id").and_then(serde_json::Value::as_u64) == Some(u64::from(*id))
        }) {
            if let serde_json::Value::Object(map) = existing {
                map.insert(
                    "status".to_string(),
                    serde_json::Value::String("done".to_string()),
                );
            }
        } else {
            plan_entries.push(serde_json::json!({"id": id, "status": "done"}));
        }
    }
    if !obj.contains_key("next_skill") {
        obj.insert(
            "next_skill".to_string(),
            serde_json::Value::String("review".to_string()),
        );
    }

    Some(format!(
        "verification passed at step {green_step}; open plan steps marked done and handed off \
         to review"
    ))
}

/// With thinking on, eight of the bench battery's `implement` rejections
/// were `validate_default`'s core rule "action `done` is only valid when the
/// workspace is terminal or hands off": `implement` sent `done` with neither
/// `done: true` nor `next_skill` set, after finishing the plan and passing
/// verification -- a shape none of the checks above catch, since
/// `green_done_note` only rewrites the patch when the merged plan has an
/// OPEN step (`open_ids` non-empty) and `premature_done_note`'s case (a)
/// only fires on an open step too. A plan that is already fully done, with
/// verification already green, has nothing left for either of those to
/// steer -- the model just forgot to name a handoff, and eight rejections
/// telling it to set `next_skill` never got it to add the field.
///
/// Called in `ImplementSkill::validate` right after `green_done_note`
/// (reached only when that did not already rewrite the patch: either
/// `cur.harness.green_step` is `None`, or it fired and found no open step to
/// rewrite) and before `premature_done_note`. Fires when the action is
/// `Done`, the merged workspace is not itself terminal (`done: true`) and
/// has no `next_skill` set (a real bare `done` -- one that already names a
/// target, or that sets `done: true`, is not touched: the ordinary
/// terminal/handoff rules already accept it). Two cases:
///
/// - `cur.harness.green_step.is_some()` AND the merged plan has no open
///   step: verification already passed and there is nothing left to do, so
///   this converts what would otherwise be a rejection into the intended
///   handoff -- `next_skill: "review"` is injected into the raw patch,
///   action stays `Done`, and a note records what happened.
/// - Otherwise (a plan step is still open, or no green run): this function
///   does not touch the step at all (`None`) -- it falls straight through
///   to `premature_done_note` (open step) or `implement_no_green_runs_test`
///   (no green), which already handle a `done` in that shape regardless of
///   whether `next_skill` was set, so the caller's existing chain covers it
///   without this function's help.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `premature_done_note`/`implement_no_green_runs_test`/
/// `validate_default`, which report the real error.
fn implement_bare_done(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if merged.done {
        return None;
    }
    if has_named_handoff("implement", &merged, out) {
        return None;
    }
    cur.harness.green_step?;
    let has_open_step = merged
        .plan
        .iter()
        .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress));
    if has_open_step {
        return None;
    }

    let obj = out.patch_mut().0.as_object_mut()?;
    obj.insert(
        "next_skill".to_string(),
        serde_json::Value::String("review".to_string()),
    );

    Some(
        "`done` with no handoff named; verification passed, so this hands off to review."
            .to_string(),
    )
}

/// `review`-only: live runs 69/70 showed `review` sending `done: true` with
/// empty `findings` and no tool call, over and over -- the existing
/// look-first conversion (`review_looked_note`, right below) turned each
/// into an `Action::Continue` with a note asking it to read the changed
/// files and run the unfiltered tests, but the model just repeated the
/// blind approval; the repeat cap parked the session as a question three
/// times running (runs 45-46 had the same shape, 35 repeats, before the cap
/// existed). Run 71 showed what does work: once the model has a real test
/// observation in front of it, it follows the harness's steer. So instead
/// of asking for a look, this substitutes one: the harness runs the tests
/// itself and shows the model the result.
///
/// Called in `ReviewSkill::validate` BEFORE `review_looked_note`, under the
/// same three conditions that function checks (action `done`; the merged
/// workspace is an approval, `done: true` with empty `findings`;
/// `cur.harness.tool_calls == 0`, nothing executed yet this activation).
/// When those hold, replaces the action with `Action::Tool { name: "test",
/// args }`, where `args` is `{"cmd": <last_test_cmd>}` when
/// `cur.harness.last_test_cmd` (`core/src/version.rs`, set at the same call
/// site as `green_step` from the most recent `test` call's own args) is
/// `Some`, or `{}` (auto-detect) when it is `None`. Strips `"done"` and
/// `"next_skill"` from the raw patch, the same "leave the committed field
/// alone" treatment `review_looked_note` gives them, since the action is no
/// longer `Done`.
///
/// The session executes whatever action `validate` leaves in place
/// (`core/src/session.rs`), so the substituted `test` call runs as a normal
/// tool call: it logs an `Action` entry, produces real tool output, and
/// increments `HarnessState::tool_calls` to 1 -- exactly like a call the
/// model chose to make itself. That means this fires at most once per
/// review activation: a second blind approval in the same activation has
/// `tool_calls >= 1` and no longer matches this function's guard, so it
/// falls through to `review_looked_note` (which also requires
/// `tool_calls == 0` and so won't fire either) and on to
/// `review_no_green_note`/`review_filtered_note` if the substituted run
/// failed or was filtered, or is accepted as a legitimate approval if the
/// substituted run passed clean.
///
/// If the substituted `test` call would itself be caught by the
/// redundant-verification guard (same command, nothing edited since
/// implement's last run) -- that is fine: the skip observation quotes the
/// earlier result, which is still a real look, so no special-casing is
/// needed here.
///
/// A `done` that sends the work back (`findings` non-empty) is untouched,
/// matching `review_looked_note`'s own `findings`-empty requirement -- see
/// that function's doc for why the malformed `done: true` +
/// non-empty-`findings` shape is excluded too.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
fn review_blind_approval_runs_test(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    if cur.harness.tool_calls > 0 {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if !merged.done || !merged.findings.is_empty() {
        return None;
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
        obj.remove("next_skill");
    }

    let args = match &cur.harness.last_test_cmd {
        Some(cmd) => serde_json::json!({"cmd": cmd}),
        None => serde_json::json!({}),
    };
    out.set_action(statie_core::Action::Tool {
        name: "test".to_string(),
        args,
    });

    Some(
        "you approved without looking at anything, so the harness ran the tests for you \
         (below). Read the changed files if the result is not enough to judge, then approve \
         with `done: true` or send it back with findings."
            .to_string(),
    )
}

/// `review`-only: a live run showed `review`'s first and only step
/// approving a diff it never read -- `{"patch": {"done": true, "findings":
/// []}, "action": {"type": "done"}}`, 71 completion tokens, no tool call at
/// all. Nothing in the rules above catches this: an approval is a
/// well-formed `done` with empty `findings`, exactly what `ReviewSkill::
/// rules` expects.
///
/// Called before `validate_default` in `ReviewSkill::validate`, ahead of
/// `strip_malformed_next`'s own patching, and AFTER
/// `review_blind_approval_runs_test`: that function fires under the exact
/// same three conditions this one checks, so once it has substituted the
/// action for a `test` call, this function's own `action()` check (looking
/// for `Done`) no longer matches and it is a no-op for that step. This
/// function remains the fallback for the shapes `review_blind_approval_runs_test`
/// does not touch (see that function's doc): once the substituted call has
/// run and `tool_calls` is no longer `0`. Fires when all three hold: the
/// action is `done`; the *merged* (post-patch) workspace is an approval
/// (`done: true`); and `cur.harness.tool_calls` (the number of tool calls
/// that actually executed so far in this activation, harness-owned --
/// `core/src/version.rs`) is `0` -- nothing has been looked at yet. Strips
/// `"done"` from the raw patch (so the approval itself does not land -- an
/// existing `done: false` stays `false`, the same "leave the committed
/// field alone" treatment `strip_malformed_next` gives a dropped `next`)
/// and returns a note for the caller to convert the action to
/// `Action::Continue` and attach via `with_notes`.
///
/// A `done` that sends the work back (`findings` non-empty) is untouched --
/// review is allowed to reject work sight-unseen is not the concern here,
/// only an approval is. This also covers the malformed shape `done: true`
/// with non-empty `findings`: that combination is not an approval either
/// (`ReviewSkill::rules`' own "done can only be true when findings is
/// empty" check must still fire for it, tool_calls or no), so this function
/// requires empty `findings` in the merged workspace too, not just
/// `done: true`, before treating it as the approval this unit is about.
/// `tool_calls >= 1` is also untouched: one look is enough to satisfy this
/// check (it does not judge whether the look was relevant, only that one
/// happened).
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
///
/// Also strips `next_skill` from the raw patch, the same as
/// `premature_done_note`'s case (a): the caller converts the action to
/// `Action::Continue`, and `core/src/session.rs`'s step 7.5 rejects any step
/// whose patch changes the handoff target unless the action is `Done` -- a
/// live run sent `done: true` with an empty `next_skill` 35 times, then once
/// added `next_skill: "implement"` to the same approval; left in place, that
/// stray `next_skill` would make step 7.5 reject the now-`Continue` step
/// instead of converting it, defeating this whole conversion.
fn review_looked_note(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    if cur.harness.tool_calls > 0 {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if !merged.done || !merged.findings.is_empty() {
        return None;
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
        obj.remove("next_skill");
    }

    Some(
        "review has not looked at anything yet. Read the changed files (the `files` list, or \
         `shell` `git diff`) and run the unfiltered tests, then approve with `done: true` or \
         send the work back with findings."
            .to_string(),
    )
}

/// Live run 59: `review` ran the unfiltered test right after `implement`'s
/// own `test` call had failed, saw it fail, and approved anyway (`done:
/// true`, `findings: []`) -- the harness ended in `done` with a tree that
/// does not compile.
///
/// Called in `ReviewSkill::validate` AFTER `review_looked_note` (look-first
/// keeps priority: a `done` sent before any tool call in this activation
/// converts via that check instead, regardless of `green_step`) and only
/// for what would otherwise be an approval -- the case `review_sendback_note`
/// does NOT convert (`done: true` with empty `findings` in the merged
/// workspace; a send-back, non-empty `findings`, is never affected by this
/// rule, matching `review_looked_note`'s own `findings`-empty requirement).
/// Fires when `cur.harness.green_step` is `None`: the most recent `test` run
/// anywhere in the session failed, or none has run. Converts the action to
/// `Action::Continue`; the patch is kept, `done` is stripped from the raw
/// patch so the approval itself does not land (same treatment
/// `review_looked_note` gives a look-first failure), and `next_skill` is
/// stripped too so no handoff lands.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
fn review_no_green_note(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    if cur.harness.green_step.is_some() {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if !merged.done || !merged.findings.is_empty() {
        return None;
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
        obj.remove("next_skill");
    }

    Some(
        "the most recent test run failed, or none has run in this session; approval needs a \
         passing unfiltered run. Run the tests; if they fail, send it back with `done: true` \
         and the failure as a finding."
            .to_string(),
    )
}

/// `review`-only sibling of `review_no_green_note`, same treatment for a
/// different gap: live run 67, `implement` passed the target test with a
/// test-name filter, the `test` tool's own output noted tests were
/// filtered out, and `review` ran that SAME filtered command, saw it pass,
/// and approved -- the whole suite failed on disk (an arm the new code
/// shadows) that the filtered run never exercised. `green_step` alone
/// can't catch this: a filtered pass still sets it (mirroring
/// `note_test_result`'s pass/fail-only bookkeeping), so this checks
/// `cur.harness.green_filtered` instead, set at the same call site as
/// `green_step` (`core/src/session.rs`) from the `test` tool output's
/// filtered-count note (`tools/src/test_tool.rs`'s `filtered_out_note`).
///
/// Called in `ReviewSkill::validate` right after `review_no_green_note`
/// (which already handles `green_step.is_none()`) and under the same
/// conditions otherwise: only for what would be an approval (`done: true`,
/// empty `findings` in the merged workspace), after `review_looked_note`
/// keeps its look-first priority, and never for a send-back. Fires when
/// `green_step.is_some() && green_filtered`. Converts the action to
/// `Action::Continue`; the patch is kept, `done` and `next_skill` stripped
/// from the raw patch, same as `review_no_green_note`.
///
/// Implement is NOT affected: a filtered pass is still a green checkpoint
/// for `implement`'s own `done` validation
/// (`implement_no_green_runs_test`/plan-complete check), which only cares
/// that SOME recent run passed, not whether it was filtered.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
fn review_filtered_note(
    cur: &CommittedState<CodingWorkspace>,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    if !matches!(out.action(), statie_core::Action::Done) {
        return None;
    }
    if cur.harness.green_step.is_none() || !cur.harness.green_filtered {
        return None;
    }
    let merged = merged_workspace(&cur.agent, out)?;
    if !merged.done || !merged.findings.is_empty() {
        return None;
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
        obj.remove("next_skill");
    }

    Some(
        "the passing test run was filtered, so part of the suite never ran; approval needs an \
         unfiltered pass. Run the same test command without the test-name filter; if it fails, \
         send it back with `done: true` and the failure as a finding."
            .to_string(),
    )
}

/// `review`-only: converts two more shapes a live run sent when it meant
/// "send the work back to implement with these findings" -- both used to be
/// rejected outright, burning a retry each, until the run failed on the
/// retry cap.
///
/// Checked in order, returning the first that applies:
///
/// 1. Action `done`, merged (post-patch) workspace has `done: true` AND
///    non-empty `findings` -- not a valid approval (`ReviewSkill::rules`'s
///    "done can only be true when findings is empty" would reject it) and
///    not a valid send-back either (a send-back needs `done: false`). The
///    live run sent exactly this shape twice: `{"done": true, "findings":
///    ["..."]}`. Rewritten in place: `done` forced to `false`,
///    `next_skill` forced to `"implement"` unless the model already set a
///    *different* valid transition for this skill (checked the same way
///    `common_rules` checks it: equal to `"review"` itself, or in
///    `allowed_transitions`) -- honoring the model's own routing choice
///    when it made one. Action stays `Done`, so this yields a handoff.
/// 2. Action is a tool call or `ask_user`, and the raw patch sets
///    `next_skill` to a valid transition -- not a handoff at all (the core
///    rule "changing `next_skill` requires action `done`",
///    `core/src/session.rs`) would reject the whole step here. The live run
///    sent this shape twice too: a tool call carrying `next_skill:
///    "implement"`. Two cases, on the *merged* `findings`:
///    - non-empty: the model has findings and a tool call was just a
///      look, not a decision to keep working -- this IS the intended
///      handoff, so the action is replaced with `Done`.
///    - empty: nothing to send back, so `next_skill` is dropped from the
///      raw patch (the same "leave the committed field alone" treatment
///      `strip_malformed_next` gives a dropped `next`) and the tool call
///      runs as scheduled.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
/// `cur.harness.tool_calls == 0` is not consulted here -- the look-first
/// rule (`review_looked_note`) gates approvals only, never a send-back.
fn review_sendback_note(
    review: &ReviewSkill,
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let is_valid_transition = |target: &str| -> bool {
        target == "review" || review.allowed_transitions().iter().any(|t| t == target)
    };

    if matches!(out.action(), statie_core::Action::Done) {
        let merged = merged_workspace(cur, out)?;
        if merged.done && !merged.findings.is_empty() {
            let keep_next_skill = merged
                .next_skill
                .as_deref()
                .map(|t| t != "review" && is_valid_transition(t))
                .unwrap_or(false);

            let obj = out.patch_mut().0.as_object_mut()?;
            obj.insert("done".to_string(), serde_json::Value::Bool(false));
            if !keep_next_skill {
                obj.insert(
                    "next_skill".to_string(),
                    serde_json::Value::String("implement".to_string()),
                );
            }

            return Some(
                "findings are not an approval: `done` set back to false and the work handed \
                 to implement with your findings"
                    .to_string(),
            );
        }
        return None;
    }

    if !matches!(
        out.action(),
        statie_core::Action::Tool { .. } | statie_core::Action::AskUser { .. }
    ) {
        return None;
    }

    let target = out.patch().0.get("next_skill").and_then(|v| v.as_str())?;
    if !is_valid_transition(target) {
        return None;
    }

    let merged = merged_workspace(cur, out)?;
    if !merged.findings.is_empty() {
        out.set_action(statie_core::Action::Done);
        Some(
            "`next_skill` with a tool call is not a handoff; sent `done` with your findings \
             instead"
                .to_string(),
        )
    } else {
        if let Some(obj) = out.patch_mut().0.as_object_mut() {
            obj.remove("next_skill");
        }
        Some(
            "`next_skill` is ignored until you approve (`done: true`) or record findings; the \
             tool call ran"
                .to_string(),
        )
    }
}

/// Parses the first `<path>:<digits>` token out of a review finding (item
/// 12: grounded review findings) -- the location a grounded finding must
/// cite. Scans whitespace-delimited words left to right; each word is first
/// trimmed of a light shell of wrapping punctuation (`()[]{}"',;`) so shapes
/// like `"(src/foo.rs:12)"` or `"src/foo.rs:12,"` still match. The first word
/// left with a `:` is a candidate: `path` is everything before that `:` (must
/// be non-empty), and `line` is the maximal run of ASCII digits right after
/// it (so `"src/foo.rs:12:something"` and `"src/foo.rs:12."` both yield line
/// `12` -- trailing non-digit text after the digits does not disqualify the
/// match). A word whose text after the first `:` starts with a non-digit
/// (e.g. a bare `"note:"`) is skipped in favor of the next word, not treated
/// as a failed parse of the whole finding.
fn parse_finding_location(finding: &str) -> Option<(&str, u32)> {
    for raw_word in finding.split_whitespace() {
        let word = raw_word.trim_matches(|c: char| "()[]{}\"',;".contains(c));
        let Some(colon) = word.find(':') else {
            continue;
        };
        let path = &word[..colon];
        if path.is_empty() {
            continue;
        }
        let rest = &word[colon + 1..];
        let digit_len = rest.chars().take_while(char::is_ascii_digit).count();
        if digit_len == 0 {
            continue;
        }
        if let Ok(line) = rest[..digit_len].parse::<u32>() {
            return Some((path, line));
        }
    }
    None
}

/// Whether `path` is present in the sandbox per the workspace's own records
/// -- `files` (the model's manifest of files touched/relevant) or `seen`
/// (the harness's own read/edit ledger) -- and, if so, the line count to
/// check `line` against: `seen`'s recorded `lines` when that entry has one
/// (`> 0`; see `SeenFile::lines`'s doc -- `0` means never learned), else
/// `None` (present, but no known length to check against -- see
/// `finding_is_grounded`'s doc for what that means for the line check).
fn known_file_line_count(ws: &CodingWorkspace, path: &str) -> Option<Option<u32>> {
    if let Some(entry) = ws.seen.iter().find(|f| f.path == path) {
        return Some((entry.lines > 0).then_some(entry.lines));
    }
    if ws.files.iter().any(|f| f.path == path) {
        return Some(None);
    }
    None
}

/// Whether a review finding is grounded (item 12): it must contain a
/// `<path:line>` citation (`parse_finding_location`) naming a file present
/// in the sandbox (`ws.files` or `ws.seen`) with `line` within that file's
/// known line count. When the file is known but its line count is not (no
/// `seen` entry with a nonzero `lines`, e.g. a `files` entry never actually
/// read), the citation is accepted on the path alone -- there is nothing to
/// range-check yet, and the harness must not penalize a finding for citing a
/// file it has not itself read the length of.
fn finding_is_grounded(ws: &CodingWorkspace, finding: &str) -> bool {
    let Some((path, line)) = parse_finding_location(finding) else {
        return false;
    };
    if line == 0 {
        return false;
    }
    match known_file_line_count(ws, path) {
        Some(Some(lines)) => line <= lines,
        Some(None) => true,
        None => false,
    }
}

/// `review`-only (item 12: grounded review findings; `guards.
/// grounded_findings`, `core/src/guards.rs`, default on): on a send-back
/// (the merged workspace's `findings` is non-empty), drops every ungrounded
/// finding (`finding_is_grounded`) from the raw patch's `findings` array in
/// place. Returns `Ok(None)` when nothing was dropped (including when
/// `findings` was not sent in this patch at all, or the merged `findings` is
/// empty -- an approval, not this rule's concern) or when the switch is off.
/// Returns `Ok(Some(note))` when some (but not all) findings were dropped,
/// the note naming exactly which ones and why -- the raw patch's `findings`
/// is rewritten to the grounded subset, in its original order. Returns
/// `Err(PatchError::RuleViolation(note))` when every finding was dropped:
/// the send-back is refused outright, the same as any other rule violation
/// (the step never commits, the model sees the reason and gets a retry) --
/// unlike a partial drop, there is no grounded subset left to send back with.
///
/// Only inspects/rewrites `findings` when the raw patch actually carries the
/// key (`out.patch().0.get("findings")`) -- a patch that leaves `findings`
/// untouched (committed findings, if any, unchanged) is not this rule's
/// concern either, matching `review_sendback_note`'s own pattern of only
/// acting on what the model actually sent.
///
/// Called in `ReviewSkill::validate` after `review_sendback_note` has had a
/// chance to normalize a send-back shape into a raw patch that actually
/// carries `findings` (e.g. the `done: true` + findings shape needs no
/// normalizing here since findings was already in the raw patch; see that
/// function's doc for the shapes it rewrites). Grounding runs on whatever
/// `findings` array is in the raw patch by that point, regardless of which
/// path put it there.
///
/// Grounds each finding against the *merged* (post-patch) workspace, not
/// just `cur` -- the same file this step's own patch just added to `files`
/// (e.g. a tool-call step that both notes a file and cites it, in the same
/// patch) is visible to the check, matching how `review_sendback_note`/
/// `review_blind_approval_runs_test` already read `merged_workspace` rather
/// than `cur` alone. A patch that fails to deserialize/apply is left alone
/// (`Ok(None)`): it falls straight through to `validate_default`, which
/// reports the real error.
fn review_grounded_findings(
    grounded_findings_enabled: bool,
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Result<Option<String>, PatchError> {
    if !grounded_findings_enabled {
        return Ok(None);
    }

    let Some(sent) = out.patch().0.get("findings") else {
        return Ok(None);
    };
    let Some(sent) = sent.as_array() else {
        return Ok(None);
    };
    let sent: Vec<String> = sent
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if sent.is_empty() {
        return Ok(None);
    }

    let Some(merged) = merged_workspace(cur, out) else {
        return Ok(None);
    };

    let mut grounded = Vec::with_capacity(sent.len());
    let mut dropped = Vec::new();
    for finding in &sent {
        if finding_is_grounded(&merged, finding) {
            grounded.push(finding.clone());
        } else {
            dropped.push(finding.clone());
        }
    }

    if dropped.is_empty() {
        return Ok(None);
    }

    if grounded.is_empty() {
        return Err(PatchError::RuleViolation(format!(
            "Dropped {} finding{} with no file:line citation. Every finding must name the line \
             it is about.",
            dropped.len(),
            if dropped.len() == 1 { "" } else { "s" },
        )));
    }

    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.insert(
            "findings".to_string(),
            serde_json::Value::Array(
                grounded
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }

    let named: Vec<String> = dropped
        .iter()
        .map(|f| format!("\"{}\"", f.chars().take(60).collect::<String>()))
        .collect();
    Ok(Some(format!(
        "Dropped {} finding{} with no file:line citation. Every finding must name the line it \
         is about. Dropped: {}.",
        dropped.len(),
        if dropped.len() == 1 { "" } else { "s" },
        named.join("; "),
    )))
}

/// Case (b) alone, shared by `plan`/`implement`/`review`: `ask_user` with an
/// empty/whitespace `message`. See `premature_done_note`'s doc.
///
/// Also strips `next_skill` from the raw patch when it fires, the same as
/// `premature_done_note`'s case (a) and `review_looked_note`: the caller
/// converts the action to `Action::Continue`, and `core/src/session.rs`'s
/// step 7.5 rejects any step whose patch changes the handoff target unless
/// the action is `Done` -- a stray `next_skill` left in place (e.g. the
/// model sent both `ask_user` and `next_skill` in the same step, having read
/// the instructions' "set next_skill and send done") would make step 7.5
/// reject the now-`Continue` step instead of converting it.
///
/// Falls through to `ask_user_statement_note` when the message is
/// non-empty, covering case (b'): a non-empty `ask_user` that is not
/// actually a question either.
fn ask_user_empty_note(out: &mut statie_core::ParsedOutput<CodingWorkspace>) -> Option<String> {
    if let statie_core::Action::AskUser { message } = out.action()
        && message.trim().is_empty()
    {
        if let Some(obj) = out.patch_mut().0.as_object_mut() {
            obj.remove("next_skill");
        }
        return Some(
            "`ask_user` had no question, so nothing was asked and your patch was kept. If \
             you are blocked, put the question in `message`; otherwise keep working with a \
             tool call."
                .to_string(),
        );
    }
    ask_user_statement_note(out)
}

/// Case (b'), sibling of case (b) above and shared by the same three
/// callers (`implement` via `premature_done_note`'s fallthrough; `plan`/
/// `review` directly, right after `ask_user_empty_note`): `ask_user` with a
/// non-empty `message` that is nonetheless not a question -- 14 of 15
/// parked sessions across 48 live runs with a small local model were
/// `ask_user` carrying a statement of intent ("I need to fix the unbalanced
/// braces in `tui/src/headless.rs` before I can run the tests. I will first
/// read the file...") rather than an actual question, stalling the run on
/// a human with nothing to answer.
///
/// The check is deliberately just "does `message` contain a `?` anywhere",
/// not a grammar check: a real question missing its `?` (e.g. "Should I
/// keep both arms") is also converted, since the harness cannot tell intent
/// from phrasing alone and the model can always add the `?`.
///
/// Same mechanics as `ask_user_empty_note`: patch kept, `next_skill`
/// stripped from the raw patch so the now-`Continue` step is not rejected
/// by `core/src/session.rs`'s step 7.5.
fn ask_user_statement_note(out: &mut statie_core::ParsedOutput<CodingWorkspace>) -> Option<String> {
    if let statie_core::Action::AskUser { message } = out.action()
        && !message.contains('?')
    {
        if let Some(obj) = out.patch_mut().0.as_object_mut() {
            obj.remove("next_skill");
        }
        return Some(
            "`ask_user` carried a statement of what you will do next, not a question; \
             nothing was asked and your patch was kept. Do it with a tool call. Ask the \
             user only for something you cannot find out yourself, and end the question \
             with `?`."
                .to_string(),
        );
    }
    None
}

/// Plan-step regression guard shared by `plan`/`implement`/`review`: once a
/// step is committed `Done`, it stays `Done` or moves on to `Skipped` --
/// never back to `Pending`/`InProgress`. Observed three times in live runs
/// with a local model: after a resume, the model moves a finished step back
/// to `pending`/`in_progress` ("let's start by running the test command as
/// requested in the plan"), then redoes finished work or loops on step 1.
///
/// Only steps present (by `id`) in both `cur.plan` and `next.plan` are
/// checked -- removing a step or adding a new one is unaffected. Reports
/// only the first offending step, in `next.plan` order, and points at the
/// first still-open step (also in `next.plan` order) that is not itself one
/// of the reopened steps in this same patch, so the redirect never sends the
/// model back to the very step it just illegally reopened.
///
/// `enabled` is `guards.done_stays_done` (statie-spec.md "10. After it
/// works: the rule audit"; defaults to `false` as of the first bench
/// battery's audit -- the switch off was shown no worse, since the bench's
/// fourth miss was this rule rejecting the model twelve times over for
/// moving its "run cargo test" plan step back to in-progress after an edit,
/// until the retry cap parked it as a failure with the crate already
/// green); when `false`, the whole rule is a no-op and reopening a done
/// step is allowed. The individual `PlanSkill`/`ImplementSkill`/
/// `ReviewSkill` constructors (`::new()`/`Default`) still default their own
/// `done_stays_done` field to `true` -- only the config-facing defaults
/// (`Guards::default()`, `coding_skills()`/`coding_skills_with_delegates()`,
/// and `CodingRegistry::with_guards` with no configured `guards` section)
/// now thread `false` through.
fn plan_rules(
    cur: &CodingWorkspace,
    next: &CodingWorkspace,
    enabled: bool,
) -> Result<(), PatchError> {
    if !enabled {
        return Ok(());
    }
    let mut reopened_ids = std::collections::HashSet::new();
    for next_step in &next.plan {
        if matches!(
            next_step.status,
            StepStatus::Pending | StepStatus::InProgress
        ) && let Some(cur_step) = cur.plan.iter().find(|s| s.id == next_step.id)
            && matches!(cur_step.status, StepStatus::Done)
        {
            reopened_ids.insert(next_step.id);
        }
    }

    let Some(offending_id) = next
        .plan
        .iter()
        .find(|s| reopened_ids.contains(&s.id))
        .map(|s| s.id)
    else {
        return Ok(());
    };
    let offending_text = next
        .plan
        .iter()
        .find(|s| s.id == offending_id)
        .map(|s| s.text.as_str())
        .unwrap_or("");

    let next_open = next.plan.iter().find(|s| {
        matches!(s.status, StepStatus::Pending | StepStatus::InProgress)
            && !reopened_ids.contains(&s.id)
    });

    let redirect = match next_open {
        Some(step) => format!(
            "Work on the first open step: step {} ({}) -- or add a new step for new work",
            step.id, step.text
        ),
        None => "All steps are done: run the tests and hand off to review".to_string(),
    };

    Err(PatchError::RuleViolation(format!(
        "plan step {offending_id} is done and stays done ({offending_text}). {redirect}"
    )))
}

/// `implement`-only: `done: true` is never `implement`'s to set -- only
/// `review`'s approval (empty `findings`) finishes the task
/// (statie-spec.md §5/§10; `ImplementSkill::is_terminal` is unconditionally
/// `false`, mirroring `PlanSkill::is_terminal`/`RouterSkill::is_terminal`).
/// Unlike `plan`/`router`'s shared `done_not_ours_note`, this does NOT
/// decide the action -- `implement`'s existing chain (`green_done_note` /
/// `implement_bare_done` / `implement_no_green_runs_test` /
/// `premature_done_note`) already makes that decision, and does it better
/// than a plain "not yours" `Continue` would: with a passing test and a
/// complete plan those conversions hand off to review; without a passing
/// test they substitute a real `test` call; with an open step and nothing
/// new to learn from re-running, they attach the open-step note. Folding a
/// bare-`done` short-circuit in here (the way `done_not_ours_note` does for
/// `plan`/`router`, which have no such chain) would pre-empt all of that.
///
/// So this only ever strips `done` from the raw patch (when the *merged*,
/// i.e. post-patch, workspace would read `done: true`) and returns a note;
/// it never touches `out.action()`. Called in `ImplementSkill::validate`
/// FIRST, before `green_done_note`/`implement_bare_done`/
/// `implement_no_green_runs_test`/`premature_done_note` run, so every one of
/// those sees a raw patch (and therefore a merged workspace) with no `done`
/// key at all -- exactly as if the model had never sent it. Each of those
/// functions already has its own `if merged.done { return None; }` guard (or
/// equivalent), so this also ensures none of them short-circuits on a
/// `done: true` that was never review's to commit in the first place.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to the chain/`validate_default`, which report the real
/// error.
fn implement_done_not_ours_strip(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    let merged = merged_workspace(cur, out)?;
    if !merged.done {
        return None;
    }
    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
    }
    Some(
        "`done: true` is review's to set; implement hands off with `next_skill: \"review\"` \
         once verification passes."
            .to_string(),
    )
}

/// `plan`-only: `done: true` is never `plan`'s to set -- only `review`'s
/// approval (empty `findings`) finishes the task. Thin wrapper around
/// `done_not_ours_note` (see its doc for the full case analysis and the
/// bench run that motivated it) with `plan`'s note text. Called from
/// `PlanSkill::validate` AFTER the existing bare-`done`/empty-plan
/// handling, so that logic's own note wins for an empty plan (its `done`
/// is already stripped there, so this becomes a no-op for that step: the
/// merged workspace it recomputes reads `done: false`).
fn plan_done_not_ours_note(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    done_not_ours_note(
        cur,
        out,
        "`done: true` is not the plan skill's to set; the task is finished by review. \
         Write the plan and hand off to implement with `next_skill`.",
    )
}

/// `router`-only sibling of `plan_done_not_ours_note`: router has no
/// plan-emptiness case of its own (it never writes `plan`), so this is
/// exactly the shared logic, no wrapping needed. See
/// `RouterSkill::validate` for where this is wired in and
/// `plan_done_not_ours_note`'s doc for the full case analysis (both
/// functions behave identically -- strip `done`, convert a bare `done` to
/// `Continue`, leave a named handoff alone -- only the note text differs).
fn router_done_not_ours_note(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
) -> Option<String> {
    done_not_ours_note(
        cur,
        out,
        "`done: true` is not the router's to set; the task is finished by review. Decide the \
         next skill and hand off with `next_skill`.",
    )
}

/// Shared by `plan_done_not_ours_note`/`router_done_not_ours_note`: neither
/// `plan` nor `router` can finish the task (`common_is_terminal`'s `done`
/// flag is `review`-only, statie-spec.md §5/§10; `PlanSkill::is_terminal`
/// and `RouterSkill::is_terminal` are both unconditionally `false`, so a
/// `done: true` that slipped past this would not even end the session --
/// this still strips it so the workspace itself never carries a lie). A
/// bench run showed the `plan` case of this
/// (`{"patch": {"done": true}, "action": {"type": "done"}}` sent as
/// `plan`'s very first step, empty plan) commit `done: true`: the
/// empty-plan conversion that already existed only stripped `next_skill`,
/// not `done`, so the workspace became terminal and the session ended
/// after three steps with no edit and no test -- a false completion. This
/// function is the general fix for the underlying hole, for both skills
/// that share it: whenever the *merged* (post-patch) workspace would be
/// `done: true`, `done` is stripped from the raw patch before it can
/// commit, regardless of what else the patch carries or what action the
/// model sent ("with any action" -- a model could in principle attach
/// `done: true` to a tool call or an `ask_user` too, not just `done`).
///
/// Two cases, both leaving the rest of the patch (including `next_skill`
/// when present) alone:
///
/// - The action is `done` AND the raw patch (pre-strip) carries no
///   `next_skill`: nothing else about this step is a real handoff, so the
///   whole step converts to the harness-internal `Action::Continue`
///   (`core/src/model.rs`) with `note` -- the same "commit the patch,
///   spend no retry, tell the model what to do instead" treatment
///   `premature_done_note` and the empty-plan conversion already use
///   elsewhere in this module.
/// - Otherwise (the action was not `done`, or it was `done` with a
///   `next_skill` already named -- a legitimate handoff): the action is
///   left as the caller set it; `note` is returned for the caller to
///   attach to the step instead of converting it. A `done` +
///   `next_skill: "implement"` step still hands off; it just does so
///   without ever committing `done: true` into the shared workspace.
///
/// A patch that fails to deserialize/apply is left alone (`None`): it
/// falls straight through to `validate_default`, which reports the real
/// error.
fn done_not_ours_note(
    cur: &CodingWorkspace,
    out: &mut statie_core::ParsedOutput<CodingWorkspace>,
    note: &str,
) -> Option<String> {
    let merged = merged_workspace(cur, out)?;
    if !merged.done {
        return None;
    }

    let had_next_skill = out.patch().0.get("next_skill").is_some();
    if let Some(obj) = out.patch_mut().0.as_object_mut() {
        obj.remove("done");
    }

    if matches!(out.action(), statie_core::Action::Done) && !had_next_skill {
        out.set_action(statie_core::Action::Continue {
            note: note.to_string(),
        });
    }

    Some(note.to_string())
}

/// `absorb_report` shared by every coding skill: insert the finished child's
/// `Report` under its own agent name, overwriting any earlier report for
/// that same child. The workspace is otherwise untouched -- the model reads
/// the report from `reports[<name>]` on its next step.
fn common_absorb_report(ws: &mut CodingWorkspace, report: Report) -> Result<(), PatchError> {
    ws.reports.insert(report.agent.clone(), report);
    Ok(())
}

/// The router skill: decides which coding skill should run next and does
/// nothing else. Has no tools -- routing is a pure decision over the
/// workspace and inbox.
#[derive(Default)]
pub struct RouterSkill {
    spec: OnceSpec,
}

/// The plan skill: explores the repo and writes the plan.
pub struct PlanSkill {
    spec: OnceSpec,
    delegates: Vec<String>,
    /// `guards.done_stays_done` (statie-spec.md "10. After it works: the
    /// rule audit"); defaults to `true` (today's behaviour). Set with
    /// [`PlanSkill::with_done_stays_done`].
    done_stays_done: bool,
    /// `guards.seen_map` (`core/src/guards.rs`; BACKLOG.md item 2); defaults
    /// to `true` (today's behaviour). Set with [`PlanSkill::state_fields`].
    seen: bool,
    /// `guards.decision_field`; see [`PlanSkill::seen`]. Set with
    /// [`PlanSkill::state_fields`].
    decision: bool,
    /// `guards.tests_field` (BACKLOG.md item 5); see [`PlanSkill::seen`].
    /// Set with [`PlanSkill::state_fields`].
    tests: bool,
    /// `thinker.listed` (`cli/src/config.rs`); defaults to `true` (today's
    /// behaviour). `false` omits `think` from [`PlanSkill::tools`] without
    /// disabling it -- the harness's own substitutions still construct a
    /// `think` action directly. Set with [`PlanSkill::with_listed`].
    listed: bool,
    /// `guards.situational_rules` (`core/src/guards.rs`; unit 26). Defaults
    /// to `false` (today's behaviour: the full rules block every step).
    /// Set with [`PlanSkill::with_situational_rules`].
    situational: bool,
}

impl Default for PlanSkill {
    fn default() -> Self {
        PlanSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            situational: false,
        }
    }
}

/// The implement skill: works the plan, one step at a time.
pub struct ImplementSkill {
    spec: OnceSpec,
    delegates: Vec<String>,
    /// `guards.done_stays_done`; see [`PlanSkill::done_stays_done`].
    done_stays_done: bool,
    /// `guards.seen_map`; see [`PlanSkill::seen`].
    seen: bool,
    /// `guards.decision_field`; see [`PlanSkill::seen`].
    decision: bool,
    /// `guards.tests_field` (BACKLOG.md item 5); see [`PlanSkill::seen`].
    tests: bool,
    /// `thinker.listed`; see [`PlanSkill::listed`]. Set with
    /// [`ImplementSkill::with_listed`].
    listed: bool,
    /// `guards.situational_rules`; see [`PlanSkill::situational`]. Set with
    /// [`ImplementSkill::with_situational_rules`].
    situational: bool,
}

impl Default for ImplementSkill {
    fn default() -> Self {
        ImplementSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            situational: false,
        }
    }
}

/// The review skill: re-checks the changed files and either approves or
/// sends work back to `implement`.
pub struct ReviewSkill {
    spec: OnceSpec,
    delegates: Vec<String>,
    /// `guards.done_stays_done`; see [`PlanSkill::done_stays_done`].
    done_stays_done: bool,
    /// `guards.seen_map`; see [`PlanSkill::seen`].
    seen: bool,
    /// `guards.decision_field`; see [`PlanSkill::seen`].
    decision: bool,
    /// `guards.tests_field` (BACKLOG.md item 5); see [`PlanSkill::seen`].
    tests: bool,
    /// `thinker.listed`; see [`PlanSkill::listed`]. Set with
    /// [`ReviewSkill::with_listed`].
    listed: bool,
    /// `guards.grounded_findings` (`core/src/guards.rs`; item 12: grounded
    /// review findings). Set with [`ReviewSkill::with_grounded_findings`].
    grounded_findings: bool,
    /// `guards.situational_rules`; see [`PlanSkill::situational`]. Set with
    /// [`ReviewSkill::with_situational_rules`]. `review`'s instructions had
    /// no "after X" bullet against unit 26's node set, so this switch only
    /// affects `spec()`'s doc note and `situational()`'s (empty) return --
    /// see `ReviewSkill::spec`.
    situational: bool,
}

impl Default for ReviewSkill {
    fn default() -> Self {
        ReviewSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            grounded_findings: true,
            situational: false,
        }
    }
}

/// Appended to the plan/implement/review instructions when the skill has
/// one or more configured delegates (never to router's -- it does not
/// delegate). Empty `delegates` means no paragraph is appended, so
/// instruction text with no configured delegates stays byte-identical to
/// before this field existed.
fn delegate_paragraph(delegates: &[String]) -> String {
    if delegates.is_empty() {
        return String::new();
    }
    format!(
        "\n\nSubagents: you may hand a self-contained job to one of these configured \
         subagents with action `delegate` (`skill`, `task`): {}. Write a task that stands \
         alone (the child sees none of your state). The result arrives as your next \
         observation and is stored in `reports[<name>]` (summary, findings, files, data); \
         read it there, then continue.",
        delegates.join(", ")
    )
}

/// Tiny helper so each skill can hand out a stable `Arc<SkillSpec>` from
/// `spec()` without recreating it on every call, while `new()` stays plain
/// (no fallible construction).
struct OnceSpec(std::sync::OnceLock<Arc<SkillSpec>>);

impl Default for OnceSpec {
    fn default() -> Self {
        OnceSpec(std::sync::OnceLock::new())
    }
}

impl OnceSpec {
    fn get(&self, name: &str, instructions: String) -> Arc<SkillSpec> {
        Arc::clone(
            self.0
                .get_or_init(|| Arc::new(SkillSpec::new(name, instructions))),
        )
    }
}

impl RouterSkill {
    pub fn new() -> Self {
        RouterSkill::default()
    }
}

impl Skill for RouterSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "router"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.spec.get(
            "router",
            "You are the router for a coding task. Your only job is to decide which \
             skill runs next and then stop -- you do not explore files or write code.\n\n\
             What to put in the workspace:\n\
             - If `task` is empty, copy the user's request from the inbox into `task` \
               verbatim (do not paraphrase or trim it).\n\
             - Decide the next skill: if `plan` is empty, hand off to `plan`; if `plan` \
               has any step that is not `done`/`skipped`, hand off to `implement`; if every \
               step is `done` or `skipped` and `findings` is empty, hand off to `review`.\n\
             - Set `next_skill` to that choice.\n\n\
             When to hand off: always, in a single step. Ack every inbox item you read, \
             then set action `done`. Do not leave the router activation without a \
             `next_skill` set. The router cannot ask the user anything; if the task is \
             unclear, hand off to `plan`, which can.\n\n\
             History is searchable with the `recall` tool, but the router has no tools \
             and should not need it. `recent_summary` is the only memory that survives \
             across steps and across skill handoffs -- keep it current if you touch it."
                .to_string(),
        )
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec![
            "plan".to_string(),
            "implement".to_string(),
            "review".to_string(),
        ]
    }

    fn skill_with_tool(&self, tool: &str) -> Option<String> {
        let s = coding_skill_with_tool(tool)?;
        self.allowed_transitions()
            .iter()
            .any(|t| t == s)
            .then(|| s.to_string())
    }

    /// `router` can never finish the task on its own, the same as `plan`
    /// (see `PlanSkill::is_terminal`'s doc) -- only `review`'s approval
    /// does that. Unconditionally `false`: router only ever hands off via
    /// `next_skill`. See `router_done_not_ours_note` for the
    /// `validate`-side half.
    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn allows_yield(&self) -> bool {
        false
    }

    /// The router cannot talk to the user -- see the spec's last sentence
    /// in the "When to hand off" paragraph, added alongside this override.
    /// A task that needs clarification is handed off to `plan`, which can
    /// `ask_user`.
    fn allows_ask_user(&self) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        common_handoff(self.name(), s)
    }

    fn on_activate(&self, ws: &Self::Workspace) -> Option<Patch> {
        common_on_activate(ws)
    }

    fn rules(
        &self,
        cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        common_rules(self.name(), &["plan", "implement", "review"], cur, next)
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        common_absorb_report(ws, report)
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        common_harness_owned_fields()
    }

    fn notes(&self, ws: &Self::Workspace) -> Option<String> {
        common_notes(ws)
    }

    /// `router` gets the same `done: true`-stripping treatment as `plan`
    /// (`plan_done_not_ours_note`): router has no plan-emptiness case of
    /// its own (it never writes `plan`), so this is the plain form --
    /// whenever the merged workspace would be `done: true`, strip it from
    /// the raw patch, and convert a bare `done` (no `next_skill`) to
    /// `Action::Continue` with the note; a `done` that already names a
    /// `next_skill` keeps handing off, just without ever committing
    /// `done: true`.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let mut notes = strip_harness_owned(&self.harness_owned_fields(), &mut out);
        let note = strip_malformed_next(&cur.agent, &mut out);
        let not_ours_note = router_done_not_ours_note(&cur.agent, &mut out);
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        notes.extend(note);
        notes.extend(not_ours_note);
        Ok(if notes.is_empty() {
            step
        } else {
            step.with_notes(notes)
        })
    }
}

impl PlanSkill {
    pub fn new() -> Self {
        PlanSkill::default()
    }

    /// Builds a `plan` skill configured with the given subagent names as
    /// its allowed delegates. Appends the "Subagents:" paragraph to its
    /// instructions when `delegates` is non-empty.
    pub fn with_delegates(delegates: Vec<String>) -> Self {
        PlanSkill {
            spec: OnceSpec::default(),
            delegates,
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            situational: false,
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
        self
    }

    /// Sets `guards.situational_rules` (default `false`; `core/src/
    /// guards.rs`, unit 26: situational rules). `true`: `spec()` renders
    /// only the universal bullets and `situational()` returns
    /// `procedure::render(procedure::situation(cur))` as the prompt's
    /// `## Now` section. `false` (default): `spec()` is byte-identical to
    /// before this switch existed, and `situational()` returns `None` --
    /// no `## Now` section.
    pub fn with_situational_rules(mut self, enabled: bool) -> Self {
        self.situational = enabled;
        self
    }

    /// Sets `thinker.listed` (default `true`). `false` omits `think` from
    /// [`PlanSkill::tools`] -- the model can no longer choose it on its own
    /// -- without disabling `think` itself: the harness's own
    /// substitutions (`implement_repeat_failure_thinks`'s repeated-failure
    /// trigger, the early-park trigger) still construct `Action::Tool {
    /// name: "think", .. }` directly and are unaffected.
    pub fn with_listed(mut self, listed: bool) -> Self {
        self.listed = listed;
        self
    }

    /// Sets `guards.seen_map`/`guards.decision_field`/`guards.tests_field`
    /// (`core/src/guards.rs`; all default `true`; BACKLOG.md items 2 and
    /// 5). `seen = false`: `CodingWorkspace::seen` is never written by the
    /// harness (stays empty). `decision = false`: `CodingWorkspace::
    /// decision` is never written or cleared by the harness (the `think`
    /// tool still runs and its answer still reaches the model as that
    /// call's own observation). `tests = false`: `CodingWorkspace::tests`
    /// is never written by the harness (stays `null`). See
    /// `common_after_tool`'s doc for the exact per-branch effect.
    pub fn state_fields(mut self, seen: bool, decision: bool, tests: bool) -> Self {
        self.seen = seen;
        self.decision = decision;
        self.tests = tests;
        self
    }
}

/// The note `PlanSkill::validate` attaches when it converts an `ask_user`
/// into the plan-is-written hand-off (unit 30, "a question asked after the
/// plan is written is the hand-off"): followed by the first 200 characters
/// (chars, not bytes) of the `ask_user` message, plus `…` if that truncated
/// it.
pub const PLAN_ASK_HANDOFF_NOTE_PREFIX: &str = "The plan is written, so the harness handed off \
     to `implement` instead of asking. The plan answers the question; `implement` has the \
     editing tools. The question was: ";

impl Skill for PlanSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "plan"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        // Unit 26 (situational rules): with the switch on, the "design
        // fork" bullet moves to `procedure.rs`'s `start` edge
        // (`START_THINK_OPTIONS`) and `situational()` renders it instead --
        // left out of the universal spec text entirely, not duplicated.
        // With the switch off, `head` + `tail` concatenate to exactly the
        // instructions string this skill has always had (byte-identical;
        // see `skills/tests/it/situational.rs`'s snapshot test).
        let head = "You are the planner for a coding task. Use `search` to find definitions and \
             usages before reading whole files; use `list_files` and `read_file` \
             (with `offset`/`limit` for files longer than ~200 lines) until you \
             understand what needs to change. Do not write or run code yet.\n\n\
             What to put in the workspace:\n\
             - Record every file you judge relevant in `files`, each with a one-line \
               note explaining why it matters.\n\
             - Write `plan` as a numbered list of small, concrete steps; each step's \
               `text` should be at most one sentence and independently checkable. Give \
               each step a unique `id`. If the repo has tests, make the first step \
               \"run `test` to record the baseline\" and the last step \"run `test` and \
               confirm it passes\".\n\
             - Keep `recent_summary` current as you explore.\n\
             - `notes` is kept by the harness from your previous reasoning: read it \
               before deciding; you cannot patch it.\n\
             - If you commit to a next step by setting `next`, the harness holds you to \
               it -- change `next` (and say why in `recent_summary`) if the plan changes.\n";
        let think_options_bullet = "- For a design fork with more than one reasonable approach, call \
             `think` with `mode: \"options\"` before writing the plan.\n";
        let tail = "\n\
             When to hand off: once the plan is solid, set `next_skill: \"implement\"` \
             and action `done`. The plan must be non-empty before you hand off to \
             `implement`.\n\n\
             History is searchable with the `recall` tool if you need to check what an \
             earlier step already found. `recent_summary` is the only memory that \
             survives across steps and across skill handoffs. If a tool output overflowed \
             (the observation says so), call `filter_last_tool_call` with a regex instead \
             of re-running the command.";
        let mut instructions = String::new();
        instructions.push_str(head);
        if !self.situational {
            instructions.push_str(think_options_bullet);
        }
        instructions.push_str(tail);
        instructions.push_str(&delegate_paragraph(&self.delegates));
        self.spec.get("plan", instructions)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        let mut tools = vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
            "test".to_string(),
        ];
        if self.listed {
            tools.push("think".to_string());
        }
        tools
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec!["implement".to_string(), "router".to_string()]
    }

    fn skill_with_tool(&self, tool: &str) -> Option<String> {
        let s = coding_skill_with_tool(tool)?;
        self.allowed_transitions()
            .iter()
            .any(|t| t == s)
            .then(|| s.to_string())
    }

    /// `plan` can never finish the task on its own -- only `review`'s
    /// approval (empty `findings`) does that (`common_is_terminal`,
    /// `ReviewSkill::is_terminal`). Unconditionally `false`, even when
    /// `ws.done` is somehow `true`: `plan`'s job is to write the plan and
    /// hand off to `implement` with `next_skill`, never to end the session
    /// itself. See `plan_done_not_ours_note` for the `validate`-side half
    /// of this: it strips a `done: true` a model sends from `plan`'s patch
    /// before it can commit, so `ws.done` should never actually reach
    /// `true` here in practice -- this override is the second, independent
    /// line of defense.
    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn allows_yield(&self) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        common_handoff(self.name(), s)
    }

    fn on_activate(&self, ws: &Self::Workspace) -> Option<Patch> {
        common_on_activate(ws)
    }

    fn rules(
        &self,
        cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        common_rules(self.name(), &["implement", "router"], cur, next)?;
        if next.next_skill.as_deref() == Some("implement") && next.plan.is_empty() {
            return Err(PatchError::RuleViolation(
                "plan must be non-empty before handing off to implement".to_string(),
            ));
        }
        plan_rules(&cur.agent, next, self.done_stays_done)
    }

    fn allowed_delegates(&self) -> Vec<String> {
        self.delegates.clone()
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        common_absorb_report(ws, report)
    }

    fn commitment(&self, ws: &Self::Workspace) -> Option<Commitment> {
        common_commitment(ws)
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        common_harness_owned_fields()
    }

    fn notes(&self, ws: &Self::Workspace) -> Option<String> {
        common_notes(ws)
    }

    fn situational(&self, cur: &CommittedState<Self::Workspace>) -> Option<String> {
        common_situational(self.situational, cur)
    }

    fn after_tool(
        &self,
        ws: &Self::Workspace,
        _harness: &HarnessState,
        step: u64,
        name: &str,
        args: &serde_json::Value,
        output: &str,
    ) -> Option<Patch> {
        common_after_tool(
            ws,
            step,
            name,
            args,
            output,
            self.seen,
            self.decision,
            self.tests,
        )
    }

    /// A bare `done` (no `next_skill` set) in `plan` means "the plan is
    /// solid, go implement it": if the model sends `done` with `next_skill`
    /// left unset and the merged plan is non-empty, inject
    /// `next_skill: "implement"` into the raw patch before running the
    /// default validation, rather than rejecting with the "set `next_skill`"
    /// message forever -- a small local model has been observed to write a
    /// full plan, then repeat exactly this premature `done` (reasoning
    /// "I will now proceed to implement") without ever adding the field the
    /// rejection asked for.
    ///
    /// Right after `ask_user_empty_note` and before this `is_done` block
    /// (unit 30, "a question asked after the plan is written is the
    /// hand-off"): a model whose plan is already written sometimes asks
    /// permission to move on instead of just handing off ("Should I hand
    /// off to the implement skill now?") -- an `ask_user` the harness can
    /// answer itself, since the plan already answers it. If `out.action()`
    /// is `Action::AskUser { message }` and the merged plan (via
    /// `merged_plan_nonempty`, the same helper the `is_done` block below
    /// uses) is non-empty, the step is converted to the intended hand-off
    /// instead of parking: inject `next_skill: "implement"` into the raw
    /// patch when the patch does not already name one (an explicit
    /// `next_skill` already in the patch, e.g. `"review"`, is left alone),
    /// set the action to `Action::Done`, and attach
    /// `PLAN_ASK_HANDOFF_NOTE_PREFIX` followed by the first 200 characters
    /// of `message` (plus `…` if truncated) as the step's note. An
    /// `ask_user` with an EMPTY merged plan is left untouched here -- it
    /// parks as `Park::AskUser`, same as always.
    ///
    /// An EMPTY merged plan is handled the same way `premature_done_note`
    /// handles a premature `done` in `implement`: `done` (with or without
    /// `next_skill` -- a model sending both together, having read the
    /// instructions' "set next_skill and send done", was observed twice at
    /// the start of a live run) is converted to the harness-internal
    /// `Action::Continue` with a note explaining what to do, `next_skill` is
    /// stripped from the raw patch so no handoff lands, and the rest of the
    /// patch is kept -- rather than rejecting the whole step (discarding any
    /// real progress it carried and spending a retry) with the
    /// non-empty-plan rule below, which used to fire here too.
    ///
    /// Before any of that, strips every harness-owned key present in the
    /// raw patch (see `strip_harness_owned`) so a model patch touching
    /// `notes`/`reports`/`seen` commits the rest of the patch instead of
    /// being rejected outright. Also strips a malformed `next` (see
    /// `strip_malformed_next`) before `validate_default` runs, attaching the
    /// note it returns to the resulting `ValidatedStep`; runs
    /// `shell_read_substitution` (BACKLOG.md item 16), replacing a `shell`
    /// action whose `cmd` is a pure single-file read with the equivalent
    /// `read_file` call (see `ImplementSkill::validate`'s doc for why); runs
    /// `write_file_auto_confirm` (BACKLOG.md item 15, harmless here since
    /// `plan` has no `write_file` tool -- see `ImplementSkill::validate`'s
    /// doc); and converts an
    /// empty `ask_user` into the harness-internal `Action::Continue` instead
    /// of rejecting the step (case (b) of `premature_done_note`'s doc;
    /// `plan` does not get case (a) -- its bare-`done` handling above
    /// already covers `done`).
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let mut notes = strip_harness_owned(&self.harness_owned_fields(), &mut out);
        let note = strip_malformed_next(&cur.agent, &mut out);
        let shell_read_note = shell_read_substitution(&mut out);
        write_file_auto_confirm(cur, &mut out);

        if let Some(note) = ask_user_empty_note(&mut out) {
            out.set_action(statie_core::Action::Continue { note });
        }

        // Unit 30, "a question asked after the plan is written is the
        // hand-off": an `ask_user` whose message is a real question (the
        // empty/statement cases above already converted anything else) but
        // whose merged plan is already non-empty gets converted straight to
        // the intended hand-off instead of parking -- the plan answers the
        // question, and `implement` is the skill with the editing tools.
        // An empty merged plan is left alone here: it parks as
        // `Park::AskUser`, same as always.
        if let statie_core::Action::AskUser { message } = out.action()
            && merged_plan_nonempty(&cur.agent, &out) == Some(true)
        {
            let message = message.clone();
            if out.patch().0.get("next_skill").is_none()
                && let Some(obj) = out.patch_mut().0.as_object_mut()
            {
                obj.insert(
                    "next_skill".to_string(),
                    serde_json::Value::String("implement".to_string()),
                );
            }
            out.set_action(statie_core::Action::Done);
            let mut quoted: String = message.chars().take(200).collect();
            if message.chars().count() > 200 {
                quoted.push('…');
            }
            notes.push(format!("{PLAN_ASK_HANDOFF_NOTE_PREFIX}{quoted}"));
        }

        let is_done = matches!(out.action(), statie_core::Action::Done);
        if is_done {
            // Apply the patch strategy the same way `validate_default` is
            // about to, just to peek at whether the resulting plan is
            // non-empty. A patch that fails to deserialize/apply is left
            // alone here -- it falls straight through to
            // `validate_default` below, which reports the real error.
            let merged_plan_nonempty = merged_plan_nonempty(&cur.agent, &out);

            match merged_plan_nonempty {
                Some(true)
                    if out.patch().0.get("next_skill").is_none()
                        && out.patch().0.get("done").is_none() =>
                {
                    // Inject the missing key into the raw merge patch; the
                    // Patch log entry will simply show this key alongside
                    // whatever else the model sent. Guarded on the raw patch
                    // ALSO carrying no `done` field: a model that explicitly
                    // sent `done: true` (rather than just the bare `done`
                    // action with nothing in the patch about it) gets
                    // `plan_done_not_ours_note`'s treatment below instead --
                    // `done: true` is never `plan`'s to set, so that shape
                    // must not silently become an auto-handoff.
                    if let Some(obj) = out.patch_mut().0.as_object_mut() {
                        obj.insert(
                            "next_skill".to_string(),
                            serde_json::Value::String("implement".to_string()),
                        );
                    }
                }
                Some(false) => {
                    let continue_note = "the plan is empty. Write it first: `plan` is a list of \
                                          {id, status, text} steps ending with `test`; then send \
                                          `done` with `next_skill: \"implement\"`. Your patch was \
                                          kept."
                        .to_string();
                    if let Some(obj) = out.patch_mut().0.as_object_mut() {
                        obj.remove("done");
                        obj.remove("next_skill");
                    }
                    out.set_action(statie_core::Action::Continue {
                        note: continue_note,
                    });
                }
                _ => {}
            }
        }
        // `done: true` is never `plan`'s to set, with any action (see
        // `plan_done_not_ours_note`'s doc) -- so this runs unconditionally,
        // not just when the action is `done`. Runs AFTER the empty-plan
        // handling above so that case's own note wins for an empty plan
        // (its `done` is already stripped there, hence this becomes a
        // no-op for that step: the merged workspace it recomputes now
        // reads `done: false`). For every other shape that still carries
        // `done: true` -- most notably a non-empty plan with no
        // `next_skill`, the `Some(true)` arm above deliberately leaves
        // alone -- this strips `done` and attaches its own note.
        let not_ours_note = plan_done_not_ours_note(&cur.agent, &mut out);
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        notes.extend(note);
        notes.extend(shell_read_note);
        notes.extend(not_ours_note);
        Ok(if notes.is_empty() {
            step
        } else {
            step.with_notes(notes)
        })
    }
}

impl ImplementSkill {
    pub fn new() -> Self {
        ImplementSkill::default()
    }

    /// Builds an `implement` skill configured with the given subagent names
    /// as its allowed delegates. Appends the "Subagents:" paragraph to its
    /// instructions when `delegates` is non-empty.
    pub fn with_delegates(delegates: Vec<String>) -> Self {
        ImplementSkill {
            spec: OnceSpec::default(),
            delegates,
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            situational: false,
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
        self
    }

    /// Sets `thinker.listed`; see [`PlanSkill::with_listed`].
    pub fn with_listed(mut self, listed: bool) -> Self {
        self.listed = listed;
        self
    }

    /// Sets `guards.seen_map`/`guards.decision_field`/`guards.tests_field`;
    /// see [`PlanSkill::state_fields`].
    pub fn state_fields(mut self, seen: bool, decision: bool, tests: bool) -> Self {
        self.seen = seen;
        self.decision = decision;
        self.tests = tests;
        self
    }

    /// Sets `guards.situational_rules`; see
    /// [`PlanSkill::with_situational_rules`].
    pub fn with_situational_rules(mut self, enabled: bool) -> Self {
        self.situational = enabled;
        self
    }
}

impl Skill for ImplementSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "implement"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        // Unit 26 (situational rules): with the switch on, four bullets
        // move to `procedure.rs` edges and `situational()` renders them
        // instead -- left out of the universal spec text entirely, not
        // duplicated:
        //   - the `edit_file`-not-found retry clause -> `edit_error`
        //     (`EDIT_ERROR_RETRY`)
        //   - the "tool reports a line number" clause -> `edit_error`
        //     (`READ_FROM_REPORTED_OFFSET`)
        //   - the after-edit "run `test`" bullet -> `edit_ok`
        //     (`EDIT_OK_RUN_TEST`)
        //   - the `think`-when-stuck bullet -> `test_fail_*`
        //     (`THINK_WHEN_STUCK`)
        //   - the `try_fixes`-after-failure bullet -> `start`/`test_fail_*`
        //     (`TRY_FIXES_AFTER_FAILURE`)
        // With the switch off, `head`/`edit_retry_clause`/`mid`/
        // `line_number_clause`/`mid2`/`run_test_bullet`/`mid3`/
        // `think_bullet`/`try_fixes_bullet`/`tail` concatenate to exactly
        // the instructions string this skill has always had (byte-
        // identical; see `skills/tests/it/situational.rs`'s snapshot test).
        let head = "You are implementing the plan for a coding task, one step at a time.\n\n\
             What to put in the workspace:\n\
             - Mark the step you are working `in_progress`, then `done` once it is \
               finished; use `skipped` only when a step turns out unnecessary.\n\
             - Prefer `edit_file` for changes to existing files: give it a small \
               anchored `old` text copied exactly from a `read_file` result, including \
               indentation. Use `write_file` only for new files or full rewrites. ";
        let edit_retry_clause = "If \
               `edit_file` reports the text was not found or ambiguous, re-read the \
               file and retry with a more specific anchor -- never fall back to \
               rewriting the whole file. ";
        let mid = "To add code, anchor on one line you can see: \
               `edit_file` with `after` (or `before`) and `new`, instead of a \
               multi-line `old`. ";
        let line_number_clause = "When a tool reports a line number, read from that \
               offset (`read_file` with `offset`) instead of paging from the top. ";
        let mid2 = "Use `shell` for other changes.\n\
             - Commit before you look: when you decide what to change, set `next` \
               (tool, path, what) in the same patch, where `tool` is the mutating call \
               you will make -- `edit_file`, `write_file`, `shell`, or `test`, never \
               `read_file`. While `next` is set you may read that file at most twice; \
               then the harness expects the call. If the plan changes, change `next` \
               and say why in `recent_summary`.\n";
        let run_test_bullet = "- After finishing a step that changes code, run `test` and read the \
               `result:` line; do not mark a step `done` while `test` reports `fail` \
               unless the failure predates your change (check with `recall`, e.g. \
               query \"test output\" to see the last test run).\n";
        let mid3 = "- Keep `files` and `recent_summary` up to date as you go.\n\
             - `notes` is kept by the harness from your previous reasoning: read it \
               before deciding; you cannot patch it.\n";
        let think_bullet = "- `think` asks a reasoning subagent that sees what you see; use it when a \
               fix did not change the failure or you cannot tell which cause is real, \
               then act on its `decision`.\n";
        let try_fixes_bullet = "- `try_fixes` is only useful after a failing `test`: it proposes and verifies \
               candidate fixes for the most recent failure, so call `test` first if none has \
               failed yet.\n";
        let tail = "- If you are blocked (missing information only the user can give you), use \
               action `ask_user` rather than guessing -- it is the only way to pause; \
               `yield` is rejected in this skill.\n\n\
             When to hand off: once every plan step is `done` or `skipped`, set \
             `next_skill: \"review\"` and action `done`. Do not hand off to `review` \
             while any step is still `pending` or `in_progress`.\n\n\
             History is searchable with the `recall` tool -- use it to recall what an \
             earlier step already tried or found. `recent_summary` is the only memory \
             that survives across steps and across skill handoffs. If a tool output \
             overflowed (the observation says so), call `filter_last_tool_call` with a \
             regex instead of re-running the command.";
        let mut instructions = String::new();
        instructions.push_str(head);
        if !self.situational {
            instructions.push_str(edit_retry_clause);
        }
        instructions.push_str(mid);
        if !self.situational {
            instructions.push_str(line_number_clause);
        }
        instructions.push_str(mid2);
        if !self.situational {
            instructions.push_str(run_test_bullet);
        }
        instructions.push_str(mid3);
        if !self.situational {
            instructions.push_str(think_bullet);
            instructions.push_str(try_fixes_bullet);
        }
        instructions.push_str(tail);
        instructions.push_str(&delegate_paragraph(&self.delegates));
        self.spec.get("implement", instructions)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        let mut tools = vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "revert_file".to_string(),
            "shell".to_string(),
            "test".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
        ];
        if self.listed {
            tools.push("think".to_string());
        }
        tools.push("try_fixes".to_string());
        tools.push("probe".to_string());
        tools
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec![
            "review".to_string(),
            "plan".to_string(),
            "router".to_string(),
        ]
    }

    fn skill_with_tool(&self, tool: &str) -> Option<String> {
        let s = coding_skill_with_tool(tool)?;
        self.allowed_transitions()
            .iter()
            .any(|t| t == s)
            .then(|| s.to_string())
    }

    /// Only `review`'s approval (empty `findings`) finishes the task
    /// (statie-spec.md §5/§10) -- `implement` only ever hands off, via
    /// `next_skill`, whatever `ws.done` says. Unconditionally `false`,
    /// mirroring `PlanSkill::is_terminal`/`RouterSkill::is_terminal`: a raw
    /// `done: true` that somehow reached the committed workspace (it should
    /// not -- see `implement_done_not_ours_strip`, which strips it from the
    /// patch before it can commit) still could not end the session by
    /// itself. See `implement_done_not_ours_strip` for the `validate`-side
    /// half that keeps `ws.done` itself from ever going true.
    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn allows_yield(&self) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        common_handoff(self.name(), s)
    }

    fn on_activate(&self, ws: &Self::Workspace) -> Option<Patch> {
        common_on_activate(ws)
    }

    fn rules(
        &self,
        cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        common_rules(self.name(), &["review", "plan", "router"], cur, next)?;
        if next.next_skill.as_deref() == Some("review") {
            let unfinished = next
                .plan
                .iter()
                .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress));
            if unfinished {
                return Err(PatchError::RuleViolation(
                    "cannot hand off to review while a plan step is pending or in_progress"
                        .to_string(),
                ));
            }
        }
        plan_rules(&cur.agent, next, self.done_stays_done)
    }

    fn allowed_delegates(&self) -> Vec<String> {
        self.delegates.clone()
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        common_absorb_report(ws, report)
    }

    fn commitment(&self, ws: &Self::Workspace) -> Option<Commitment> {
        common_commitment(ws)
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        common_harness_owned_fields()
    }

    fn notes(&self, ws: &Self::Workspace) -> Option<String> {
        common_notes(ws)
    }

    fn situational(&self, cur: &CommittedState<Self::Workspace>) -> Option<String> {
        common_situational(self.situational, cur)
    }

    fn after_tool(
        &self,
        ws: &Self::Workspace,
        _harness: &HarnessState,
        step: u64,
        name: &str,
        args: &serde_json::Value,
        output: &str,
    ) -> Option<Patch> {
        common_after_tool(
            ws,
            step,
            name,
            args,
            output,
            self.seen,
            self.decision,
            self.tests,
        )
    }

    fn silent_read_cap(&self) -> Option<u32> {
        // Two free looks that write nothing: a read whose patch records a
        // file note, a plan step, or `recent_summary` is never counted, so
        // this only bounds a look that leaves no trace.
        Some(2)
    }

    /// First strips every harness-owned key present in the raw patch (see
    /// `strip_harness_owned`) so a model patch touching
    /// `notes`/`reports`/`seen` commits the rest of the patch instead of
    /// being rejected outright. Then strips a malformed `next` (see
    /// `strip_malformed_next`) before `validate_default` runs, attaching the
    /// note it returns to the resulting `ValidatedStep`. Then
    /// `shell_read_substitution` (BACKLOG.md item 16): a `shell` action
    /// whose `cmd` is a pure single-file read (`shell_read_as_read_file`)
    /// is replaced with the equivalent `read_file` call, so it pages, lands
    /// in `seen`, and counts for the read guards, instead of running as an
    /// opaque `shell` call those guards cannot see. Then `write_file_auto_
    /// confirm` (BACKLOG.md item 15): on a `write_file` action, strips any
    /// model-supplied `confirmed_full_read` from `args` and, when
    /// `write_file_confirmed_by_seen` says `seen` already shows a full,
    /// recent read of the target path, sets it back to `true` -- the whole-
    /// file rewrite guard (`tools/src/write_file.rs`'s line cap) then
    /// consults the harness's own record of what the model has read instead
    /// of a fixed line count, silently (no note either way; the tool's own
    /// refusal explains the rule when the flag isn't set). Then
    /// `implement_repeat_failure_thinks`: with a repeated test failure open
    /// and no `think` since, an edit, an idle test rerun, or a non-question
    /// `ask_user` is replaced with a `think` call instead (see its doc for
    /// the three shapes). This runs deliberately early -- before
    /// `implement_done_not_ours_strip` and the `done`/`ask_user` chain below
    /// -- so an `ask_user` it replaces is already `Tool { name: "think",
    /// .. }` by the time `premature_done_note`'s fallthrough to
    /// `ask_user_empty_note`/`ask_user_statement_note` would otherwise see
    /// and re-convert it. Then `implement_early_park_thinks`
    /// (DECISIONS.md D4): a SECOND consecutive non-question `ask_user`, with
    /// no repeated test failure open (so the function above did not already
    /// fire) and no `think` since the last recorded tool call, is replaced
    /// with a `think` call the same way, one strike before
    /// `CONTINUE_REPEAT_CAP` (`core/src/session.rs`) would otherwise park
    /// the run on the third. Runs immediately after
    /// `implement_repeat_failure_thinks` for the same reason: an `ask_user`
    /// it replaces must already be `Tool { name: "think", .. }` before the
    /// `done`/`ask_user` chain below can see it. Then
    /// `implement_done_not_ours_strip`: whenever the
    /// raw patch would commit
    /// `done: true`, that key is removed before any of the conversions below
    /// run -- `done` is never `implement`'s to set, only `review`'s (see
    /// that function's doc for why this only strips the key and returns a
    /// note, rather than also deciding the action the way `plan`/`router`'s
    /// `done_not_ours_note` does). Every conversion below therefore sees a
    /// merged workspace exactly as if `done` had never been sent. Then, when
    /// verification passed as of this activation
    /// (`cur.harness.green_step.is_some()`) and the action is
    /// `done`, `green_done_note` overrides the ordinary premature-done
    /// bookkeeping: it marks every open plan step done and hands off to
    /// review instead of converting to `Continue` (see its doc -- this is
    /// what closes the gap where a passing-test `done` used to get steered
    /// into an unrelated edit that broke the checkpointed state). Next,
    /// `implement_bare_done` catches the gap `green_done_note` leaves open:
    /// verification already green, plan already fully done, but `done` was
    /// sent with no `next_skill` -- it injects `next_skill: "review"`
    /// instead of letting `validate_default`'s bare-`done` rule reject the
    /// step (see its doc). Next, `implement_no_green_runs_test`: when the
    /// action is still `Done` after both of those (so `cur.harness.
    /// green_step` is `None` -- the most recent `test` run anywhere in the
    /// session failed, or none has run) and either something was edited
    /// since the last test or none has ever run, this substitutes a real
    /// `test` call for the handoff -- run 59: `implement` handed off to
    /// `review` right after its own `test` call failed, and `review`
    /// approved the failing tree anyway. This runs BEFORE
    /// `premature_done_note` now, and fires even when a plan step is still
    /// open: a later live run showed the open-step note just getting
    /// repeated three times (into the repeat cap) instead of the model
    /// running the test itself, so the harness now runs it regardless of
    /// what plan step is open (see that function's doc). Finally,
    /// `premature_done_note` converts a premature `done` (a plan step still
    /// open, in the one shape `implement_no_green_runs_test` leaves alone --
    /// a failing run with nothing edited since) or an empty `ask_user` into
    /// the harness-internal `Action::Continue` instead of rejecting the step
    /// (see its doc): the patch is kept, no retry is spent, and the model is
    /// told in its next observation what is still open.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let owned_notes = strip_harness_owned(&self.harness_owned_fields(), &mut out);
        let strip_note = strip_malformed_next(&cur.agent, &mut out);
        let shell_read_note = shell_read_substitution(&mut out);
        write_file_auto_confirm(cur, &mut out);
        // Unit 20: a model-chosen `try_fixes` with no failing `tests` record
        // to work from is substituted for `test` before anything else below
        // gets a chance to see (or itself construct) a `try_fixes` action --
        // `implement_repeat_failure_thinks` only ever builds `try_fixes`
        // itself once a repeated failure is already open, which implies a
        // failing `tests` record exists, so the two never fight over the
        // same step.
        let try_fixes_gate_note = implement_try_fixes_needs_failing_test(&mut out, cur);
        // Unit 21: a second refused re-read (core's `skipped_redundant`
        // guard already declined to re-run it once) is substituted for
        // `think` before `implement_repeat_failure_thinks` gets a look --
        // the two never target the same action shape (this one only fires
        // on a read-only tool call; that one only fires on an edit/idle-test/
        // ask_user), so ordering between them never matters in practice, but
        // this runs first to match the brief's stated order.
        let repeated_skip_think_note = implement_repeated_skip_thinks(cur, &mut out);
        // Unit 23 part (b), reshaped by unit 25: the second time in a row
        // the model repeats the SAME KIND (tool + error shape) of tool call
        // that just failed (`HarnessState::last_tool_error`), substituted
        // for `think` right after the repeated-skip trigger above,
        // mirroring its wiring -- the two never target the same committed
        // action twice (a tool call that just failed and a read-only call
        // that was just skipped are mutually exclusive outcomes of the SAME
        // step), so ordering between them only matters in the sense the
        // brief states it in.
        let repeated_error_think_note = implement_repeated_error_thinks(cur, &mut out);
        // Unit 25 part 3: a stalled path (a refused re-read and a failed
        // edit on the SAME path, in either order, `HarnessState::
        // last_stall`) is substituted for `think` right after the
        // repeated-error trigger above -- run last of the three `think`
        // substitutions since it is the broadest (it can fire on either a
        // read-only OR an edit-tool action), and `substitute_think` is
        // idempotent-safe to call at most once per step, matching the other
        // two triggers here.
        let stall_think_note = implement_stall_thinks(cur, &mut out);
        // Unit 27 ("no green cap"): a run that has failed `test` at least
        // `NO_GREEN_THINK_AT` times since its last pass, with no `think`
        // since that failing test's own step, is substituted for `think`
        // right after the stalled-path trigger above -- keyed on the raw
        // failure count rather than any particular action shape, so it can
        // fire on an action none of the streak-shaped triggers above would
        // recognize.
        let no_green_think_note = implement_no_green_thinks(cur, &mut out);
        let repeat_think_note = implement_repeat_failure_thinks(cur, &mut out);
        // Only reachable when `implement_repeat_failure_thinks` did not
        // already fire: that function replaces a matching `ask_user` action
        // with `Action::Tool { name: "think", .. }`, so `out.action()` is no
        // longer `AskUser` by the time this runs whenever it already
        // substituted -- the two never both act on the same step.
        let early_park_think_note = implement_early_park_thinks(cur, &mut out);
        let not_ours_note = implement_done_not_ours_strip(&cur.agent, &mut out);
        let mut extra_note = None;
        if let Some(note) = green_done_note(cur, &mut out) {
            // Action stays `Done`: verification already passed, so this is
            // a real handoff, not a harness-internal `Continue`.
            extra_note = Some(note);
        } else if let Some(note) = implement_bare_done(cur, &mut out) {
            // Action stays `Done`: verification already passed and the plan
            // is complete, so this is a real handoff too.
            extra_note = Some(note);
        } else if let Some(note) = implement_no_green_runs_test(cur, &mut out) {
            // `implement_no_green_runs_test` substitutes a real `test` tool
            // call for the test-needed case (action already set, see its
            // doc); for the Continue-fallback case it leaves the action
            // untouched (still `Done`) and relies on the caller to convert
            // it, the same as every other note function here.
            if matches!(out.action(), statie_core::Action::Done) {
                out.set_action(statie_core::Action::Continue { note });
            } else {
                extra_note = Some(note);
            }
        } else if let Some(note) = premature_done_note(&cur.agent, &mut out) {
            out.set_action(statie_core::Action::Continue { note });
        }
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        let notes: Vec<String> = owned_notes
            .into_iter()
            .chain(strip_note)
            .chain(shell_read_note)
            .chain(try_fixes_gate_note)
            .chain(repeated_skip_think_note)
            .chain(repeated_error_think_note)
            .chain(stall_think_note)
            .chain(no_green_think_note)
            .chain(repeat_think_note)
            .chain(early_park_think_note)
            .chain(not_ours_note)
            .chain(extra_note)
            .collect();
        Ok(if notes.is_empty() {
            step
        } else {
            step.with_notes(notes)
        })
    }
}

impl ReviewSkill {
    pub fn new() -> Self {
        ReviewSkill::default()
    }

    /// Builds a `review` skill configured with the given subagent names as
    /// its allowed delegates. Appends the "Subagents:" paragraph to its
    /// instructions when `delegates` is non-empty.
    pub fn with_delegates(delegates: Vec<String>) -> Self {
        ReviewSkill {
            spec: OnceSpec::default(),
            delegates,
            done_stays_done: true,
            seen: true,
            decision: true,
            tests: true,
            listed: true,
            grounded_findings: true,
            situational: false,
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
        self
    }

    /// Sets `thinker.listed`; see [`PlanSkill::with_listed`].
    pub fn with_listed(mut self, listed: bool) -> Self {
        self.listed = listed;
        self
    }

    /// Sets `guards.seen_map`/`guards.decision_field`/`guards.tests_field`;
    /// see [`PlanSkill::state_fields`].
    pub fn state_fields(mut self, seen: bool, decision: bool, tests: bool) -> Self {
        self.seen = seen;
        self.decision = decision;
        self.tests = tests;
        self
    }

    /// Sets `guards.grounded_findings` (default `true`; `core/src/
    /// guards.rs`, item 12: grounded review findings). `false` restores the
    /// pre-grounding behaviour: every send-back finding passes through
    /// unchanged, with no `path:line` check.
    pub fn with_grounded_findings(mut self, enabled: bool) -> Self {
        self.grounded_findings = enabled;
        self
    }

    /// Sets `guards.situational_rules`; see
    /// [`PlanSkill::with_situational_rules`]. `review`'s instructions have
    /// no "after X" bullet against unit 26's node set (see
    /// [`ReviewSkill::situational`]'s doc), so this only changes `spec()`'s
    /// doc note; `situational()` always returns `None` for `review`.
    pub fn with_situational_rules(mut self, enabled: bool) -> Self {
        self.situational = enabled;
        self
    }
}

impl Skill for ReviewSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "review"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        let instructions = format!(
            "You are reviewing the work done for a coding task. Run `test` first: \
             `result: fail` is an automatic finding -- quote the failure summary. Use \
             `search` to check that every plan step's change actually landed. Read only \
             the files in `files` unless a finding requires more. Look before you approve: \
             read the changed files or run the tests first; an approval with no tool call \
             in this activation is not accepted. Before approving, run the tests without a \
             name filter (the crate's whole test target); a pass on one filtered test does \
             not cover regressions.\n\n\
             What to put in the workspace:\n\
             - Write concrete `findings` -- specific, actionable problems, not vague \
               impressions. Leave `findings` empty if the work is correct and complete.\n\
             - If you commit to a next step by setting `next`, the harness holds you to \
               it -- change `next` (and say why in `recent_summary`) if the plan changes.\n\n\
             When to hand off: if `findings` is non-empty, set `next_skill: \"implement\"` \
             and action `done` so the problems get fixed. If `findings` is empty, set \
             `done: true` and action `done` -- this ends the task. `done: true` is only \
             valid with empty `findings`. To send work back, set `findings` and send \
             `done` with `next_skill: \"implement\"`; `done: true` is an approval and \
             needs empty findings.\n\n\
             History is searchable with the `recall` tool -- use it to see what \
             `implement` actually did. `recent_summary` is the only memory that survives \
             across steps and across skill handoffs.{}",
            delegate_paragraph(&self.delegates)
        );
        self.spec.get("review", instructions)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        let mut tools = vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "test".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
        ];
        if self.listed {
            tools.push("think".to_string());
        }
        tools
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec!["implement".to_string(), "router".to_string()]
    }

    fn skill_with_tool(&self, tool: &str) -> Option<String> {
        let s = coding_skill_with_tool(tool)?;
        self.allowed_transitions()
            .iter()
            .any(|t| t == s)
            .then(|| s.to_string())
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        common_is_terminal(s)
    }

    fn allows_yield(&self) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        common_handoff(self.name(), s)
    }

    fn on_activate(&self, ws: &Self::Workspace) -> Option<Patch> {
        common_on_activate(ws)
    }

    fn rules(
        &self,
        cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        common_rules(self.name(), &["implement", "router"], cur, next)?;
        if next.done && !next.findings.is_empty() {
            return Err(PatchError::RuleViolation(
                "done can only be true when findings is empty".to_string(),
            ));
        }
        plan_rules(&cur.agent, next, self.done_stays_done)
    }

    fn allowed_delegates(&self) -> Vec<String> {
        self.delegates.clone()
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        common_absorb_report(ws, report)
    }

    fn commitment(&self, ws: &Self::Workspace) -> Option<Commitment> {
        common_commitment(ws)
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        common_harness_owned_fields()
    }

    fn notes(&self, ws: &Self::Workspace) -> Option<String> {
        common_notes(ws)
    }

    fn situational(&self, cur: &CommittedState<Self::Workspace>) -> Option<String> {
        common_situational(self.situational, cur)
    }

    fn after_tool(
        &self,
        ws: &Self::Workspace,
        _harness: &HarnessState,
        step: u64,
        name: &str,
        args: &serde_json::Value,
        output: &str,
    ) -> Option<Patch> {
        common_after_tool(
            ws,
            step,
            name,
            args,
            output,
            self.seen,
            self.decision,
            self.tests,
        )
    }

    /// First strips every harness-owned key present in the raw patch (see
    /// `strip_harness_owned`) so a model patch touching
    /// `notes`/`reports`/`seen` commits the rest of the patch instead of
    /// being rejected outright. Then strips a malformed `next` (see
    /// `strip_malformed_next`) before `validate_default` runs, attaching the
    /// note it returns to the resulting `ValidatedStep`. Then
    /// `shell_read_substitution` (BACKLOG.md item 16): a `shell` action
    /// whose `cmd` is a pure single-file read is replaced with the
    /// equivalent `read_file` call (see `ImplementSkill::validate`'s doc for
    /// why). Then `write_file_auto_confirm` (BACKLOG.md item 15, harmless
    /// here since `review` has no `write_file` tool -- see
    /// `ImplementSkill::validate`'s doc). Then, converts one of two
    /// send-back shapes a live run sent (see `review_sendback_note`'s doc)
    /// into the intended
    /// handoff to `implement` instead of a rejection; then, still ahead of
    /// the three conversions below, `review_grounded_findings` (item 12:
    /// grounded review findings; `guards.grounded_findings`) drops any
    /// ungrounded finding from whatever `findings` array the raw patch now
    /// carries (whether the model sent it directly or `review_sendback_note`
    /// just normalized it there), attaching a note naming what was dropped
    /// and why -- or, if every finding was ungrounded, propagating a
    /// `PatchError::RuleViolation` straight out of `validate` (via `?`) to
    /// refuse the whole step instead, before any of the further conversions
    /// run; converts an empty
    /// `ask_user` into the harness-internal `Action::Continue` instead of
    /// rejecting the step (case (b) of `premature_done_note`'s doc;
    /// `review`'s `done` semantics -- `done: true` only with empty
    /// `findings` -- are unaffected); and, ahead of all three,
    /// `review_blind_approval_runs_test` substitutes a real `test` tool call
    /// for an approval (`done` with the merged workspace's `done: true`)
    /// sent before this activation has executed a single tool call -- the
    /// harness runs the tests itself rather than just asking the model to
    /// (see that function's doc for why: the ask-first note alone did not
    /// work on this model). Only once that no longer applies (`tool_calls`
    /// is no longer `0`, because either a real call or the substituted one
    /// already ran) does `review_looked_note` get a chance to convert a
    /// *further* look-nothing approval into `Action::Continue` instead --
    /// review must look before it approves. `review_sendback_note` requires
    /// non-empty `findings` in its `done`-rewriting case, and both
    /// `review_blind_approval_runs_test`/`review_looked_note` require empty
    /// `findings`, so none of the three ever both fire on the same step.
    /// Next, still ahead of the empty-`ask_user` check, `review_no_green_note`
    /// converts an approval sent while `cur.harness.green_step` is `None`
    /// (the most recent `test` run anywhere in the session failed, or none
    /// has run) into `Continue` as well -- run 59: `review` ran the
    /// unfiltered test, saw it fail, and approved anyway. Next,
    /// `review_filtered_note` converts an approval sent while
    /// `green_step.is_some() && green_filtered` (the passing run had tests
    /// filtered out) into `Continue` too -- run 67: `review` ran the same
    /// filtered command `implement` used, saw it pass, and approved anyway,
    /// while the unfiltered suite failed on disk. A send-back (non-empty
    /// `findings`) is never affected by any of these.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let owned_notes = strip_harness_owned(&self.harness_owned_fields(), &mut out);
        let note = strip_malformed_next(&cur.agent, &mut out);
        let shell_read_note = shell_read_substitution(&mut out);
        write_file_auto_confirm(cur, &mut out);
        let sendback_note = review_sendback_note(self, &cur.agent, &mut out);
        let grounded_note = review_grounded_findings(self.grounded_findings, &cur.agent, &mut out)?;
        let mut blind_approval_note = None;
        if sendback_note.is_none() {
            blind_approval_note = review_blind_approval_runs_test(cur, &mut out);
            if blind_approval_note.is_some() {
                // Action already substituted for a `test` tool call inside
                // `review_blind_approval_runs_test`; none of the further
                // conversions below apply to this step.
            } else if let Some(note) = review_looked_note(cur, &mut out) {
                out.set_action(statie_core::Action::Continue { note });
            } else if let Some(note) = review_no_green_note(cur, &mut out) {
                out.set_action(statie_core::Action::Continue { note });
            } else if let Some(note) = review_filtered_note(cur, &mut out) {
                out.set_action(statie_core::Action::Continue { note });
            } else if let Some(note) = ask_user_empty_note(&mut out) {
                out.set_action(statie_core::Action::Continue { note });
            }
        }
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        let notes: Vec<String> = owned_notes
            .into_iter()
            .chain(note)
            .chain(shell_read_note)
            .chain(sendback_note)
            .chain(grounded_note)
            .chain(blind_approval_note)
            .collect();
        Ok(if notes.is_empty() {
            step
        } else {
            step.with_notes(notes)
        })
    }
}

/// All four coding skills, keyed by name, as trait objects sharing the
/// coding workspace/patch types, with no configured delegates (equivalent
/// to `coding_skills_with_delegates(vec![])`; instruction text is
/// byte-identical to before delegation existed).
#[allow(clippy::type_complexity)]
pub fn coding_skills() -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    coding_skills_with_delegates(vec![])
}

/// All four coding skills, keyed by name, as trait objects sharing the
/// coding workspace/patch types. `delegates` names the configured child
/// (subagent) skills that `plan`, `implement`, and `review` may hand a
/// self-contained job to via action `delegate`; `router` never delegates.
/// When `delegates` is empty, instruction text is byte-identical to the
/// no-delegation form (no "Subagents:" paragraph is appended).
///
/// `done_stays_done` defaults to `false` here (`coding_skills_configured`'s
/// default path, the first bench battery's audit showed the switch off no
/// worse -- see `plan_rules`'s doc); `seen_map`/`decision_field`/
/// `tests_field`/`grounded_findings` default to `true` (today's behaviour,
/// like every other guard -- see `core/src/guards.rs`); `situational_rules`
/// (unit 26) defaults to `false`, like `guards.situational_rules` itself.
/// Use `coding_skills_configured` directly to opt any of these back.
#[allow(clippy::type_complexity)]
pub fn coding_skills_with_delegates(
    delegates: Vec<String>,
) -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    coding_skills_configured(delegates, false, true, true, true, true, true, false)
}

/// All four coding skills with every knob: `delegates` as in
/// [`coding_skills_with_delegates`]; `done_stays_done` = `guards.
/// done_stays_done` (statie-spec.md "10. After it works: the rule audit");
/// `seen`/`decision`/`tests` = `guards.seen_map`/`guards.decision_field`/
/// `guards.tests_field` (`core/src/guards.rs`; BACKLOG.md items 2 and 5);
/// `grounded_findings` = `guards.grounded_findings` (`core/src/guards.rs`,
/// item 12: grounded review findings, `review`-only); `situational` =
/// `guards.situational_rules` (`core/src/guards.rs`, unit 26: situational
/// rules) -- all threaded into `plan`, `implement`, and `review` via
/// [`PlanSkill::with_done_stays_done`]/[`PlanSkill::state_fields`]/
/// [`ReviewSkill::with_grounded_findings`]/[`PlanSkill::
/// with_situational_rules`] and their siblings. `router` is unaffected by
/// all six: it has no plan-step rules, no tools, and no procedure-graph
/// text, so nothing in it reads any of them.
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
pub fn coding_skills_configured(
    delegates: Vec<String>,
    done_stays_done: bool,
    seen: bool,
    decision: bool,
    tests: bool,
    grounded_findings: bool,
    listed: bool,
    situational: bool,
) -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    vec![
        ("router".to_string(), Arc::new(RouterSkill::new())),
        (
            "plan".to_string(),
            Arc::new(
                PlanSkill::with_delegates(delegates.clone())
                    .with_done_stays_done(done_stays_done)
                    .state_fields(seen, decision, tests)
                    .with_listed(listed)
                    .with_situational_rules(situational),
            ),
        ),
        (
            "implement".to_string(),
            Arc::new(
                ImplementSkill::with_delegates(delegates.clone())
                    .with_done_stays_done(done_stays_done)
                    .state_fields(seen, decision, tests)
                    .with_listed(listed)
                    .with_situational_rules(situational),
            ),
        ),
        (
            "review".to_string(),
            Arc::new(
                ReviewSkill::with_delegates(delegates)
                    .with_done_stays_done(done_stays_done)
                    .state_fields(seen, decision, tests)
                    .with_grounded_findings(grounded_findings)
                    .with_listed(listed)
                    .with_situational_rules(situational),
            ),
        ),
    ]
}
