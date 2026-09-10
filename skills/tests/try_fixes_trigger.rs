//! `implement_repeat_failure_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `strip_malformed_next`), the
//! `try_fixes` half of BACKLOG.md item 4: when a `Fixer` is attached
//! (`HarnessState::try_fixes_available`), the repeated-failure trigger
//! substitutes `try_fixes {}` instead of `think` for the model's next stuck
//! move.
//!
//! This is the end-to-end proof that the substitution actually drives a
//! real `try_fixes` call through `Session::execute_try_fixes`
//! (`core/src/session.rs`) -- the unit tests in `skills/src/coding/
//! tests.rs` cover the `validate`-level substitution in isolation (which
//! tool name gets set), but cannot exercise `Session::with_fixer`/
//! `execute_try_fixes` at all. Fixture style copied from `skills/tests/
//! think_trigger.rs`: a `Session<CodingWorkspace>` wired with the real file
//! tools, a scripted `MockModel`, and outcomes/log entries read straight
//! from the `SessionLog`, plus a `MockFixer` attached via `Session::
//! with_fixer`.
//!
//! Deliberately not `mod common;`, for the same reason `think_trigger.rs`
//! inlines its own `config`/`make_file_tools_session`: `skills/tests/
//! common/mod.rs` exports items this file has no use for, and `-D warnings`
//! fails a binary that leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, Fixer, LogKind, MemoryLog, NullSink, Outcome, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `think_trigger.rs`'s `config`.
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

/// Same shape as `think_trigger.rs`'s `ArcModel`.
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

/// Same shape as `think_trigger.rs`'s `make_file_tools_session`, but
/// attaches `fixer` via `Session::with_fixer` instead of a thinker.
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    fixer: Arc<dyn Fixer>,
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
    )
    .with_fixer(fixer, 3);
    (session, log)
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// A turn that runs action `test` with the given shell command and an
/// empty patch.
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

/// A turn that writes a file (a real edit, so the mutated flag is set and
/// `edits_since_test` climbs).
fn write_file_turn(path: &str, content: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "write_file",
                "args": {"path": path, "content": content},
            },
        })
        .to_string(),
    )
}

/// Finds the most recent `Action` log entry's `name`/`args`. Same as
/// `think_trigger.rs`'s `last_action`.
fn last_action(log: &Arc<dyn SessionLog>) -> (String, Value) {
    let entries = log.entries(Some(LogKind::Action), 10_000);
    let entry = entries.last().expect("an Action entry must exist");
    (
        entry
            .payload
            .get("name")
            .and_then(Value::as_str)
            .expect("action entry must have a name")
            .to_string(),
        entry.payload.get("args").cloned().unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn repeat_failure_substitutes_try_fixes_when_a_fixer_is_attached() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let yield_turn = MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string());

    let turns = vec![
        // Step 1: fail (fresh failure).
        test_turn("false"),
        // Step 2: a real write (mutated flag set).
        write_file_turn("a.txt", "still wrong\n"),
        // Step 3: fail again, identical -- repeated_failure_step gets set
        // to this step.
        test_turn("false"),
        // Step 4: the model tries another edit -- must be substituted for
        // `try_fixes` (a fixer is attached).
        write_file_turn("a.txt", "second attempt\n"),
        // Step 5: read to confirm the substitution note reached the next
        // prompt.
        yield_turn,
    ];
    let model = Arc::new(MockModel::new(turns));
    let fixer = Arc::new(statie_core::MockFixer::new(vec![Err(
        statie_core::FixError::Empty,
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&fixer) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: fail (fresh).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Step 2: write_file (real edit).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (name2, _args2) = last_action(&log);
    assert_eq!(name2, "write_file");

    // Step 3: fail again, identical -- repeated_failure_step must be set.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state, _v) = active.session().state().await.unwrap();
    assert!(
        state.harness.repeated_failure_step.is_some(),
        "the second identical fail must set repeated_failure_step"
    );
    assert!(state.harness.try_fixes_available, "a fixer is attached");
    assert!(!any_rejected(&log));

    // Step 4: the model sends another write_file -- must be substituted for
    // `try_fixes {}` since a fixer is attached; the mock fixer must have
    // been called exactly once.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    assert!(!any_rejected(&log));

    let (name4, args4) = last_action(&log);
    assert_eq!(
        name4, "try_fixes",
        "step 4's write_file must be substituted for try_fixes when a fixer is attached"
    );
    assert_eq!(args4, json!({}));

    assert_eq!(
        fixer.inputs().len(),
        1,
        "the mock fixer must have been called exactly once"
    );

    // `last_try_fixes_step`/`last_think_step` are both set (to the same
    // step), so the once-per-streak rule is shared.
    let (state, _v) = active.session().state().await.unwrap();
    assert!(state.harness.last_try_fixes_step.is_some());
    assert_eq!(
        state.harness.last_try_fixes_step, state.harness.last_think_step,
        "try_fixes must set last_think_step to the same step as last_try_fixes_step"
    );

    // The next prompt's observation carries the substitution note (the
    // note text `implement_repeat_failure_thinks` returns for the `Edit`
    // shape, unchanged by which tool it substituted).
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 5, "expected exactly 5 model calls");
    let fifth_prompt = &prompts[4];
    assert!(
        fifth_prompt
            .user
            .contains("harness asked the reasoning subagent"),
        "expected the repeat substitution note in the 5th prompt:\n{}",
        fifth_prompt.user
    );
}
