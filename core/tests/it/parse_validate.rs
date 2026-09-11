use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Capabilities, CommittedState, HarnessState, InboxItem, LoggedPatch, Model,
    ModelError, ParseError, ParsedOutput, PatchError, PatchStrategy, RawOutput, Reasoning, Schema,
    Session, SessionConfig, SessionLog, Skill, SkillSpec, StateStore, StepOutput, StepSink,
    StoreError, ToolSet, Version, validate_default,
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
            pinned: None,
            green_step: None,
            green_filtered: false,
            tool_calls: 0,
            last_test_cmd: None,
            edits_since_test: 0,
            last_think_step: None,
            repeated_failure_step: None,
            try_fixes_available: false,
            last_try_fixes_step: None,
            consecutive_non_question_asks: 0,
            last_failure_summary: None,
            last_skip: None,
            last_tool_error: None,
            last_stall: None,
            last_tool: None,
            fails_since_green: 0,
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

/// `Action::Continue` is harness-internal (see its doc in
/// `core/src/model.rs`): unlike an ordinary unrecognized `action.type`
/// (`wrong_action_tag_is_normalized_to_a_tool_call`, just above), a model
/// output naming `"continue"` specifically must NOT be normalized to a tool
/// call named "continue" and must NOT deserialize into `Action::Continue`
/// via `Action`'s own `serde` tag (which would otherwise happily accept it,
/// since it's just another `rename_all = "snake_case"` variant tag) -- it
/// must be rejected outright, with the same "unknown action" style error an
/// actually-unknown type gets.
#[test]
fn model_sending_type_continue_is_rejected_not_normalized() {
    let content = r#"{"patch":{"value":5},"action":{"type":"continue","note":"x"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert!(matches!(err, ParseError::InvalidJson(_)), "got {err:?}");
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

/// The premature-`done` rejection message includes the exact JSON to send
/// next, using the skill's first allowed transition as `next_skill`, so a
/// model that ignores the prose still has a copyable shape to retry with.
#[test]
fn premature_done_message_includes_exact_shape_with_first_allowed_target() {
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
                "implement".to_string(),
                "plan".to_string(),
                "router".to_string(),
            ]
        }
        fn is_terminal(&self, s: &Workspace) -> bool {
            s.value >= 10
        }
    }
    let skill = TerminalAtTen;
    let cur = CommittedState {
        harness: HarnessState::default(),
        agent: skill.initial_workspace(),
    };
    let content = r#"{"patch":{"value":3},"action":{"type":"done"}}"#.to_string();
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(RawOutput::new(
        Version(0),
        None,
        content,
    ))
    .unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("non-terminal, non-handoff done must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        _ => unreachable!("{err}"),
    };
    assert!(msg.contains("Exact shape:"), "{msg}");
    assert!(msg.contains(r#""next_skill": "implement""#), "{msg}");
    assert!(
        msg.contains(
            r#"{"patch": {"next_skill": "implement"}, "action": {"type": "done", "name": "", "args": {}}, "ack_inbox": []}"#
        ),
        "{msg}"
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

// ---------------------------------------------------------------------
// `delegate` action and `ask_user` gating (milestone 9).
// ---------------------------------------------------------------------

/// A skill whose `allowed_delegates()` lists `"ui-review"`, for exercising
/// the `delegate` action's validation.
struct DelegatingSkill;

impl Skill for DelegatingSkill {
    type Workspace = Workspace;
    type Patch = ReplaceStrategy;

    fn name(&self) -> &str {
        "implement"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("implement", ""))
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

    fn is_terminal(&self, _s: &Workspace) -> bool {
        false
    }

    fn allowed_delegates(&self) -> Vec<String> {
        vec!["ui-review".to_string()]
    }
}

#[test]
fn delegate_to_unlisted_child_is_rejected() {
    let skill = DelegatingSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"delegate","skill":"rust-testing","task":"run the tests"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("delegate to an unlisted child must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`delegate`"), "{msg}");
    assert!(msg.contains("`implement`"), "{msg}");
    assert!(
        msg.contains(&format!("{:?}", vec!["ui-review".to_string()])),
        "{msg}"
    );
}

#[test]
fn delegate_to_listed_child_validates() {
    let skill = DelegatingSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"delegate","skill":"ui-review","task":"check the login page"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let step = skill
        .validate(&cur, parsed)
        .expect("delegate to a listed child should validate");
    match step.action() {
        Action::Delegate { skill, task } => {
            assert_eq!(skill, "ui-review");
            assert_eq!(task, "check the login page");
        }
        other => panic!("expected Delegate action, got {other:?}"),
    }
}

/// A `delegate` action with an empty `skill` or `task` (the placeholder
/// value a grammar-constrained decoder can send when it collapses to the
/// shortest branch) must be rejected before the harness ever tries to hand
/// off to a subagent.
#[test]
fn delegate_with_empty_skill_is_rejected() {
    let skill = DelegatingSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"delegate","name":"","args":{},"skill":"","task":"check the page"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("delegate with an empty skill must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert_eq!(
        msg,
        "action `delegate` requires non-empty `skill` and `task`"
    );
}

#[test]
fn delegate_with_empty_task_is_rejected() {
    let skill = DelegatingSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"delegate","name":"","args":{},"skill":"ui-review","task":""},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("delegate with an empty task must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert_eq!(
        msg,
        "action `delegate` requires non-empty `skill` and `task`"
    );
}

/// `delegate` is validated like a parking action for inbox purposes: an
/// unacked inbox item must be rejected the same way `yield`/`ask_user`/
/// `done` are.
#[test]
fn delegate_with_unacked_inbox_is_rejected() {
    let skill = DelegatingSkill;
    let cur = committed(
        0,
        vec![InboxItem {
            id: 1,
            text: "hello".to_string(),
            acked: false,
        }],
    );
    let content = r#"{"patch":{"value":1},"action":{"type":"delegate","skill":"ui-review","task":"check the page"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill.validate(&cur, parsed).unwrap_err();
    assert!(matches!(err, PatchError::UnackedInbox(_)), "{err}");
}

/// A skill that opts out of `ask_user` (children, milestone 9).
struct NoAskUserSkill;

impl Skill for NoAskUserSkill {
    type Workspace = Workspace;
    type Patch = ReplaceStrategy;

    fn name(&self) -> &str {
        "ui-review"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("ui-review", ""))
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

    fn is_terminal(&self, _s: &Workspace) -> bool {
        false
    }

    fn allows_ask_user(&self) -> bool {
        false
    }
}

#[test]
fn ask_user_is_rejected_when_skill_disallows_it() {
    let skill = NoAskUserSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"ask_user","message":"which one?"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("ask_user must be rejected when allows_ask_user is false");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`ask_user`"), "{msg}");
    assert!(msg.contains("`ui-review`"), "{msg}");
}

/// The toy skill keeps the default `allows_ask_user() -> true`: `ask_user`
/// with an acked inbox still validates.
#[test]
fn ask_user_is_allowed_by_default() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"ask_user","message":"which one?"},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let step = skill
        .validate(&cur, parsed)
        .expect("ask_user should be allowed by default");
    assert!(matches!(step.action(), Action::AskUser { .. }));
}

/// The live grammar-decoding failure this schema/validation change fixes:
/// a model sends `ask_user` with an empty (placeholder) `message`.
/// `validate_default` must reject it with a message-specific error rather
/// than the harness silently parking the session on a blank question.
#[test]
fn ask_user_with_empty_message_is_rejected() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"ask_user","name":"","args":{},"message":""},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("ask_user with an empty message must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert_eq!(
        msg,
        "action `ask_user` requires a non-empty `message`: the question for the user"
    );
}

/// A whitespace-only `message` is treated the same as empty (trimmed
/// before the emptiness check).
#[test]
fn ask_user_with_whitespace_only_message_is_rejected() {
    let skill = ToySkill {
        reject_negative: false,
    };
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1},"action":{"type":"ask_user","name":"","args":{},"message":"   "},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("ask_user with a whitespace-only message must be rejected");
    assert!(matches!(err, PatchError::RuleViolation(_)), "{err}");
}

// ---------------------------------------------------------------------
// Harness-owned fields (`Skill::harness_owned_fields`): a model patch
// touching one is rejected regardless of action; an operator patch
// (`Active::apply_user_patch`) touching the same field is accepted.
// ---------------------------------------------------------------------

/// A skill that declares `recent_summary` as harness-owned: the model's
/// patch may not touch it, whatever the patch strategy.
struct HarnessOwnedSkill;

impl Skill for HarnessOwnedSkill {
    type Workspace = Workspace;
    type Patch = ReplaceStrategy;

    fn name(&self) -> &str {
        "owned"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("owned", ""))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace {
            value: 0,
            recent_summary: String::new(),
        }
    }

    fn tools(&self) -> Vec<String> {
        vec!["echo".to_string()]
    }

    fn is_terminal(&self, _s: &Workspace) -> bool {
        false
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["recent_summary".to_string()]
    }
}

#[test]
fn patch_touching_a_harness_owned_field_is_rejected() {
    let skill = HarnessOwnedSkill;
    let cur = committed(0, vec![]);
    let content = r#"{"patch":{"value":1,"recent_summary":"tampered"},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("a patch that writes a harness-owned field must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`recent_summary`"), "{msg}");
    assert!(msg.contains("harness-owned"), "{msg}");
}

#[test]
fn patch_leaving_harness_owned_fields_alone_validates() {
    let skill = HarnessOwnedSkill;
    let cur = committed(0, vec![]);
    let content =
        r#"{"patch":{"value":1},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    skill
        .validate(&cur, parsed)
        .expect("a patch that leaves harness-owned fields alone should validate");
}

/// `ReplaceStrategy` sends the whole workspace on every patch, so the raw
/// patch object always carries the harness-owned key. The check must be
/// value-based (compare committed vs. post-patch), not presence-based, or
/// this would always be rejected -- this is the scenario the previous
/// fixture hack (`skip_serializing_if` on `recent_summary`) was papering
/// over instead of fixing.
#[test]
fn replace_patch_echoing_owned_field_unchanged_validates() {
    let skill = HarnessOwnedSkill;
    let mut cur = committed(0, vec![]);
    cur.agent.recent_summary = "written by the maintainer".to_string();
    let content = r#"{"patch":{"value":1,"recent_summary":"written by the maintainer"},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    skill.validate(&cur, parsed).expect(
        "a replace-strategy patch that carries the owned field's committed value unchanged \
         should validate",
    );
}

/// Same replace-strategy setup, but this time the model actually changes
/// the owned field's value: must still be rejected, with the same message.
#[test]
fn replace_patch_changing_owned_field_is_rejected() {
    let skill = HarnessOwnedSkill;
    let mut cur = committed(0, vec![]);
    cur.agent.recent_summary = "written by the maintainer".to_string();
    let content = r#"{"patch":{"value":1,"recent_summary":"tampered"},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap();
    let err = skill
        .validate(&cur, parsed)
        .expect_err("a replace-strategy patch that changes the owned field must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`recent_summary`"), "{msg}");
    assert!(msg.contains("harness-owned"), "{msg}");
}

// ---------------------------------------------------------------------
// A JSON-merge-patch-style strategy (only the keys present in the patch
// are applied; a `null` deletes/resets a field), used to exercise the
// harness-owned check against a strategy where "untouched" and "present
// in the raw patch object" genuinely differ, unlike `ReplaceStrategy`.
// ---------------------------------------------------------------------

/// Distinguishes an absent `recent_summary` key (outer `None`: leave
/// unchanged) from a key present with value `null` (`Some(None)`: clear
/// it) -- serde's default `Option<T>` deserialization collapses both to
/// `None` unless told otherwise.
fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// A patch that may omit either field (leaving it unchanged) or set
/// `recent_summary` explicitly to `null` (reset to empty, modeling a
/// merge-patch-style deletion).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MergeWorkspacePatch {
    #[serde(default)]
    pub value: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub recent_summary: Option<Option<String>>,
}

pub struct MergeStrategy;

impl PatchStrategy<Workspace> for MergeStrategy {
    type Patch = MergeWorkspacePatch;

    fn name() -> &'static str {
        "merge"
    }

    fn describe() -> &'static str {
        "Only keys present in the patch are applied; `recent_summary: null` clears it."
    }

    fn apply(cur: &Workspace, patch: &MergeWorkspacePatch) -> Result<Workspace, PatchError> {
        let mut next = cur.clone();
        if let Some(value) = patch.value {
            next.value = value;
        }
        if let Some(recent_summary) = &patch.recent_summary {
            next.recent_summary = recent_summary.clone().unwrap_or_default();
        }
        Ok(next)
    }
}

/// A merge patch that sets the owned field to `null` (deletion) is
/// rejected, even though a merge strategy only ever sends the keys it
/// actually means to touch. Calls `validate_default` directly (rather than
/// `skill.validate`) with `MergeStrategy` as the patch strategy, since
/// `HarnessOwnedSkill::Patch` is fixed to `ReplaceStrategy`.
#[test]
fn merge_patch_deleting_owned_field_is_rejected() {
    let skill = HarnessOwnedSkill;
    let mut cur = committed(0, vec![]);
    cur.agent.recent_summary = "written by the maintainer".to_string();
    let content = r#"{"patch":{"recent_summary":null},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<MergeStrategy>(raw).unwrap();
    let err = validate_default::<Workspace, MergeStrategy>(&skill, &cur, parsed)
        .expect_err("a merge patch that deletes (nulls out) the owned field must be rejected");
    let msg = match &err {
        PatchError::RuleViolation(m) => m.clone(),
        other => panic!("expected RuleViolation, got {other:?}"),
    };
    assert!(msg.contains("`recent_summary`"), "{msg}");
    assert!(msg.contains("harness-owned"), "{msg}");
}

/// A merge patch that sets the owned field to its current value is
/// tolerated: a no-op touch is not the same as a change, so the
/// value-based check lets it through.
#[test]
fn merge_patch_setting_owned_field_to_current_value_validates() {
    let skill = HarnessOwnedSkill;
    let mut cur = committed(0, vec![]);
    cur.agent.recent_summary = "written by the maintainer".to_string();
    let content = r#"{"patch":{"recent_summary":"written by the maintainer"},"action":{"type":"tool","name":"echo","args":{}},"ack_inbox":[]}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<MergeStrategy>(raw).unwrap();
    validate_default::<Workspace, MergeStrategy>(&skill, &cur, parsed).expect(
        "a merge patch that sets the owned field to its current value (a no-op touch) should \
         validate",
    );
}

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` and unused `Model`, local to this
// test file (core cannot depend on `statie-store`/`statie-models`), used
// only by `operator_patch_touching_harness_owned_field_is_accepted` below
// to drive `Active::apply_user_patch` -- the operator/maintainer path,
// which does not run the harness-owned-field check that gates model
// patches.
// ---------------------------------------------------------------------

struct Inner<W> {
    initial: CommittedState<W>,
    current: CommittedState<W>,
    version: Version,
    log: Vec<LoggedPatch>,
}

struct TestStore<W> {
    inner: std::sync::Mutex<Option<Inner<W>>>,
}

impl<W> TestStore<W> {
    fn new() -> Self {
        TestStore {
            inner: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl<W> StateStore<W> for TestStore<W>
where
    W: Clone + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .as_ref()
            .map(|inner| (inner.current.clone(), inner.version)))
    }

    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().unwrap();
        if guard.is_some() {
            return Err(StoreError::Backend("already initialized".to_string()));
        }
        let version = Version(0);
        *guard = Some(Inner {
            initial: initial.clone(),
            current: initial,
            version,
            log: Vec::new(),
        });
        Ok(version)
    }

    async fn commit(
        &self,
        patch: statie_core::ValidatedPatch<W>,
        harness: HarnessState,
        action: &Action,
        expect: Version,
    ) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        if patch.version() != expect || expect != inner.version {
            return Err(StoreError::VersionMismatch {
                expected: patch.version(),
                actual: inner.version,
            });
        }
        let (raw, next, _v) = patch.into_parts();
        let new_version = Version(inner.version.0 + 1);
        inner.current.agent = next;
        inner.current.harness = harness.clone();
        inner.version = new_version;
        inner.log.push(LoggedPatch {
            version: new_version,
            patch: raw,
            action: action.clone(),
            harness,
        });
        Ok(new_version)
    }

    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?
            .log
            .clone())
    }

    async fn replay(
        &self,
        to: Version,
        apply: ApplyFn<'_, W>,
    ) -> Result<CommittedState<W>, StoreError> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        let mut agent = inner.initial.agent.clone();
        let mut harness = inner.initial.harness.clone();
        for entry in &inner.log {
            if entry.version > to {
                break;
            }
            agent = apply(&agent, &entry.patch).map_err(|e| StoreError::Backend(e.to_string()))?;
            harness = entry.harness.clone();
        }
        Ok(CommittedState { harness, agent })
    }

    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        inner.current.harness = harness;
        Ok(())
    }
}

struct UnusedModel;

#[async_trait]
impl Model for UnusedModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: false,
            schema_constrained: false,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        _prompt: statie_core::BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        Err(ModelError::Backend(
            "UnusedModel should never be called".to_string(),
        ))
    }
}

/// An operator patch (`/patch`, `Active::apply_user_patch`) touching a
/// harness-owned field is accepted: the harness-owned-field check in
/// `validate_default` only gates model-authored patches
/// (`Skill::validate`); the operator (and, per statie-spec.md "9. Context
/// maintenance", the maintainer) is a trusted writer of those fields.
#[tokio::test]
async fn operator_patch_touching_harness_owned_field_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::new(UnusedModel) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        SessionConfig {
            budgets: Default::default(),
            retries: 3,
            root: statie_core::SandboxRoot::new(root.path()).unwrap(),
        },
    );

    let skill = Arc::new(HarnessOwnedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let new_version = active
        .apply_user_patch(serde_json::json!({"value": 0, "recent_summary": "operator wrote this"}))
        .await
        .expect("an operator patch touching a harness-owned field should be accepted");
    assert_eq!(new_version, Version(1));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.recent_summary, "operator wrote this");
}

// ---------------------------------------------------------------------
// A well-formed `patch` with no `action` at all: with thinking on, the
// server does not enforce the schema, and a bench run sent this shape four
// times in a row, hitting the retry cap on what should have been a
// recoverable step. `ParsedOutput::parse_with_fixes` now parses this as the
// harness-internal `Action::Continue` (patch kept, model told every output
// needs an action) instead of rejecting -- see `core/src/skill.rs`.
// ---------------------------------------------------------------------

/// The exact note text `ParsedOutput::parse_with_fixes` must produce for a
/// `patch`-with-no-`action` output.
const NO_ACTION_NOTE: &str = "your output had no `action`; the patch was kept. Every output \
                               needs an action: a tool call, `done`, or `ask_user`.";

/// A patch strategy whose `Patch` type IS `Value` (mirrors
/// `core/tests/continue_action.rs`'s `MergePatch`): a raw JSON object round
/// trips through it byte-for-byte, so it is the right strategy to prove "the
/// patch value is intact" with, unmuddied by a typed strategy's own
/// round-trip normalization (e.g. `MergeStrategy`'s `MergeWorkspacePatch`
/// adding an explicit `"value": null` for a field the raw JSON never
/// mentioned).
struct RawValueStrategy;

impl PatchStrategy<Workspace> for RawValueStrategy {
    type Patch = serde_json::Value;

    fn name() -> &'static str {
        "raw"
    }

    fn describe() -> &'static str {
        "The patch is an arbitrary JSON value, applied by the test's own merge logic."
    }

    fn apply(cur: &Workspace, patch: &serde_json::Value) -> Result<Workspace, PatchError> {
        let mut cur_value =
            serde_json::to_value(cur).map_err(|e| PatchError::Malformed(e.to_string()))?;
        if let (serde_json::Value::Object(cur_map), serde_json::Value::Object(patch_map)) =
            (&mut cur_value, patch)
        {
            for (k, v) in patch_map {
                cur_map.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

/// `{"patch": {"recent_summary": "x"}}` (no `action` at all) parses to
/// `Action::Continue` with the exact note, and the patch value is intact.
#[test]
fn patch_with_no_action_parses_as_continue_with_exact_note() {
    let content = r#"{"patch":{"recent_summary":"x"}}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<RawValueStrategy>(raw)
        .expect("a patch with no action should parse as Continue, not be rejected");
    match parsed.action() {
        Action::Continue { note } => assert_eq!(note, NO_ACTION_NOTE),
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert_eq!(
        parsed.patch().0,
        serde_json::json!({"recent_summary": "x"}),
        "the patch must be kept intact"
    );
}

/// Same, but with an empty patch object (`{"patch": {}}`): still parses as
/// `Continue`, with the empty patch kept as-is (not treated as "no patch").
#[test]
fn empty_patch_with_no_action_parses_as_continue() {
    let content = r#"{"patch":{}}"#;
    let raw = raw_output(Version(0), content);
    let parsed = ParsedOutput::<Workspace>::parse::<RawValueStrategy>(raw)
        .expect("an empty patch with no action should parse as Continue, not be rejected");
    match parsed.action() {
        Action::Continue { note } => assert_eq!(note, NO_ACTION_NOTE),
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert_eq!(parsed.patch().0, serde_json::json!({}));
}

/// `{}` (no `patch`, no `action`) is NOT recovered as `Continue` -- there is
/// no patch to keep. It keeps the existing "missing field `patch`" rejection
/// (asserted verbatim, since this exact message is part of the contract this
/// change must not disturb).
#[test]
fn completely_empty_object_keeps_existing_missing_patch_error() {
    let raw = raw_output(Version(0), "{}");
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid JSON output: missing field `patch`"
    );
}

/// `{"action": {"type": "done"}}` with no `patch` at all: also not
/// recovered (no `patch` member present), keeps the existing "missing field
/// `patch`" rejection.
#[test]
fn action_done_without_patch_keeps_existing_missing_patch_error() {
    let content = r#"{"action":{"type":"done"}}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid JSON output: missing field `patch`"
    );
}

/// `{"patch": {}, "action": {"type": "continue", "note": "n"}}` -- `action`
/// IS present (even though it names the harness-internal "continue" type),
/// so this must NOT be recovered by the no-`action` path; it keeps falling
/// through to the existing "model sent type continue" rejection.
#[test]
fn patch_with_explicit_continue_action_is_still_rejected() {
    let content = r#"{"patch":{},"action":{"type":"continue","note":"n"}}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw)
        .expect_err("an explicit action.type=continue must still be rejected outright");
    assert!(matches!(err, ParseError::InvalidJson(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "invalid JSON output: unknown action type \"continue\": not a valid Action"
    );
}

/// A `patch` that is present but not an object (e.g. a string) with no
/// `action`: does not match the recovery shape (`patch` must itself be an
/// object), falls through to the ordinary deserialize and its error.
#[test]
fn non_object_patch_with_no_action_is_not_recovered() {
    let content = r#"{"patch":"not an object"}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw)
        .expect_err("a non-object patch must not be recovered as Continue");
    assert!(matches!(err, ParseError::InvalidJson(_)), "{err:?}");
}

/// The schema check on `patch` still applies to the no-`action` recovery
/// path: a `patch` that is a well-formed JSON object but not a valid
/// `Workspace` value (wrong field type) is still a parse error, not silently
/// accepted just because `action` also happens to be missing.
#[test]
fn patch_with_no_action_but_wrong_shaped_patch_is_still_rejected() {
    // `ReplaceStrategy::Patch = Workspace`, whose `value` field is `i64`.
    let content = r#"{"patch":{"value":"not a number"}}"#;
    let raw = raw_output(Version(0), content);
    let err = ParsedOutput::<Workspace>::parse::<ReplaceStrategy>(raw)
        .expect_err("a patch that doesn't match the workspace shape must still fail to parse");
    assert!(matches!(err, ParseError::InvalidJson(_)), "{err:?}");
}

/// The model-facing schema is unchanged: `action` is still a required
/// field in the `StepOutput` JSON Schema advertised to the model (the
/// no-`action` recovery is a harness-side parse-time tolerance, not a
/// relaxation of what the model is told to send).
#[test]
fn model_facing_schema_still_requires_action() {
    let schema = Schema::of::<StepOutput<Workspace>>();
    let value = serde_json::to_value(&schema).expect("schema serializes");
    let required = value
        .get("required")
        .and_then(|r| r.as_array())
        .expect("schema has a top-level `required` array");
    let required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        required.contains(&"action"),
        "`action` must still be required in the model-facing schema: {required:?}"
    );
}
