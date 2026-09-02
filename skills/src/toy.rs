//! `ToySkill`: a minimal skill used to exercise the session loop end to end
//! (milestone 1's "toy skill runs 50 steps with flat prompt size").

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use statie_core::{PatchError, Skill, SkillSpec};
use statie_patch::JsonMergePatch;

/// The toy workspace: a counter driven up to `target`, a one-line summary
/// the model maintains itself, and a list of notes.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct ToyWorkspace {
    #[serde(default)]
    pub counter: u64,
    #[serde(default)]
    pub target: u64,
    #[serde(default)]
    pub recent_summary: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A toy skill: increments a counter each step, calling `echo` along the
/// way, until `counter >= target`.
pub struct ToySkill {
    target: u64,
    spec: Arc<SkillSpec>,
}

impl ToySkill {
    pub fn new(target: u64) -> Self {
        let spec = Arc::new(SkillSpec::new(
            "toy",
            "Each step, increment `counter` by one and keep `recent_summary` to a \
             single short line describing what you just did. Call the `echo` tool \
             with `{\"text\": ...}` each step you are still working. Once \
             `counter >= target`, choose action `done`.",
        ));
        ToySkill { target, spec }
    }
}

impl Skill for ToySkill {
    type Workspace = ToyWorkspace;
    type Patch = JsonMergePatch<ToyWorkspace>;

    fn name(&self) -> &str {
        "toy"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        ToyWorkspace {
            counter: 0,
            target: self.target,
            recent_summary: String::new(),
            notes: Vec::new(),
        }
    }

    fn tools(&self) -> Vec<String> {
        vec!["echo".to_string()]
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.counter >= s.target
    }

    fn rules(
        &self,
        _cur: &statie_core::CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        if next.recent_summary.len() > 500 {
            return Err(PatchError::RuleViolation(
                "recent_summary must be at most 500 characters".to_string(),
            ));
        }
        Ok(())
    }
}
