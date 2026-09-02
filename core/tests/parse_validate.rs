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
            last_overflow: None,
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
    let content =
        "```json\n{\"patch\":{\"value\":7},\"action\":{\"type\":\"yield\"},\"ack_inbox\":[]}\n```";
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

/// An unrecognized `action.type` is no longer a hard parse error: it is
/// normalized to a tool call named after that string (see
/// `unknown_action_type_is_treated_as_tool_name`), since local models
/// routinely send the tool name as the action type.
#[test]
fn wrong_action_tag_is_normalized_to_a_tool_call() {
    let content = r#"{"patch":{"value":5},"action":{"type":"not_a_real_action"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).expect("should parse");
    match parsed.action() {
        Action::Tool { name, .. } => assert_eq!(name, "not_a_real_action"),
        other => panic!("expected Tool action, got {other:?}"),
    }
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
    let step = skill
        .validate(&cur, parsed)
        .expect("tool calls bypass inbox ack check");
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
        fn name(&self) -> &str {
            "t"
        }
        fn spec(&self) -> Arc<SkillSpec> {
            Arc::new(SkillSpec::new("t", ""))
        }
        fn initial_workspace(&self) -> Workspace {
            Workspace {
                value: 0,
                recent_summary: String::new(),
            }
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn allowed_transitions(&self) -> Vec<String> {
            vec![
                "review".to_string(),
                "plan".to_string(),
                "router".to_string(),
            ]
        }
        fn is_terminal(&self, s: &Workspace) -> bool {
            s.value >= 10
        }
        fn handoff(&self, s: &Workspace) -> Option<String> {
            (s.value == 7).then(|| "next".to_string())
        }
    }
    let skill = TerminalAtTen;
    let cur = CommittedState {
        harness: HarnessState::default(),
        agent: skill.initial_workspace(),
    };
    let parse = |value: i64| {
        let content = format!(r#"{{"patch":{{"value":{value}}},"action":{{"type":"done"}}}}"#);
        ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(RawOutput::new(
            Version(0),
            None,
            content,
        ))
        .unwrap()
    };
    let err = skill
        .validate(&cur, parse(3))
        .expect_err("non-terminal done must be rejected");
    assert!(
        matches!(err, PatchError::RuleViolation(ref m) if m.contains("`done`")),
        "{err}"
    );
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        _ => unreachable!(),
    };
    let expected_transitions = format!(
        "{:?}",
        vec![
            "review".to_string(),
            "plan".to_string(),
            "router".to_string()
        ]
    );
    assert!(msg.contains(&expected_transitions), "{msg}");
    assert!(msg.contains("`t`"), "{msg}");
    assert!(
        skill.validate(&cur, parse(10)).is_ok(),
        "terminal done is accepted"
    );
    assert!(
        skill.validate(&cur, parse(7)).is_ok(),
        "handoff done is accepted"
    );
}

/// `yield` must be rejected when a skill opts out of it via `allows_yield`,
/// with a message naming the skill and its allowed transitions.
#[test]
fn yield_is_rejected_when_skill_disallows_it() {
    struct NoYieldSkill;
    impl Skill for NoYieldSkill {
        type Workspace = Workspace;
        type Patch = ReplaceStrategy;
        fn name(&self) -> &str {
            "a"
        }
        fn spec(&self) -> Arc<SkillSpec> {
            Arc::new(SkillSpec::new("a", ""))
        }
        fn initial_workspace(&self) -> Workspace {
            Workspace {
                value: 0,
                recent_summary: String::new(),
            }
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn allowed_transitions(&self) -> Vec<String> {
            vec!["b".to_string()]
        }
        fn is_terminal(&self, _s: &Workspace) -> bool {
            false
        }
        fn allows_yield(&self) -> bool {
            false
        }
    }
    let skill = NoYieldSkill;
    let cur = CommittedState {
        harness: HarnessState::default(),
        agent: skill.initial_workspace(),
    };
    let content = r#"{"patch":{"value":1},"action":{"type":"yield"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("yield must be rejected when allows_yield is false");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`a`"), "{msg}");
    assert!(
        msg.contains(&format!("{:?}", vec!["b".to_string()])),
        "{msg}"
    );
}

/// The toy skill keeps the default `allows_yield() -> true`: a `yield` with
/// an acked inbox still validates.
#[test]
fn yield_is_allowed_by_default() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"yield"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let step = skill
        .validate(&cur, parsed)
        .expect("yield should be allowed by default");
    assert!(matches!(step.action(), Action::Yield));
}

// ---------------------------------------------------------------------
// Normalization of malformed `action` shapes from a 26B local model.
// ---------------------------------------------------------------------

/// `{"type":"shell","args":{"cmd":"ls"}}` -- the model used the tool name
/// as the action type. Normalized to a `Tool` action named `shell`, and the
/// fix is reported.
#[test]
fn unknown_action_type_is_treated_as_tool_name() {
    let content =
        r#"{"patch":{"value":1},"action":{"type":"shell","args":{"cmd":"ls"}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let (parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    match parsed.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "shell");
            assert_eq!(args, &serde_json::json!({"cmd": "ls"}));
        }
        other => panic!("expected Tool action, got {other:?}"),
    }
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    assert!(fixes[0].contains("shell"), "{fixes:?}");
}

/// Same malformed shape but with no `args` key at all: normalization fills
/// in an empty object rather than leaving it missing.
#[test]
fn unknown_action_type_without_args_gets_empty_args() {
    let content = r#"{"patch":{"value":1},"action":{"type":"shell"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let (parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    match parsed.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "shell");
            assert_eq!(args, &serde_json::json!({}));
        }
        other => panic!("expected Tool action, got {other:?}"),
    }
    assert_eq!(fixes.len(), 1, "{fixes:?}");
}

/// `{"args":{"path":"calc"},"type":"tool"}` -- a tool call with no `name`
/// at all is unrecoverable, but the rejection message should speak in
/// model terms rather than surfacing serde's raw "missing field" text.
#[test]
fn tool_action_without_name_has_clear_message() {
    let content =
        r#"{"patch":{"value":1},"action":{"args":{"path":"calc"},"type":"tool"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("needs a \"name\""), "{msg}");
}

/// A well-formed output triggers no normalization: the fixes list is empty.
#[test]
fn well_formed_actions_report_no_fixes() {
    let content = r#"{"patch":{"value":5},"action":{"type":"tool","name":"x"},"ack_inbox":[1]}"#;
    let raw = raw_output(Version(3), content);
    let (_parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    assert!(fixes.is_empty(), "{fixes:?}");
}

// ---------------------------------------------------------------------
// JSON repair: outputs cut off or slightly malformed at the end.
// ---------------------------------------------------------------------

/// The observed live shape (shortened): the model's output was cut off
/// right after the outermost object's closing brace was due, leaving
/// `ack_inbox` stranded inside `action` instead of at the top level. This
/// must be repaired (close the one unclosed bracket) and normalized (hoist
/// `ack_inbox` back out), producing a valid `write_file` tool action with
/// `ack_inbox == []` at the top level, and both fixes reported.
#[test]
fn unclosed_object_is_repaired_and_reported() {
    let content = r#"{"patch": {"value": 5, "recent_summary": "x"}, "action": {"type": "tool", "name": "write_file", "args": {"content": "a\nb", "path": "f"}, "ack_inbox": []}"#;
    let raw = raw_output(Version(0), content);
    let (parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");

    match parsed.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert_eq!(
                args.get("content").and_then(serde_json::Value::as_str),
                Some("a\nb")
            );
            assert_eq!(
                args.get("path").and_then(serde_json::Value::as_str),
                Some("f")
            );
        }
        other => panic!("expected Tool action, got {other:?}"),
    }
    assert_eq!(
        parsed.ack_inbox(),
        &[] as &[u64],
        "ack_inbox should have been hoisted to the top level"
    );

    assert!(
        fixes.iter().any(|f| f.contains("closed")),
        "expected a bracket-repair fix, got {fixes:?}"
    );
    assert!(
        fixes.iter().any(|f| f.contains("ack_inbox")),
        "expected an ack_inbox-hoisting fix, got {fixes:?}"
    );
}

/// A cutoff that lands in the middle of a string value (e.g. a
/// `write_file` whose `content` was truncated mid-character) must NOT be
/// silently repaired by closing the string -- that would accept truncated
/// file content as if it were complete. It stays a hard parse error.
#[test]
fn cutoff_inside_string_is_still_rejected() {
    let content = r#"{"patch": {}, "action": {"type": "tool", "name": "write_file", "args": {"content": "abc"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert!(matches!(err, ParseError::InvalidJson(_)));
}

/// A well-formed output that happens to need no repair reports no
/// bracket-repair fix (the existing `well_formed_actions_report_no_fixes`
/// covers the general "no fixes at all" case; this documents that the
/// repair path specifically is untouched).
#[test]
fn well_formed_output_has_no_repair_fix() {
    let content =
        r#"{"patch":{"value":5},"action":{"type":"tool","name":"x","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let (_parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    assert!(fixes.is_empty(), "{fixes:?}");
}

/// A model that puts a literal (raw, unescaped) newline inside a JSON
/// string -- typically `write_file`/`edit_file` file content -- produces a
/// serde_json "control character" error. This must be repaired by
/// escaping the raw control character(s) found inside strings, not
/// rejected outright.
#[test]
fn raw_newline_inside_string_is_escaped_and_reported() {
    let content = "{\"patch\":{\"value\":1},\"action\":{\"type\":\"tool\",\"name\":\"write_file\",\"args\":{\"content\":\"line one\nline two\",\"path\":\"f\"}},\"ack_inbox\":[]}";
    let raw = raw_output(Version(0), content);
    let (parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    match parsed.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert_eq!(
                args.get("content").and_then(serde_json::Value::as_str),
                Some("line one\nline two")
            );
        }
        other => panic!("expected Tool action, got {other:?}"),
    }
    assert!(
        fixes.iter().any(|f| f.contains("control character")),
        "expected a control-character repair fix, got {fixes:?}"
    );
}

/// Trailing prose after the top-level JSON object (a common local-model
/// habit: emitting the object, then a sentence explaining what it did)
/// must be dropped and re-parsed rather than rejected as "trailing
/// characters".
#[test]
fn trailing_prose_after_object_is_dropped() {
    let content = r#"{"patch":{"value":1},"action":{"type":"yield"},"ack_inbox":[]}
I have now written the file."#;
    let raw = raw_output(Version(0), content);
    let (parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    assert!(matches!(parsed.action(), Action::Yield));
    assert!(
        fixes.iter().any(|f| f.contains("trailing")),
        "expected a trailing-characters repair fix, got {fixes:?}"
    );
}

/// A cutoff whose only problem is an unclosed bracket must not also be
/// mis-detected as having "trailing characters": the unclosed-bracket
/// repair alone should fix it, with no trailing-cut fix reported.
#[test]
fn trailing_text_then_unclosed_is_not_double_repaired_wrongly() {
    let content = r#"{"patch":{"value":1},"action":{"type":"done"}"#;
    let raw = raw_output(Version(0), content);
    let (_parsed, fixes) =
        ParsedOutput::<Workspace>::parse_with_fixes::<ReplaceStrategy>(raw).expect("should parse");
    assert!(
        fixes.iter().any(|f| f.contains("closed")),
        "expected an unclosed-bracket repair fix, got {fixes:?}"
    );
    assert!(
        !fixes.iter().any(|f| f.contains("trailing")),
        "should not report a trailing-characters fix, got {fixes:?}"
    );
}
