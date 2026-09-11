//! Integration tests for `ChildSkill` (milestone 9's configured child
//! skill): schema rejection with a field-naming message, `done`/`summary`/
//! `findings`/`data.reason` gating, an end-to-end mock-model run to
//! `Outcome::Done`, and a scripted `ask_user` step being rejected because
//! children cannot talk to the user.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, Tool,
    ToolCtx, ToolError, ToolOutputFull, ToolSet,
};
use statie_models::MockModel;
use statie_skills::{ChildBudgets, ChildSkill, ChildSkillConfig};
use statie_store::InMemoryStore;

// Deliberately not `mod common;` here: `skills/tests/common/mod.rs` is
// hardcoded to `Session<ToyWorkspace>`, and this file needs
// `Session<ReportWorkspace>`, so it builds its own small harness below
// rather than pulling in `common` (and its unused-for-this-file helpers).

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the given text back."
    }

    fn args_schema(&self) -> Schema {
        #[derive(schemars::JsonSchema)]
        struct Args {
            #[allow(dead_code)]
            text: String,
        }
        Schema::of::<Args>()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("missing `text`".to_string()))?;
        Ok(ToolOutputFull::new(format!("echo: {text}")))
    }
}

struct ArcModel(Arc<MockModel>);

#[async_trait]
impl statie_core::Model for ArcModel {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.0.step(prompt, schema, sink, cancel).await
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn make_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<statie_core::ReportWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let mut tools = ToolSet::new();
    tools.register(Arc::new(EchoTool));
    let session = Session::new(
        Box::new(InMemoryStore::<statie_core::ReportWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

fn renders_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"renders": {"type": "boolean"}},
        "required": ["renders"]
    })
}

#[test]
fn schema_rejects_wrong_type_naming_the_field() {
    let cfg = ChildSkillConfig {
        instructions: "check the page".to_string(),
        data_schema: Some(renders_schema()),
        ..ChildSkillConfig::default()
    };
    let skill = ChildSkill::new("ui-review", cfg).unwrap();

    let cur = statie_core::CommittedState {
        harness: statie_core::HarnessState::default(),
        agent: statie_core::ReportWorkspace::default(),
    };
    let bad = statie_core::ReportWorkspace {
        done: true,
        summary: "checked".to_string(),
        data: json!({"renders": "yes"}).as_object().cloned().unwrap(),
        ..statie_core::ReportWorkspace::default()
    };

    let err = <ChildSkill as statie_core::Skill>::rules(&skill, &cur, &bad).unwrap_err();
    assert!(err.to_string().contains("renders"), "{err}");

    let good = statie_core::ReportWorkspace {
        done: true,
        summary: "checked".to_string(),
        data: json!({"renders": true}).as_object().cloned().unwrap(),
        ..statie_core::ReportWorkspace::default()
    };
    assert!(<ChildSkill as statie_core::Skill>::rules(&skill, &cur, &good).is_ok());
}

#[test]
fn config_deserializes_from_architecture_example() {
    let json_text = json!({
        "ui-review": {
            "instructions": "Build the app, take a screenshot, and report ...",
            "tools": ["shell", "read_file", "search", "screenshot", "filter_last_tool_call"],
            "guide": "ui-review",
            "data_schema": {
                "type": "object",
                "properties": {
                    "renders": {"type": "boolean"},
                    "issues": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["renders"]
            },
            "budgets": {"total": 12000, "observation": 2000, "augmentation": 800},
            "max_steps": 30
        }
    })
    .to_string();

    let map: std::collections::BTreeMap<String, ChildSkillConfig> =
        serde_json::from_str(&json_text).unwrap();
    let cfg = map.get("ui-review").expect("ui-review stanza");
    assert_eq!(cfg.guide.as_deref(), Some("ui-review"));
    assert_eq!(cfg.max_steps, 30);
    assert_eq!(
        cfg.budgets,
        Some(ChildBudgets {
            total: 12000,
            observation: 2000,
            augmentation: 800
        })
    );
    assert_eq!(
        cfg.tools,
        vec![
            "shell".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "screenshot".to_string(),
            "filter_last_tool_call".to_string(),
        ]
    );

    let skill = ChildSkill::new("ui-review", cfg.clone()).unwrap();
    assert_eq!(skill.max_steps(), 30);
    assert_eq!(skill.guide(), Some("ui-review"));
}

/// A minimal child config (an empty stanza with only `instructions` set)
/// still round-trips: every other field defaults.
#[test]
fn minimal_config_stanza_round_trips() {
    let json_text = json!({"instructions": "just do it"}).to_string();
    let cfg: ChildSkillConfig = serde_json::from_str(&json_text).unwrap();
    assert_eq!(cfg.instructions, "just do it");
    assert!(cfg.tools.is_empty());
    assert!(cfg.guide.is_none());
    assert!(cfg.data_schema.is_none());
    assert!(cfg.budgets.is_none());
    assert_eq!(cfg.max_steps, 30);
    assert!(cfg.rules.is_empty());

    ChildSkill::new("child", cfg).expect("valid config should construct");
}

/// End-to-end: a child with tools `["echo"]` takes one tool step, then a
/// `done` step with a valid report; the session reports `Outcome::Done` and
/// the final workspace carries the `summary`/`data` the model sent.
#[tokio::test]
async fn end_to_end_run_reaches_done_with_reported_summary_and_data() {
    use statie_models::MockTurn;

    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"recent_summary": "looked at the page"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "checking"}},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {
                    "summary": "the login page renders correctly",
                    "outcome": "done",
                    "data": {"renders": true, "issues": []},
                    "done": true
                },
                "action": {"type": "done"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), session_config(&root));

    let cfg = ChildSkillConfig {
        instructions: "check the login page".to_string(),
        tools: vec!["echo".to_string()],
        data_schema: Some(renders_schema()),
        ..ChildSkillConfig::default()
    };
    let skill: Arc<ChildSkill> = Arc::new(ChildSkill::new("ui-review", cfg).unwrap());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active
        .step(
            Event::User("check the login page".to_string()),
            &mut NullSink,
        )
        .await
        .unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Done), "{outcome2:?}");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.summary, "the login page renders correctly");
    assert!(state.agent.done);
    assert_eq!(
        state.agent.data.get("renders").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(model.calls(), 2);
}

/// A scripted `ask_user` step is rejected (children cannot talk to the
/// user); the run continues to the next scripted turn instead of parking.
#[tokio::test]
async fn scripted_ask_user_is_rejected_and_run_continues() {
    use statie_models::MockTurn;

    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"recent_summary": "trying to ask"},
                "action": {"type": "ask_user", "message": "what should I do?"},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {
                    "summary": "could not reach the user so I finished with what I had",
                    "outcome": "blocked",
                    "data": {"reason": "needed input only the parent can give"},
                    "done": true
                },
                "action": {"type": "done"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), session_config(&root));

    let cfg = ChildSkillConfig {
        instructions: "check the login page".to_string(),
        ..ChildSkillConfig::default()
    };
    let skill: Arc<ChildSkill> = Arc::new(ChildSkill::new("ui-review", cfg).unwrap());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active
        .step(
            Event::User("check the login page".to_string()),
            &mut NullSink,
        )
        .await
        .unwrap();
    let entries1 = rec1.entries().to_vec();
    let outcome1 = log.append_record(rec1).unwrap();
    // The ask_user attempt is rejected as a rule violation, not accepted as
    // a park: the loop keeps going (`Continue`), not `Parked(AskUser(_))`.
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");
    assert!(
        !matches!(outcome1, Outcome::Parked(Park::AskUser(_))),
        "ask_user must not be allowed to park a child"
    );

    let rejected = entries1
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("rejected")
        })
        .expect("expected a rejected system log entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("ask_user"),
        "rejection reason should mention ask_user: {reason}"
    );

    // The run continues to the next scripted turn and reaches Done.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Done), "{outcome2:?}");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done);
    assert_eq!(state.agent.outcome, statie_core::ReportOutcome::Blocked);

    assert_eq!(
        model.calls(),
        2,
        "both scripted turns should have been used"
    );

    // `Action` import exercised for readability of expected shapes above.
    let _ = |a: Action| a;
}
