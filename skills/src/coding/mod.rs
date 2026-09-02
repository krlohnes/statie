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

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use statie_core::{CommittedState, PatchError, Skill, SkillSpec};
use statie_patch::JsonMergePatch;

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
    /// Current status of this step.
    pub status: StepStatus,
}

/// A file the task has touched or judged relevant, with a one-line note
/// explaining why (e.g. "contains the parser to modify", "existing test to
/// extend").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileNote {
    /// Path to the file, relative to the sandbox root.
    pub path: String,
    /// One line: why this file matters to the task.
    pub note: String,
}

/// Shared workspace for the coding skill set (router/plan/implement/review).
///
/// This is the *only* memory that survives across steps and across skill
/// handoffs: keep `recent_summary`, `files`, `plan`, and `findings` current
/// enough that the next step (possibly a different skill) can pick up
/// without re-deriving context. Full step-by-step history is not in the
/// prompt, but it is searchable with the `recall` tool.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// The router skill: decides which coding skill should run next and does
/// nothing else. Has no tools -- routing is a pure decision over the
/// workspace and inbox.
#[derive(Default)]
pub struct RouterSkill {
    spec: OnceSpec,
}

/// The plan skill: explores the repo and writes the plan.
#[derive(Default)]
pub struct PlanSkill {
    spec: OnceSpec,
}

/// The implement skill: works the plan, one step at a time.
#[derive(Default)]
pub struct ImplementSkill {
    spec: OnceSpec,
}

/// The review skill: re-checks the changed files and either approves or
/// sends work back to `implement`.
#[derive(Default)]
pub struct ReviewSkill {
    spec: OnceSpec,
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
}

impl PlanSkill {
    pub fn new() -> Self {
        PlanSkill::default()
    }
}

impl Skill for PlanSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "plan"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.spec.get(
            "plan",
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
             - Keep `recent_summary` current as you explore.\n\n\
             When to hand off: once the plan is solid, set `next_skill: \"implement\"` \
             and action `done`. The plan must be non-empty before you hand off to \
             `implement`.\n\n\
             History is searchable with the `recall` tool if you need to check what an \
             earlier step already found. `recent_summary` is the only memory that \
             survives across steps and across skill handoffs. If a tool output overflowed \
             (the observation says so), call `filter_last_tool_call` with a regex instead \
             of re-running the command."
                .to_string(),
        )
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
        Ok(())
    }
}

impl ImplementSkill {
    pub fn new() -> Self {
        ImplementSkill::default()
    }
}

impl Skill for ImplementSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "implement"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.spec.get(
            "implement",
            "You are implementing the plan for a coding task, one step at a time.\n\n\
             What to put in the workspace:\n\
             - Mark the step you are working `in_progress`, then `done` once it is \
               finished; use `skipped` only when a step turns out unnecessary.\n\
             - Prefer `edit_file` for changes to existing files: give it a small \
               anchored `old` text copied exactly from a `read_file` result, including \
               indentation. Use `write_file` only for new files or full rewrites. If \
               `edit_file` reports the text was not found or ambiguous, re-read the \
               file and retry with a more specific anchor -- never fall back to \
               rewriting the whole file. Use `shell` for other changes.\n\
             - After finishing a step that changes code, run `test` and read the \
               `result:` line; do not mark a step `done` while `test` reports `fail` \
               unless the failure predates your change (check with `recall`, e.g. \
               query \"test output\" to see the last test run).\n\
             - Keep `files` and `recent_summary` up to date as you go.\n\
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
             regex instead of re-running the command."
                .to_string(),
        )
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
        Ok(())
    }
}

impl ReviewSkill {
    pub fn new() -> Self {
        ReviewSkill::default()
    }
}

impl Skill for ReviewSkill {
    type Workspace = CodingWorkspace;
    type Patch = JsonMergePatch<CodingWorkspace>;

    fn name(&self) -> &str {
        "review"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.spec.get(
            "review",
            "You are reviewing the work done for a coding task. Run `test` first: \
             `result: fail` is an automatic finding -- quote the failure summary. Use \
             `search` to check that every plan step's change actually landed. Read only \
             the files in `files` unless a finding requires more.\n\n\
             What to put in the workspace:\n\
             - Write concrete `findings` -- specific, actionable problems, not vague \
               impressions. Leave `findings` empty if the work is correct and complete.\n\n\
             When to hand off: if `findings` is non-empty, set `next_skill: \"implement\"` \
             and action `done` so the problems get fixed. If `findings` is empty, set \
             `done: true` and action `done` -- this ends the task. `done: true` is only \
             valid with empty `findings`.\n\n\
             History is searchable with the `recall` tool -- use it to see what \
             `implement` actually did. `recent_summary` is the only memory that survives \
             across steps and across skill handoffs."
                .to_string(),
        )
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
        Ok(())
    }
}

/// All four coding skills, keyed by name, as trait objects sharing the
/// coding workspace/patch types.
#[allow(clippy::type_complexity)]
pub fn coding_skills() -> Vec<(
    String,
    Arc<dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>>,
)> {
    vec![
        ("router".to_string(), Arc::new(RouterSkill::new())),
        ("plan".to_string(), Arc::new(PlanSkill::new())),
        ("implement".to_string(), Arc::new(ImplementSkill::new())),
        ("review".to_string(), Arc::new(ReviewSkill::new())),
    ]
}
