//! Shared test-only helpers for exercising `StateStore` impls through
//! core's public API. Used by both `memory` and `sqlite` unit tests.
#![cfg(test)]

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use statie_core::{
    Action, Budgets, CommittedState, HarnessState, ParsedOutput, PatchError, PatchStrategy,
    RawOutput, Skill, SkillSpec, ValidatedPatch, Version,
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Workspace {
    pub value: i64,
}

pub struct ReplaceStrategy;

impl PatchStrategy<Workspace> for ReplaceStrategy {
    type Patch = Workspace;

    fn name() -> &'static str {
        "replace"
    }

    fn describe() -> &'static str {
        "The patch is the entire new workspace value."
    }

    fn apply(_cur: &Workspace, patch: &Workspace) -> Result<Workspace, PatchError> {
        Ok(patch.clone())
    }
}

pub struct ToySkill;

impl Skill for ToySkill {
    type Workspace = Workspace;
    type Patch = ReplaceStrategy;

    fn name(&self) -> &str {
        "toy"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("toy", "a toy skill for tests"))
    }

    fn initial_workspace(&self) -> Self::Workspace {
        Workspace { value: 0 }
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }
}

pub fn harness(step: u64) -> HarnessState {
    HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: Budgets {
            total: 1000,
            observation: 100,
            augmentation: 100,
            notes: 400,
            working_set: 6000,
        },
        activation: "toy".to_string(),
        step,
        last_overflow: None,
        pinned: None,
        green_step: None,
        green_filtered: false,
        tool_calls: 0,
        last_test_cmd: None,
        edits_since_test: 0,
        last_think_step: None,
        repeated_failure_step: None,
    }
}

pub fn committed_state(value: i64, step: u64) -> CommittedState<Workspace> {
    CommittedState {
        harness: harness(step),
        agent: Workspace { value },
    }
}

/// Build a `ValidatedPatch<Workspace>` the way a real caller would: through
/// `RawOutput` -> `ParsedOutput::parse` -> `Skill::validate`, going through
/// core's public path only.
pub fn make_validated_patch(
    cur: &CommittedState<Workspace>,
    version: Version,
    value: i64,
    ack_inbox: &[u64],
) -> (ValidatedPatch<Workspace>, Action) {
    let content = format!(
        r#"{{"patch":{{"value":{value}}},"action":{{"type":"yield"}},"ack_inbox":{acks:?}}}"#,
        value = value,
        acks = ack_inbox,
    );
    let raw = RawOutput::new(version, None, content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).expect("parses");
    let skill = ToySkill;
    let step = skill.validate(cur, parsed).expect("validates");
    let (patch, action, _acks) = step.into_parts();
    (patch, action)
}
