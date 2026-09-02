//! Skills: spec, trait, parsed/validated output.

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{ParseError, PatchError};
use crate::model::{Action, RawOutput, StepOutput};
use crate::patch::{CheckedPatch, InboxAcked, PatchStrategy, RawPatch, ValidatedPatch};
use crate::promptable::{Promptable, sealed};
use crate::version::{CommittedState, HarnessState, Version};

/// Activation-specific instructions. Immutable per activation (C1): no
/// `&mut` accessors.
pub struct SkillSpec {
    name: String,
    instructions: String,
}

impl SkillSpec {
    pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        SkillSpec {
            name: name.into(),
            instructions: instructions.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl sealed::Sealed for Arc<SkillSpec> {}
impl Promptable for Arc<SkillSpec> {
    fn render(&self) -> String {
        format!("# {}\n{}", self.name, self.instructions)
    }
}

/// The output of core's parser: `StepOutput<P::Patch>` re-serialized as
/// `RawPatch`, plus action, acks, and the prompt version it answers.
pub struct ParsedOutput<W> {
    patch: RawPatch,
    action: Action,
    ack_inbox: Vec<u64>,
    version: Version,
    _marker: std::marker::PhantomData<W>,
}

impl<W> std::fmt::Debug for ParsedOutput<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedOutput")
            .field("patch", &self.patch)
            .field("action", &self.action)
            .field("ack_inbox", &self.ack_inbox)
            .field("version", &self.version)
            .finish()
    }
}

impl<W> ParsedOutput<W> {
    /// Deserialize `raw.content` into `StepOutput<P::Patch>` (this is the
    /// schema check). Tolerates leading/trailing whitespace and a fenced
    /// ```json block around the object.
    pub fn parse<P: PatchStrategy<W>>(raw: RawOutput) -> Result<ParsedOutput<W>, ParseError> {
        let version = raw.version();
        let cleaned = strip_fence(raw.content.trim());
        let parsed: StepOutput<P::Patch> = serde_json::from_str(cleaned)
            .map_err(|e| ParseError::InvalidJson(e.to_string()))?;
        let patch_value = serde_json::to_value(&parsed.patch)
            .map_err(|e| ParseError::InvalidJson(e.to_string()))?;
        Ok(ParsedOutput {
            patch: RawPatch(patch_value),
            action: parsed.action,
            ack_inbox: parsed.ack_inbox,
            version,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn patch(&self) -> &RawPatch {
        &self.patch
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn ack_inbox(&self) -> &[u64] {
        &self.ack_inbox
    }

    pub fn version(&self) -> Version {
        self.version
    }
}

fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    s
}

/// A validated step: a committable patch plus the action to execute and the
/// acks that were applied.
pub struct ValidatedStep<W> {
    patch: ValidatedPatch<W>,
    action: Action,
    acks: Vec<u64>,
}

impl<W: std::fmt::Debug> std::fmt::Debug for ValidatedStep<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedStep")
            .field("patch", &self.patch)
            .field("action", &self.action)
            .field("acks", &self.acks)
            .finish()
    }
}

impl<W> ValidatedStep<W> {
    /// Build a validated step whose action is `Tool`. No inbox-ack witness
    /// required: tool calls are allowed with unacked inbox items.
    pub fn tool(patch: ValidatedPatch<W>, acks: Vec<u64>, name: String, args: Value) -> Self {
        ValidatedStep {
            patch,
            action: Action::Tool { name, args },
            acks,
        }
    }

    /// Build a validated step that parks the session (`AskUser`, `Yield`,
    /// or `Done`). Requires proof (`InboxAcked`) that all unacked inbox
    /// items were acked by this step.
    pub fn parking(
        patch: ValidatedPatch<W>,
        acks: Vec<u64>,
        action: Action,
        _w: InboxAcked,
    ) -> Self {
        debug_assert!(
            !matches!(action, Action::Tool { .. }),
            "ValidatedStep::parking called with a Tool action; use ValidatedStep::tool instead"
        );
        ValidatedStep {
            patch,
            action,
            acks,
        }
    }

    pub fn patch(&self) -> &ValidatedPatch<W> {
        &self.patch
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn acks(&self) -> &[u64] {
        &self.acks
    }

    pub fn into_parts(self) -> (ValidatedPatch<W>, Action, Vec<u64>) {
        (self.patch, self.action, self.acks)
    }
}

impl<W> ValidatedStep<W> {
    /// A user-authored patch (`/edit`): action is always `Yield`, no inbox
    /// acks are applied. Used by `Active::user_patch`.
    pub(crate) fn user(patch: ValidatedPatch<W>) -> Self {
        ValidatedStep {
            patch,
            action: Action::Yield,
            acks: Vec::new(),
        }
    }
}

/// Validates a user-authored patch (`/edit`): applies the patch strategy and
/// the skill's `rules`, but does not require or check inbox acks (the
/// resulting action is always `Yield`). Returns the committable patch.
pub(crate) fn validate_user_patch<W, P>(
    skill: &(impl ?Sized + SkillRules<W>),
    cur: &CommittedState<W>,
    raw: RawPatch,
    version: Version,
) -> Result<ValidatedPatch<W>, PatchError>
where
    P: PatchStrategy<W>,
{
    let patch: P::Patch = serde_json::from_value(raw.0.clone())
        .map_err(|e| PatchError::Malformed(e.to_string()))?;
    let next = P::apply(&cur.agent, &patch)?;
    let checked = CheckedPatch::new(raw.0, next, version);

    skill.check_rules(cur, checked.next())?;

    Ok(ValidatedPatch::from_checked(checked))
}

/// All unacked inbox ids must be present in `acks`. Unknown ack ids
/// (referring to no inbox item) are ignored. Returns a witness proving the
/// check happened.
pub fn check_inbox(cur: &HarnessState, acks: &[u64]) -> Result<InboxAcked, PatchError> {
    let missing: Vec<u64> = cur
        .inbox
        .iter()
        .filter(|item| !item.acked)
        .map(|item| item.id)
        .filter(|id| !acks.contains(id))
        .collect();
    if missing.is_empty() {
        Ok(InboxAcked::new())
    } else {
        Err(PatchError::UnackedInbox(missing))
    }
}

/// A pluggable skill: workspace type, patch strategy, tools, rules.
pub trait Skill: Send + Sync + 'static {
    type Workspace: Serialize + DeserializeOwned + schemars::JsonSchema + Clone + Send + Sync + 'static;
    type Patch: PatchStrategy<Self::Workspace>;

    fn name(&self) -> &str;
    fn spec(&self) -> Arc<SkillSpec>;
    fn initial_workspace(&self) -> Self::Workspace;
    /// Active tool names.
    fn tools(&self) -> Vec<String>;
    /// Skill names this one may hand off to.
    fn allowed_transitions(&self) -> Vec<String> {
        vec![]
    }
    fn is_terminal(&self, s: &Self::Workspace) -> bool;

    /// If `Some(name)`, the session hands off to the named skill instead of
    /// calling the model this step. Checked before `is_terminal`.
    fn handoff(&self, _s: &Self::Workspace) -> Option<String> {
        None
    }

    /// Skill-specific rules on the candidate next state (e.g. a length cap
    /// on a summary field).
    fn rules(
        &self,
        _cur: &CommittedState<Self::Workspace>,
        _next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        Ok(())
    }

    /// Full validation. Provided; skills override `rules`, not this.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        validate_default::<Self::Workspace, Self::Patch>(self, cur, out)
    }
}

/// The default `Skill::validate` implementation, exposed as a free function
/// so trait objects and generic code can call it directly.
pub fn validate_default<W, P>(
    skill: &(impl ?Sized + SkillRules<W>),
    cur: &CommittedState<W>,
    out: ParsedOutput<W>,
) -> Result<ValidatedStep<W>, PatchError>
where
    P: PatchStrategy<W>,
{
    let version = out.version();
    let patch: P::Patch = serde_json::from_value(out.patch().0.clone())
        .map_err(|e| PatchError::Malformed(e.to_string()))?;
    let next = P::apply(&cur.agent, &patch)?;
    let checked = CheckedPatch::new(out.patch().0.clone(), next, version);

    skill.check_rules(cur, checked.next())?;

    let validated = ValidatedPatch::from_checked(checked);
    let action = out.action().clone();
    let acks = out.ack_inbox().to_vec();

    if matches!(action, Action::Tool { .. }) {
        let (name, args) = match action {
            Action::Tool { name, args } => (name, args),
            _ => unreachable!(),
        };
        Ok(ValidatedStep::tool(validated, acks, name, args))
    } else {
        if matches!(action, Action::Done)
            && !skill.terminal(validated.next())
            && skill.handoff_target(validated.next()).is_none()
        {
            return Err(PatchError::RuleViolation(
                "action `done` means the activation is complete: the workspace must be \
                 terminal (e.g. the goal reached / `done: true`) or hand off via `next_skill`; \
                 otherwise keep working with a tool, or use `yield`/`ask_user`"
                    .to_string(),
            ));
        }
        let witness = check_inbox(&cur.harness, &acks)?;
        Ok(ValidatedStep::parking(validated, acks, action, witness))
    }
}

/// A narrow view of `Skill` used by `validate_default` so it can be called
/// generically without requiring `Self: Sized`.
pub trait SkillRules<W> {
    fn check_rules(&self, cur: &CommittedState<W>, next: &W) -> Result<(), PatchError>;
    fn terminal(&self, next: &W) -> bool;
    fn handoff_target(&self, next: &W) -> Option<String>;
}

impl<K> SkillRules<K::Workspace> for K
where
    K: Skill + ?Sized,
{
    fn check_rules(
        &self,
        cur: &CommittedState<K::Workspace>,
        next: &K::Workspace,
    ) -> Result<(), PatchError> {
        self.rules(cur, next)
    }

    fn terminal(&self, next: &K::Workspace) -> bool {
        self.is_terminal(next)
    }

    fn handoff_target(&self, next: &K::Workspace) -> Option<String> {
        self.handoff(next)
    }
}
