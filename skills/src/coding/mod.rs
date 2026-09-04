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

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use statie_core::{
    Commitment, CommittedState, ParsedOutput, PatchError, Report, Skill, SkillSpec, ValidatedStep,
    validate_default,
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

/// `commitment` shared by `plan`/`implement`/`review`: the workspace's
/// `next` field, verbatim.
fn common_commitment(ws: &CodingWorkspace) -> Option<Commitment> {
    ws.next.clone()
}

/// `harness_owned_fields` shared by every coding skill: `notes` (maintained
/// by the harness's `Maintainer` call after each step, see statie-spec.md
/// "9. Context maintenance") and `reports` (written by `absorb_report` after
/// a `delegate` action) are never model-patchable. Checked uniformly by
/// `validate_default` against every action.
fn common_harness_owned_fields() -> Vec<String> {
    vec!["notes".to_string(), "reports".to_string()]
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

/// Tools `next.tool` may name: the mutating calls a commitment is allowed to
/// bind to.
const MUTATING_TOOLS: [&str; 5] = ["edit_file", "write_file", "revert_file", "shell", "test"];

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
/// Called in `ImplementSkill::validate` AFTER `green_done_note` and
/// `premature_done_note`: only reached when the action is still `Done`
/// after both of those, which -- for `premature_done_note` -- means its
/// case (a) did not fire, i.e. the merged plan has no open step (an open
/// step takes priority over this check) and the merged workspace is not
/// itself terminal (`done: true`). Fires when `cur.harness.green_step` is
/// `None`: the most recent `test` run anywhere in the session failed, or
/// none has ever run. Converts the action to `Action::Continue`; the patch
/// is kept, and `next_skill` is stripped from the raw patch so no handoff
/// lands (same treatment as `premature_done_note`'s case (a)).
///
/// A patch that fails to deserialize/apply is left alone (`None`): it falls
/// straight through to `validate_default`, which reports the real error.
fn implement_no_green_note(
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
    if merged.done
        || merged
            .plan
            .iter()
            .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress))
    {
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
/// (`implement_no_green_note`/plan-complete check), which only cares that
/// SOME recent run passed, not whether it was filtered.
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
/// `enabled` is `guards.done_stays_done` (default `true`, statie-spec.md
/// "10. After it works: the rule audit"); when `false`, the whole rule is a
/// no-op and reopening a done step is allowed.
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
}

impl Default for PlanSkill {
    fn default() -> Self {
        PlanSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
        }
    }
}

/// The implement skill: works the plan, one step at a time.
pub struct ImplementSkill {
    spec: OnceSpec,
    delegates: Vec<String>,
    /// `guards.done_stays_done`; see [`PlanSkill::done_stays_done`].
    done_stays_done: bool,
}

impl Default for ImplementSkill {
    fn default() -> Self {
        ImplementSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
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
}

impl Default for ReviewSkill {
    fn default() -> Self {
        ReviewSkill {
            spec: OnceSpec::default(),
            delegates: Vec::new(),
            done_stays_done: true,
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
             `next_skill` set.\n\n\
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

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        common_is_terminal(s)
    }

    fn allows_yield(&self) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        common_handoff(self.name(), s)
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
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
        self
    }
}

impl Skill for PlanSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "plan"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        let instructions = format!(
            "You are the planner for a coding task. Use `search` to find definitions and \
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
               it -- change `next` (and say why in `recent_summary`) if the plan changes.\n\n\
             When to hand off: once the plan is solid, set `next_skill: \"implement\"` \
             and action `done`. The plan must be non-empty before you hand off to \
             `implement`.\n\n\
             History is searchable with the `recall` tool if you need to check what an \
             earlier step already found. `recent_summary` is the only memory that \
             survives across steps and across skill handoffs. If a tool output overflowed \
             (the observation says so), call `filter_last_tool_call` with a regex instead \
             of re-running the command.{}",
            delegate_paragraph(&self.delegates)
        );
        self.spec.get("plan", instructions)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
            "test".to_string(),
        ]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec!["implement".to_string(), "router".to_string()]
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
    /// Also strips a malformed `next` (see `strip_malformed_next`) before
    /// `validate_default` runs, attaching the note it returns to the
    /// resulting `ValidatedStep`, and converts an empty `ask_user` into the
    /// harness-internal `Action::Continue` instead of rejecting the step
    /// (case (b) of `premature_done_note`'s doc; `plan` does not get case
    /// (a) -- its bare-`done` handling above already covers `done`).
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let note = strip_malformed_next(&cur.agent, &mut out);

        if let Some(note) = ask_user_empty_note(&mut out) {
            out.set_action(statie_core::Action::Continue { note });
        }

        let is_done = matches!(out.action(), statie_core::Action::Done);
        if is_done {
            // Apply the patch strategy the same way `validate_default` is
            // about to, just to peek at whether the resulting plan is
            // non-empty. A patch that fails to deserialize/apply is left
            // alone here -- it falls straight through to
            // `validate_default` below, which reports the real error.
            let merged_plan_nonempty = serde_json::from_value::<
                <Self::Patch as statie_core::PatchStrategy<CodingWorkspace>>::Patch,
            >(out.patch().0.clone())
            .ok()
            .and_then(|patch| {
                <Self::Patch as statie_core::PatchStrategy<CodingWorkspace>>::apply(
                    &cur.agent, &patch,
                )
                .ok()
            })
            .map(|next| !next.plan.is_empty());

            match merged_plan_nonempty {
                Some(true) if out.patch().0.get("next_skill").is_none() => {
                    // Inject the missing key into the raw merge patch; the
                    // Patch log entry will simply show this key alongside
                    // whatever else the model sent.
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
                        obj.remove("next_skill");
                    }
                    out.set_action(statie_core::Action::Continue {
                        note: continue_note,
                    });
                }
                _ => {}
            }
        }
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        Ok(match note {
            Some(note) => step.with_notes(vec![note]),
            None => step,
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
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
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
        let instructions = format!(
            "You are implementing the plan for a coding task, one step at a time.\n\n\
             What to put in the workspace:\n\
             - Mark the step you are working `in_progress`, then `done` once it is \
               finished; use `skipped` only when a step turns out unnecessary.\n\
             - Prefer `edit_file` for changes to existing files: give it a small \
               anchored `old` text copied exactly from a `read_file` result, including \
               indentation. Use `write_file` only for new files or full rewrites. If \
               `edit_file` reports the text was not found or ambiguous, re-read the \
               file and retry with a more specific anchor -- never fall back to \
               rewriting the whole file. To add code, anchor on one line you can see: \
               `edit_file` with `after` (or `before`) and `new`, instead of a \
               multi-line `old`. When a tool reports a line number, read from that \
               offset (`read_file` with `offset`) instead of paging from the top. \
               Use `shell` for other changes.\n\
             - Commit before you look: when you decide what to change, set `next` \
               (tool, path, what) in the same patch, where `tool` is the mutating call \
               you will make -- `edit_file`, `write_file`, `shell`, or `test`, never \
               `read_file`. While `next` is set you may read that file at most twice; \
               then the harness expects the call. If the plan changes, change `next` \
               and say why in `recent_summary`.\n\
             - After finishing a step that changes code, run `test` and read the \
               `result:` line; do not mark a step `done` while `test` reports `fail` \
               unless the failure predates your change (check with `recall`, e.g. \
               query \"test output\" to see the last test run).\n\
             - Keep `files` and `recent_summary` up to date as you go.\n\
             - `notes` is kept by the harness from your previous reasoning: read it \
               before deciding; you cannot patch it.\n\
             - If you are blocked (missing information only the user can give you), use \
               action `ask_user` rather than guessing -- it is the only way to pause; \
               `yield` is rejected in this skill.\n\n\
             When to hand off: once every plan step is `done` or `skipped`, set \
             `next_skill: \"review\"` and action `done`. Do not hand off to `review` \
             while any step is still `pending` or `in_progress`.\n\n\
             History is searchable with the `recall` tool -- use it to recall what an \
             earlier step already tried or found. `recent_summary` is the only memory \
             that survives across steps and across skill handoffs. If a tool output \
             overflowed (the observation says so), call `filter_last_tool_call` with a \
             regex instead of re-running the command.{}",
            delegate_paragraph(&self.delegates)
        );
        self.spec.get("implement", instructions)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        CodingWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
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
        ]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec![
            "review".to_string(),
            "plan".to_string(),
            "router".to_string(),
        ]
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

    fn silent_read_cap(&self) -> Option<u32> {
        // Two free looks that write nothing: a read whose patch records a
        // file note, a plan step, or `recent_summary` is never counted, so
        // this only bounds a look that leaves no trace.
        Some(2)
    }

    /// Strips a malformed `next` (see `strip_malformed_next`) before
    /// `validate_default` runs, attaching the note it returns to the
    /// resulting `ValidatedStep`. Then, when verification passed as of this
    /// activation (`cur.harness.green_step.is_some()`) and the action is
    /// `done`, `green_done_note` overrides the ordinary premature-done
    /// bookkeeping: it marks every open plan step done and hands off to
    /// review instead of converting to `Continue` (see its doc -- this is
    /// what closes the gap where a passing-test `done` used to get steered
    /// into an unrelated edit that broke the checkpointed state).
    /// Otherwise, converts a premature `done` (a plan step still open) or
    /// an empty `ask_user` into the harness-internal `Action::Continue`
    /// instead of rejecting the step (see `premature_done_note`'s doc): the
    /// patch is kept, no retry is spent, and the model is told in its next
    /// observation what is still open. Finally, when the action is still
    /// `Done` after both of those (the merged plan has no open step) and
    /// `cur.harness.green_step` is `None` (the most recent `test` run
    /// anywhere in the session failed, or none has run), `implement_no_green_
    /// note` converts the handoff to `Continue` too -- run 59: `implement`
    /// handed off to `review` right after its own `test` call failed, and
    /// `review` approved the failing tree anyway.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        mut out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        let strip_note = strip_malformed_next(&cur.agent, &mut out);
        let mut extra_note = None;
        if let Some(note) = green_done_note(cur, &mut out) {
            // Action stays `Done`: verification already passed, so this is
            // a real handoff, not a harness-internal `Continue`.
            extra_note = Some(note);
        } else if let Some(note) = premature_done_note(&cur.agent, &mut out) {
            out.set_action(statie_core::Action::Continue { note });
        } else if let Some(note) = implement_no_green_note(cur, &mut out) {
            out.set_action(statie_core::Action::Continue { note });
        }
        let step = validate_default::<Self::Workspace, Self::Patch>(self, cur, out)?;
        let notes: Vec<String> = strip_note.into_iter().chain(extra_note).collect();
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
        }
    }

    /// Sets `guards.done_stays_done` (default `true`). `false` allows a
    /// `done` plan step to be reopened (statie-spec.md "10. After it
    /// works: the rule audit").
    pub fn with_done_stays_done(mut self, enabled: bool) -> Self {
        self.done_stays_done = enabled;
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
        vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "test".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
        ]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec!["implement".to_string(), "router".to_string()]
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

    /// Strips a malformed `next` (see `strip_malformed_next`) before
    /// `validate_default` runs, attaching the note it returns to the
    /// resulting `ValidatedStep`; converts one of two send-back shapes a
    /// live run sent (see `review_sendback_note`'s doc) into the intended
    /// handoff to `implement` instead of a rejection; converts an empty
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
        let note = strip_malformed_next(&cur.agent, &mut out);
        let sendback_note = review_sendback_note(self, &cur.agent, &mut out);
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
        let notes: Vec<String> = note
            .into_iter()
            .chain(sendback_note)
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
#[allow(clippy::type_complexity)]
pub fn coding_skills_with_delegates(
    delegates: Vec<String>,
) -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    coding_skills_configured(delegates, true)
}

/// All four coding skills with both knobs: `delegates` as in
/// [`coding_skills_with_delegates`], and `done_stays_done` =
/// `guards.done_stays_done` (statie-spec.md "10. After it works: the rule
/// audit") threaded into `plan`, `implement`, and `review`.
#[allow(clippy::type_complexity)]
pub fn coding_skills_configured(
    delegates: Vec<String>,
    done_stays_done: bool,
) -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    vec![
        ("router".to_string(), Arc::new(RouterSkill::new())),
        (
            "plan".to_string(),
            Arc::new(
                PlanSkill::with_delegates(delegates.clone()).with_done_stays_done(done_stays_done),
            ),
        ),
        (
            "implement".to_string(),
            Arc::new(
                ImplementSkill::with_delegates(delegates.clone())
                    .with_done_stays_done(done_stays_done),
            ),
        ),
        (
            "review".to_string(),
            Arc::new(ReviewSkill::with_delegates(delegates).with_done_stays_done(done_stays_done)),
        ),
    ]
}
