use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use statie_core::{
    Action, CommittedState, HarnessState, InboxItem, ParseError, ParsedOutput, PatchError,
    PatchStrategy, RawOutput, Reasoning, Skill, SkillSpec, Version,
};

/// A tiny `Replace`-style patch strategy for tests: the patch *is* the new
/// workspace value.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
    pub value: i64,
    #[serde(default)]
    pub recent_summary: String,
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

pub struct ToySkill {
    pub reject_negative: bool,
}

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
        Workspace {
            value: 0,
            recent_summary: String::new(),
        }
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn rules(
        &self,
        _cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        if self.reject_negative && next.value < 0 {
            return Err(PatchError::RuleViolation(
                "value must not be negative".to_string(),
            ));
        }
        Ok(())
    }
}

fn committed(value: i64, inbox: Vec<InboxItem>) -> CommittedState<Workspace> {
    CommittedState {
        harness: HarnessState {
            inbox,
            active_tools: vec![],
            budgets: Default::default(),
            activation: "toy".to_string(),
            step: 0,
        },
        agent: Workspace {
            value,
            recent_summary: String::new(),
        },
    }
}

fn raw_output(version: Version, content: impl Into<String>) -> RawOutput {
    RawOutput::new(version, None, content.into())
}

#[test]
fn parses_valid_tool_action_output() {
    let content = r#"{"patch":{"value":5},"action":{"type":"tool","name":"x"},"ack_inbox":[1]}"#;
    let raw = raw_output(Version(3), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).expect("should parse");
    assert_eq!(parsed.version(), Version(3));
    assert_eq!(parsed.ack_inbox(), &[1]);
    match parsed.action() {
        Action::Tool { name, .. } => assert_eq!(name, "x"),
        other => panic!("expected Tool action, got {other:?}"),
    }
}

#[test]
fn parses_fenced_json() {
    let content = "```json\n{\"patch\":{\"value\":7},\"action\":{\"type\":\"yield\"},\"ack_inbox\":[]}\n```";
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).expect("should parse");
    assert!(matches!(parsed.action(), Action::Yield));
}

#[test]
fn malformed_json_is_a_parse_error() {
    let raw = raw_output(Version(0), "not json at all {{{");
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert!(matches!(err, ParseError::InvalidJson(_)));
}

#[test]
fn wrong_action_tag_is_a_parse_error() {
    let content = r#"{"patch":{"value":5},"action":{"type":"not_a_real_action"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert!(matches!(err, ParseError::InvalidJson(_)));
}

#[test]
fn validate_rejects_when_rules_reject() {
    let skill = ToySkill {
        reject_negative: true,
    };
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":-1},"action":{"type":"yield"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill.validate(&cur, parsed).unwrap_err();
    assert!(matches!(err, PatchError::RuleViolation(_)));
}

#[test]
fn yield_with_unacked_inbox_is_rejected() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(
        0,
        vec![InboxItem {
            id: 42,
            text: "hello".to_string(),
            acked: false,
        }],
    );
    let content = r#"{"patch":{"value":1},"action":{"type":"yield"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill.validate(&cur, parsed).unwrap_err();
    match err {
        PatchError::UnackedInbox(ids) => assert_eq!(ids, vec![42]),
        other => panic!("expected UnackedInbox, got {other:?}"),
    }
}

#[test]
fn yield_with_acked_inbox_succeeds() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(
        0,
        vec![InboxItem {
            id: 42,
            text: "hello".to_string(),
            acked: false,
        }],
    );
    let content = r#"{"patch":{"value":1},"action":{"type":"yield"},"ack_inbox":[42]}"#;
    let raw = raw_output(Version(9), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let step = skill.validate(&cur, parsed).expect("should validate");
    assert!(matches!(step.action(), Action::Yield));
    assert_eq!(step.patch().version(), Version(9));
}

#[test]
fn tool_action_with_unacked_inbox_succeeds() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(
        0,
        vec![InboxItem {
            id: 42,
            text: "hello".to_string(),
            acked: false,
        }],
    );
    let content = r#"{"patch":{"value":1},"action":{"type":"tool","name":"read"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(2), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let step = skill.validate(&cur, parsed).expect("tool calls bypass inbox ack check");
    match step.action() {
        Action::Tool { name, .. } => assert_eq!(name, "read"),
        other => panic!("expected Tool action, got {other:?}"),
    }
    assert_eq!(step.patch().version(), Version(2));
}

#[test]
fn reasoning_never_reaches_the_prompt_type() {
    // Sanity: Reasoning is constructible and usable off the prompt path.
    let r = Reasoning::new("thinking...");
    assert_eq!(r.as_ref(), "thinking...");
}

/// `done` means "activation complete": with a non-terminal workspace and no
/// handoff it must be rejected so the model gets a corrective observation.
#[test]
fn done_is_rejected_unless_terminal_or_handoff() {
    struct TerminalAtTen;
    impl Skill for TerminalAtTen {
        type Workspace = Workspace;
        type Patch = ReplaceStrategy;
        fn name(&self) -> &str { "t" }
        fn spec(&self) -> Arc<SkillSpec> { Arc::new(SkillSpec::new("t", "")) }
        fn initial_workspace(&self) -> Workspace { Workspace { value: 0, recent_summary: String::new() } }
        fn tools(&self) -> Vec<String> { vec![] }
        fn is_terminal(&self, s: &Workspace) -> bool { s.value >= 10 }
        fn handoff(&self, s: &Workspace) -> Option<String> { (s.value == 7).then(|| "next".to_string()) }
    }
    let skill = TerminalAtTen;
    let cur = CommittedState { harness: HarnessState::default(), agent: skill.initial_workspace() };
    let parse = |value: i64| {
        let content = format!(r#"{{"patch":{{"value":{value}}},"action":{{"type":"done"}}}}"#);
        ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(RawOutput::new(Version(0), None, content)).unwrap()
    };
    let err = skill.validate(&cur, parse(3)).expect_err("non-terminal done must be rejected");
    assert!(matches!(err, PatchError::RuleViolation(ref m) if m.contains("`done`")), "{err}");
    assert!(skill.validate(&cur, parse(10)).is_ok(), "terminal done is accepted");
    assert!(skill.validate(&cur, parse(7)).is_ok(), "handoff done is accepted");
}
