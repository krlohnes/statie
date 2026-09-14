//! Item 29: a rejected tool call names the skill that has the tool, and
//! repeats count.
//!
//! End-to-end proof, through a real `Session<CodingWorkspace>` with `plan`
//! active, that a scripted `edit_file` call (a tool `plan` does not have,
//! `PlanSkill::tools()`, but `implement` does, `ImplementSkill::tools()`,
//! and `implement` is in `plan`'s own `allowed_transitions()`) is rejected
//! with the exact hand-off observation text `core/src/session.rs`'s
//! `tool_rejected` branch produces from `Skill::skill_with_tool`
//! (`core/src/skill.rs`), routed here through
//! `coding_skill_with_tool`/`PlanSkill::skill_with_tool`
//! (`skills/src/coding/mod.rs`).
//!
//! Also unit-tests `coding_skill_with_tool` directly (not through a
//! session): `("edit_file") == Some("implement")`, `("recall") == None`
//! (shared by every coding skill, so no skill is "the one that has it"),
//! `("") == None` (an empty name is never routed through this function --
//! see `core/src/session.rs`'s own empty-name case).
//!
//! Fixture style copied from `skills/tests/it/error_think.rs`/`router.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools and a scripted
//! `MockModel`, outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `error_think.rs`/
//! `router.rs` inline their own `config`/session builder: `skills/tests/
//! common/mod.rs` exports items this file has no use for, and `-D warnings`
//! fails a binary that leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, PlanSkill, coding_skill_with_tool};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

fn config(root: &tempfile::TempDir) -> SessionConfig {
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

fn make_plan_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tools = default_tools(&ToolConfig::default());
    let session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// A scripted `edit_file` call -- active in `implement`, not in `plan`.
fn edit_turn(path: &str, old: &str, new: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "edit_file",
                "args": {"path": path, "old": old, "new": new},
            },
        })
        .to_string(),
    )
}

fn tool_rejected_payload(log: &Arc<dyn SessionLog>) -> Option<Value> {
    log.entries(Some(LogKind::System), 10_000)
        .into_iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected"))
        .map(|e| e.payload)
}

/// With `plan` active, a scripted `edit_file` call is rejected with the
/// observation naming `implement`, in the exact hand-off shape the brief
/// specifies.
#[tokio::test]
async fn plan_rejected_edit_file_names_implement_with_the_exact_handoff_shape() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![edit_turn("a.txt", "old text", "new text")];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_plan_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let payload = tool_rejected_payload(&log).expect("expected a tool_rejected system log entry");
    assert_eq!(
        payload.get("name").and_then(Value::as_str),
        Some("edit_file")
    );
    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        Some("implement"),
        "plan's allowed_transitions include implement, and implement has edit_file: got \
         {payload:?}"
    );

    let expected = "tool `edit_file` is not available in skill `plan`; `implement` has it. \
                    Finish this skill's job, then hand off: {\"patch\": {\"next_skill\": \
                    \"implement\"}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": \
                    {}}, \"ack_inbox\": []}";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected),
        "the observation must be the exact hand-off text the brief specifies"
    );
}

/// `coding_skill_with_tool` unit tests: `("edit_file") == Some("implement")`;
/// `("recall") == None` (shared by every coding skill -- `plan`,
/// `implement`, and `review` all carry it, so no single skill is "the one
/// that has it"); `("") == None`.
#[test]
fn coding_skill_with_tool_mapping() {
    assert_eq!(coding_skill_with_tool("edit_file"), Some("implement"));
    assert_eq!(coding_skill_with_tool("write_file"), Some("implement"));
    assert_eq!(coding_skill_with_tool("revert_file"), Some("implement"));
    assert_eq!(coding_skill_with_tool("test"), Some("implement"));
    assert_eq!(coding_skill_with_tool("try_fixes"), Some("implement"));
    assert_eq!(coding_skill_with_tool("shell"), Some("implement"));
    assert_eq!(coding_skill_with_tool("probe"), Some("implement"));

    assert_eq!(coding_skill_with_tool("recall"), None);
    assert_eq!(coding_skill_with_tool(""), None);
    assert_eq!(coding_skill_with_tool("no-such-tool"), None);
}
