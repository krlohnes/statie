//! `ChildSkill`: a configured child (subagent) whose workspace is
//! `statie_core::ReportWorkspace`, built entirely from a config stanza
//! (milestone 9, ARCHITECTURE.md "Configured child skills").
//!
//! Unlike the Rust-defined skills (`ToySkill`, the coding four), a child
//! skill carries no Rust type of its own: `ChildSkillConfig` (instructions,
//! tools, an optional guide, an optional `data` JSON Schema, budgets, a step
//! cap, and an opaque `rules` stanza) is the entire definition. The one
//! constant across every child is its workspace/patch pair --
//! `ReportWorkspace` / `JsonMergePatch<ReportWorkspace>` -- so no new
//! generics are needed per child.

use std::sync::Arc;

use jsonschema::Validator;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use statie_core::{CommittedState, PatchError, ReportOutcome, ReportWorkspace, Skill, SkillSpec};
use statie_patch::JsonMergePatch;

/// Token budgets for a child activation. `None` on
/// [`ChildSkillConfig::budgets`] means the child inherits the parent's
/// budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ChildBudgets {
    /// Whole prompt, tokens.
    pub total: usize,
    /// Latest observation section cap.
    pub observation: usize,
    /// All augmenters combined.
    pub augmentation: usize,
}

impl Default for ChildBudgets {
    fn default() -> Self {
        ChildBudgets {
            total: 24000,
            observation: 3000,
            augmentation: 2000,
        }
    }
}

/// The entire definition of a configured child skill, as a config stanza
/// (ARCHITECTURE.md "Configured child skills"). Every field but
/// `instructions` has a serde default so a partial stanza is valid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChildSkillConfig {
    /// The child's task instructions, appended to the harness paragraph
    /// every child gets automatically (see [`ChildSkill::new`]). Required:
    /// this is the only field with no default, since a child with no
    /// instructions has no job.
    pub instructions: String,
    /// Tool names active for this child. Restricted to (a subset of) the
    /// parent's own tool set at delegation time; the child never gets more
    /// tools than its parent has.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Name of a guide (declared in the top-level `guides` config) this
    /// child keeps permanently pinned, carrying its procedure without the
    /// parent ever seeing the guide text.
    #[serde(default)]
    pub guide: Option<String>,
    /// JSON Schema (draft 2020-12 or draft 7) that `data` in the child's
    /// report must validate against. `None` means any `data` is accepted.
    #[serde(default)]
    pub data_schema: Option<serde_json::Value>,
    /// Token budgets for this child's own prompts. `None` means inherit the
    /// parent's budgets.
    #[serde(default)]
    pub budgets: Option<ChildBudgets>,
    /// Maximum steps this child may take before its parent should treat it
    /// as capped.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    /// Predicate rules gating this child's `done`/other actions, evaluated
    /// against the candidate workspace (ARCHITECTURE.md "Rules over
    /// reports"). Kept as opaque JSON for now; a later unit types this
    /// against `core::predicate::Rule`.
    #[serde(default)]
    pub rules: Vec<serde_json::Value>,
}

fn default_max_steps() -> usize {
    30
}

impl Default for ChildSkillConfig {
    fn default() -> Self {
        ChildSkillConfig {
            instructions: String::new(),
            tools: Vec::new(),
            guide: None,
            data_schema: None,
            budgets: None,
            max_steps: default_max_steps(),
            rules: Vec::new(),
        }
    }
}

/// Errors constructing a [`ChildSkill`] from its config.
#[derive(Debug, thiserror::Error)]
pub enum ChildSkillError {
    /// `data_schema` itself failed to compile as a JSON Schema.
    #[error("invalid data_schema for child skill `{name}`: {source}")]
    InvalidDataSchema {
        name: String,
        #[source]
        source: Box<jsonschema::ValidationError<'static>>,
    },
}

/// The fixed harness-appended paragraph every configured child gets, telling
/// it that it is a subagent: it cannot talk to the user, must keep the
/// report fields current, and must finish by setting `done: true`.
const HARNESS_PARAGRAPH: &str = "\n\nYou are running as a subagent (a child activation) for a \
     parent agent, not talking to a person directly -- there is no `ask_user` action available \
     to you. Keep `recent_summary` current every step: one short paragraph describing what you \
     just did. When you are finished, fill in `summary` (at most 1200 characters -- this becomes \
     your parent's observation, so make it count), `findings` (concrete, specific; empty means \
     nothing to report), `files` (touched or judged relevant), and `data` (per your schema, if \
     you have one), set `outcome`, and set `done: true`, then send action `done`. If you are \
     blocked and cannot proceed -- missing information or access only the parent (or the user, \
     through the parent) can supply -- set `outcome: \"blocked\"`, explain why in both `summary` \
     and `data.reason`, and finish the same way: `done: true` with action `done`. Never leave the \
     activation any other way.";

/// A configured child skill: instructions + tools + optional guide +
/// optional `data` schema + budgets + a step cap, all read from config. Its
/// workspace is always [`ReportWorkspace`].
pub struct ChildSkill {
    name: String,
    cfg: ChildSkillConfig,
    spec: Arc<SkillSpec>,
    data_validator: Option<Validator>,
}

impl std::fmt::Debug for ChildSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSkill")
            .field("name", &self.name)
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl ChildSkill {
    /// Builds a child skill from its config stanza. Compiles `data_schema`
    /// once (draft 2020-12/7, no network resolvers) and fails with a clear
    /// error if the schema itself does not compile.
    pub fn new(
        name: impl Into<String>,
        cfg: ChildSkillConfig,
    ) -> Result<ChildSkill, ChildSkillError> {
        let name = name.into();

        let data_validator = match &cfg.data_schema {
            Some(schema) => {
                let validator = jsonschema::validator_for(schema).map_err(|e| {
                    ChildSkillError::InvalidDataSchema {
                        name: name.clone(),
                        source: Box::new(e.to_owned()),
                    }
                })?;
                Some(validator)
            }
            None => None,
        };

        let instructions = format!("{}{}", cfg.instructions, HARNESS_PARAGRAPH);
        let spec = Arc::new(SkillSpec::new(name.clone(), instructions));

        Ok(ChildSkill {
            name,
            cfg,
            spec,
            data_validator,
        })
    }

    /// Maximum steps this child may take before its parent should treat it
    /// as capped.
    pub fn max_steps(&self) -> usize {
        self.cfg.max_steps
    }

    /// Token budgets for this child's own prompts, if it overrides the
    /// parent's.
    pub fn budgets(&self) -> Option<&ChildBudgets> {
        self.cfg.budgets.as_ref()
    }

    /// Name of the guide this child keeps permanently pinned, if any.
    pub fn guide(&self) -> Option<&str> {
        self.cfg.guide.as_deref()
    }
}

impl Skill for ChildSkill {
    type Workspace = ReportWorkspace;
    type Patch = JsonMergePatch<ReportWorkspace>;

    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        ReportWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        self.cfg.tools.clone()
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.done
    }

    fn allows_yield(&self) -> bool {
        false
    }

    fn allows_ask_user(&self) -> bool {
        false
    }

    fn allowed_delegates(&self) -> Vec<String> {
        vec![]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec![]
    }

    fn rules(
        &self,
        _cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        if next.summary.len() > 1200 {
            return Err(PatchError::RuleViolation(format!(
                "summary is over 1200 characters ({})",
                next.summary.len()
            )));
        }

        if next.done {
            if next.summary.is_empty() {
                return Err(PatchError::RuleViolation(
                    "done requires a non-empty summary".to_string(),
                ));
            }
            if next.outcome != ReportOutcome::Done {
                let has_reason = next
                    .data
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|r| !r.is_empty());
                if next.findings.is_empty() && !has_reason {
                    return Err(PatchError::RuleViolation(format!(
                        "done with outcome `{:?}` requires either a non-empty findings list or \
                         a data.reason string explaining why",
                        next.outcome
                    )));
                }
            }
        }

        // The schema is a contract on the *finished* report: checking it on
        // every patch would reject a child's first tool call whenever the
        // schema has required keys.
        if let Some(validator) = self.data_validator.as_ref().filter(|_| next.done) {
            let data_value = serde_json::Value::Object(next.data.clone());
            let messages: Vec<String> = validator
                .iter_errors(&data_value)
                .map(|e| {
                    let path = e.instance_path.to_string();
                    if path.is_empty() {
                        e.to_string()
                    } else {
                        format!("{path}: {e}")
                    }
                })
                .collect();
            if !messages.is_empty() {
                return Err(PatchError::RuleViolation(format!(
                    "data does not match the child's schema: {}",
                    messages.join("; ")
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use statie_core::{FileNote, HarnessState};

    fn committed(ws: ReportWorkspace) -> CommittedState<ReportWorkspace> {
        CommittedState {
            harness: HarnessState::default(),
            agent: ws,
        }
    }

    fn cfg_with_schema(schema: serde_json::Value) -> ChildSkillConfig {
        ChildSkillConfig {
            instructions: "do the thing".to_string(),
            data_schema: Some(schema),
            ..ChildSkillConfig::default()
        }
    }

    fn renders_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"renders": {"type": "boolean"}},
            "required": ["renders"]
        })
    }

    #[test]
    fn data_schema_rejects_wrong_type_with_field_name_in_message() {
        let skill = ChildSkill::new("ui-review", cfg_with_schema(renders_schema())).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: "checked".to_string(),
            data: serde_json::json!({"renders": "yes"})
                .as_object()
                .cloned()
                .unwrap(),
            ..ReportWorkspace::default()
        };

        let err = skill.rules(&cur, &next).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("renders"), "expected `renders` in: {msg}");
    }

    #[test]
    fn data_schema_accepts_matching_data() {
        let skill = ChildSkill::new("ui-review", cfg_with_schema(renders_schema())).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: "checked".to_string(),
            data: serde_json::json!({"renders": true})
                .as_object()
                .cloned()
                .unwrap(),
            ..ReportWorkspace::default()
        };

        assert!(skill.rules(&cur, &next).is_ok());
    }

    #[test]
    fn data_schema_is_not_checked_before_done() {
        let skill = ChildSkill::new("ui-review", cfg_with_schema(renders_schema())).unwrap();
        let cur = committed(ReportWorkspace::default());
        // Empty `data` violates `required: ["renders"]`, but the child is
        // still working: a tool step must not be rejected for it.
        let working = ReportWorkspace {
            recent_summary: "reading the page".to_string(),
            ..ReportWorkspace::default()
        };
        assert!(skill.rules(&cur, &working).is_ok());
        let finished = ReportWorkspace {
            done: true,
            summary: "checked".to_string(),
            ..ReportWorkspace::default()
        };
        let err = skill.rules(&cur, &finished).unwrap_err();
        assert!(err.to_string().contains("renders"), "{err}");
    }

    #[test]
    fn config_without_instructions_is_rejected() {
        let err = serde_json::from_value::<ChildSkillConfig>(serde_json::json!({
            "tools": ["read_file"]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("instructions"), "{err}");
        let cfg: ChildSkillConfig =
            serde_json::from_value(serde_json::json!({"instructions": "look"})).unwrap();
        assert_eq!(cfg.max_steps, 30);
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn done_without_summary_is_rejected() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: String::new(),
            ..ReportWorkspace::default()
        };
        let err = skill.rules(&cur, &next).unwrap_err();
        assert!(err.to_string().contains("summary"), "{err}");
    }

    #[test]
    fn done_blocked_without_findings_or_reason_is_rejected() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: "could not proceed".to_string(),
            outcome: ReportOutcome::Blocked,
            ..ReportWorkspace::default()
        };
        assert!(skill.rules(&cur, &next).is_err());
    }

    #[test]
    fn done_blocked_with_data_reason_passes() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: "could not proceed".to_string(),
            outcome: ReportOutcome::Blocked,
            data: serde_json::json!({"reason": "no access to the sandbox"})
                .as_object()
                .cloned()
                .unwrap(),
            ..ReportWorkspace::default()
        };
        assert!(skill.rules(&cur, &next).is_ok());
    }

    #[test]
    fn done_blocked_with_findings_passes() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            done: true,
            summary: "could not proceed".to_string(),
            outcome: ReportOutcome::Blocked,
            findings: vec!["missing config".to_string()],
            ..ReportWorkspace::default()
        };
        assert!(skill.rules(&cur, &next).is_ok());
    }

    #[test]
    fn summary_over_1200_chars_is_rejected() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            summary: "x".repeat(1201),
            ..ReportWorkspace::default()
        };
        let err = skill.rules(&cur, &next).unwrap_err();
        assert!(err.to_string().contains("1200"), "{err}");
    }

    #[test]
    fn summary_at_1200_chars_is_accepted() {
        let skill = ChildSkill::new("child", ChildSkillConfig::default()).unwrap();
        let cur = committed(ReportWorkspace::default());
        let next = ReportWorkspace {
            summary: "x".repeat(1200),
            ..ReportWorkspace::default()
        };
        assert!(skill.rules(&cur, &next).is_ok());
    }

    #[test]
    fn invalid_data_schema_makes_new_fail() {
        let cfg = cfg_with_schema(serde_json::json!({"type": 12}));
        let result = ChildSkill::new("bad", cfg);
        assert!(result.is_err(), "expected an error for an invalid schema");
    }

    #[test]
    fn accessors_expose_config() {
        let cfg = ChildSkillConfig {
            instructions: "hi".to_string(),
            max_steps: 7,
            guide: Some("ui-review".to_string()),
            budgets: Some(ChildBudgets {
                total: 100,
                observation: 10,
                augmentation: 5,
            }),
            ..ChildSkillConfig::default()
        };
        let skill = ChildSkill::new("child", cfg).unwrap();
        assert_eq!(skill.max_steps(), 7);
        assert_eq!(skill.guide(), Some("ui-review"));
        assert_eq!(skill.budgets().unwrap().total, 100);
    }

    #[test]
    fn spec_includes_instructions_and_harness_paragraph() {
        let cfg = ChildSkillConfig {
            instructions: "Build the app and take a screenshot.".to_string(),
            ..ChildSkillConfig::default()
        };
        let skill = ChildSkill::new("ui-review", cfg).unwrap();
        let text = skill.spec().instructions().to_string();
        assert!(text.contains("Build the app and take a screenshot."));
        assert!(text.contains("subagent"));
        assert!(text.contains("done: true"));
    }

    #[test]
    fn name_tools_and_terminal() {
        let cfg = ChildSkillConfig {
            tools: vec!["shell".to_string(), "read_file".to_string()],
            ..ChildSkillConfig::default()
        };
        let skill = ChildSkill::new("ui-review", cfg).unwrap();
        assert_eq!(skill.name(), "ui-review");
        assert_eq!(
            skill.tools(),
            vec!["shell".to_string(), "read_file".to_string()]
        );
        assert!(!skill.allows_yield());
        assert!(!skill.allows_ask_user());
        assert!(skill.allowed_delegates().is_empty());
        assert!(skill.allowed_transitions().is_empty());

        let not_done = ReportWorkspace::default();
        assert!(!skill.is_terminal(&not_done));
        let done = ReportWorkspace {
            done: true,
            ..ReportWorkspace::default()
        };
        assert!(skill.is_terminal(&done));
    }

    // Keep FileNote import exercised: report.rs re-exports it and a config
    // author may want it when building test fixtures for `files`.
    #[allow(dead_code)]
    fn _keep_file_note(f: FileNote) -> FileNote {
        f
    }
}
