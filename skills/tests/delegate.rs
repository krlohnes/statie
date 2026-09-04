//! End-to-end tests for `ConfiguredChildRunner` (milestone 9's `delegate`
//! executor) driven entirely by a single `MockModel` script shared between
//! the parent and its child, consumed in call order: parent delegates,
//! child runs its own turns, parent continues once the child's `Report` is
//! absorbed.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, CommittedState, Event, HeuristicTokenizer, LogEntry, LogKind, MemoryLog,
    Model, NullSink, Outcome, Park, PatchError, RawOutput, Report, SandboxRoot, Schema, Session,
    SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StepSink, Tool, ToolCtx, ToolError,
    ToolOutputFull, ToolSet, Version,
};
use statie_models::{MockModel, MockTurn};
use statie_patch::JsonMergePatch;
use statie_skills::{ChildSkill, ChildSkillConfig, ConfiguredChildRunner};

// ---------------------------------------------------------------------
// Parent workspace/skill: `reports: BTreeMap<String, Report>`,
// `allowed_delegates = ["echo-child"]`, `absorb_report` inserts.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
struct ParentWorkspace {
    #[serde(default)]
    recent_summary: String,
    #[serde(default)]
    reports: BTreeMap<String, Report>,
    #[serde(default)]
    done: bool,
}

struct ParentSkill;

impl Skill for ParentSkill {
    type Workspace = ParentWorkspace;
    type Patch = JsonMergePatch<ParentWorkspace>;

    fn name(&self) -> &str {
        "parent"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "parent",
            "delegate to `echo-child` when asked, then send `done` once its report is in.",
        ))
    }

    fn initial_workspace(&self) -> Self::Workspace {
        ParentWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.done
    }

    fn allowed_delegates(&self) -> Vec<String> {
        vec!["echo-child".to_string()]
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        ws.reports.insert(report.agent.clone(), report);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// `echo` tool, shared by parent's tool set (so `ConfiguredChildRunner`'s
// construction-time tool check passes) and the child's own restricted set.
// ---------------------------------------------------------------------

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

fn tool_set() -> ToolSet {
    let mut ts = ToolSet::new();
    ts.register(Arc::new(EchoTool));
    ts
}

// ---------------------------------------------------------------------
// `ArcModel`: wraps an `Arc<MockModel>` as `Arc<dyn Model>` so the test can
// keep a handle to inspect prompts/calls after the model is shared between
// the parent `Session` and the `ConfiguredChildRunner`.
// ---------------------------------------------------------------------

struct ArcModel(Arc<MockModel>);

#[async_trait]
impl Model for ArcModel {
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
            observation: 1000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

/// Records every `child_started`/`child_step`/`child_finished` call, plus
/// `LogEntry`s aren't captured here (the shared `SessionLog` already has
/// those) -- this is purely for asserting the `StepSink` calls fired.
#[derive(Default)]
struct RecordingSink {
    started: Vec<(String, String)>,
    finished: Vec<(String, statie_core::ReportOutcome)>,
}

impl StepSink for RecordingSink {
    fn reasoning_delta(&mut self, _s: &str) {}
    fn content_delta(&mut self, _s: &str) {}

    fn child_started(&mut self, agent: &str, task: &str) {
        self.started.push((agent.to_string(), task.to_string()));
    }

    fn child_finished(&mut self, agent: &str, report: &Report) {
        self.finished.push((agent.to_string(), report.outcome));
    }
}

fn echo_child_config(max_steps: usize) -> ChildSkillConfig {
    ChildSkillConfig {
        instructions: "echo the task text, then finish.".to_string(),
        tools: vec!["echo".to_string()],
        max_steps,
        ..ChildSkillConfig::default()
    }
}

fn build_runner(
    model: Arc<MockModel>,
    log: Arc<dyn SessionLog>,
    cfg: SessionConfig,
    root: &tempfile::TempDir,
    max_steps: usize,
) -> Arc<ConfiguredChildRunner> {
    let mut children = BTreeMap::new();
    children.insert(
        "echo-child".to_string(),
        Arc::new(ChildSkill::new("echo-child", echo_child_config(max_steps)).unwrap()),
    );
    Arc::new(
        ConfiguredChildRunner::new(
            children,
            Arc::new(ArcModel(model)) as Arc<dyn Model>,
            log,
            tool_set(),
            Box::new(|| Box::new(HeuristicTokenizer)),
            cfg,
            SandboxRoot::new(root.path()).unwrap(),
        )
        .unwrap(),
    )
}

// ---------------------------------------------------------------------
// 1. Happy path: child runs two turns (echo, then done), parent then sends
// done.
// ---------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_delegate_absorbs_child_report_and_parent_continues() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Parent turn 1: delegate to echo-child.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "delegate", "skill": "echo-child", "task": "say hi"},
            })
            .to_string(),
        ),
        // Child turn 1: calls `echo`.
        MockTurn::new(
            json!({
                "patch": {"recent_summary": "echoing"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
            })
            .to_string(),
        ),
        // Child turn 2: sets summary/outcome/done, acks its seeded task
        // inbox item, and sends `done`.
        MockTurn::new(
            json!({
                "patch": {
                    "summary": "said hi",
                    "outcome": "done",
                    "done": true
                },
                "action": {"type": "done"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
        // Parent's next turn: sends `done`.
        MockTurn::new(json!({"patch": {"done": true}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let cfg = session_config(&root);

    let runner = build_runner(
        Arc::clone(&model),
        Arc::clone(&log),
        cfg_clone(&cfg),
        &root,
        30,
    );

    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(Arc::clone(&model))) as Arc<dyn Model>,
        tool_set(),
        Box::new(HeuristicTokenizer),
        cfg,
    )
    .with_child_runner(runner);

    let skill: Arc<ParentSkill> = Arc::new(ParentSkill);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let mut sink = RecordingSink::default();

    // Step 1: parent delegates; the delegate executor drives the child to
    // completion inline and absorbs its report.
    let rec1 = active.step(Event::Tick, &mut sink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    // Step 2: parent sends `done`.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Done), "{outcome2:?}");

    let (state, _version) = active.session().state().await.unwrap();
    let report = state
        .agent
        .reports
        .get("echo-child")
        .expect("report absorbed");
    assert_eq!(report.summary, "said hi");
    assert_eq!(report.stats.steps, 2);

    // Log entries for the child carry `agent == Some("parent/echo-child#1")`
    // while the parent's carry `None`.
    let all_entries = log.entries(None, 10_000);
    let child_entries: Vec<&LogEntry> = all_entries
        .iter()
        .filter(|e| e.agent.as_deref() == Some("parent/echo-child#1"))
        .collect();
    assert!(!child_entries.is_empty(), "expected tagged child entries");
    let parent_entries: Vec<&LogEntry> = all_entries.iter().filter(|e| e.agent.is_none()).collect();
    assert!(
        !parent_entries.is_empty(),
        "expected untagged parent entries"
    );

    // The child's tool call is among the tagged entries.
    assert!(
        child_entries.iter().any(|e| e.kind == LogKind::ToolOutput
            && e.payload.get("name").and_then(Value::as_str) == Some("echo")),
        "{child_entries:?}"
    );

    // `child_started`/`child_finished` fired on the recording sink.
    assert_eq!(sink.started.len(), 1);
    assert_eq!(sink.started[0].0, "parent/echo-child#1");
    assert_eq!(sink.started[0].1, "say hi");
    assert_eq!(sink.finished.len(), 1);
    assert_eq!(sink.finished[0].0, "parent/echo-child#1");
    assert_eq!(sink.finished[0].1, statie_core::ReportOutcome::Done);

    assert_eq!(model.calls(), 4);
}

// ---------------------------------------------------------------------
// 2. Child hits `max_steps: 1` -> report outcome == Capped, parent
// continues.
// ---------------------------------------------------------------------

#[tokio::test]
async fn child_hitting_max_steps_is_capped_and_parent_continues() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Parent turn 1: delegate.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "delegate", "skill": "echo-child", "task": "say hi"},
            })
            .to_string(),
        ),
        // Child turn 1 (its only allowed step, max_steps=1): still working,
        // calls `echo` rather than finishing.
        MockTurn::new(
            json!({
                "patch": {"recent_summary": "still going"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
            })
            .to_string(),
        ),
        // Parent's next turn: continues (e.g. checks the report, then
        // yields) rather than failing outright.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let cfg = session_config(&root);

    let runner = build_runner(
        Arc::clone(&model),
        Arc::clone(&log),
        cfg_clone(&cfg),
        &root,
        1,
    );

    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(Arc::clone(&model))) as Arc<dyn Model>,
        tool_set(),
        Box::new(HeuristicTokenizer),
        cfg,
    )
    .with_child_runner(runner);

    let skill: Arc<ParentSkill> = Arc::new(ParentSkill);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    let (state, _version) = active.session().state().await.unwrap();
    let report = state
        .agent
        .reports
        .get("echo-child")
        .expect("report absorbed");
    assert_eq!(report.outcome, statie_core::ReportOutcome::Capped);
    assert_eq!(report.stats.steps, 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}; parent must continue after a capped child"
    );
}

// ---------------------------------------------------------------------
// 3. Child script sends `ask_user` -> rejected (log shows the rejection),
// child continues with the next scripted turn.
// ---------------------------------------------------------------------

#[tokio::test]
async fn child_ask_user_is_rejected_and_child_continues() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Parent turn 1: delegate.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "delegate", "skill": "echo-child", "task": "say hi"},
            })
            .to_string(),
        ),
        // Child turn 1: tries ask_user, which children cannot do.
        MockTurn::new(
            json!({
                "patch": {"recent_summary": "trying to ask"},
                "action": {"type": "ask_user", "message": "what should I say?"},
            })
            .to_string(),
        ),
        // Child turn 2: finishes normally after the rejection, acking its
        // seeded task inbox item.
        MockTurn::new(
            json!({
                "patch": {
                    "summary": "said hi anyway",
                    "outcome": "done",
                    "done": true
                },
                "action": {"type": "done"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
        // Parent's next turn: done.
        MockTurn::new(json!({"patch": {"done": true}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let cfg = session_config(&root);

    let runner = build_runner(
        Arc::clone(&model),
        Arc::clone(&log),
        cfg_clone(&cfg),
        &root,
        30,
    );

    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(Arc::clone(&model))) as Arc<dyn Model>,
        tool_set(),
        Box::new(HeuristicTokenizer),
        cfg,
    )
    .with_child_runner(runner);

    let skill: Arc<ParentSkill> = Arc::new(ParentSkill);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    let (state, _version) = active.session().state().await.unwrap();
    let report = state
        .agent
        .reports
        .get("echo-child")
        .expect("report absorbed");
    assert_eq!(report.outcome, statie_core::ReportOutcome::Done);
    assert_eq!(report.summary, "said hi anyway");
    // Two committed child steps: the rejected ask_user attempt does not
    // commit, so only the final `done` step counts.
    assert_eq!(report.stats.steps, 1);

    let child_entries: Vec<LogEntry> = log
        .entries(None, 10_000)
        .into_iter()
        .filter(|e| e.agent.as_deref() == Some("parent/echo-child#1"))
        .collect();
    let rejected = child_entries
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("rejected")
        })
        .expect("expected a rejected system entry tagged to the child");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(reason.contains("ask_user"), "{reason}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Done), "{outcome2:?}");
}

/// `SessionConfig` has no `Clone` derive (it holds a `SandboxRoot`, which
/// is `Clone`, but the struct itself isn't); this test file builds two
/// separate consumers (the runner and the parent `Session`) from the same
/// logical config, so this constructs a second, equivalent `SessionConfig`
/// rather than trying to share one value.
fn cfg_clone(cfg: &SessionConfig) -> SessionConfig {
    SessionConfig {
        budgets: cfg.budgets,
        retries: cfg.retries,
        root: cfg.root.clone(),
    }
}

// Exercises `CommittedState`/`Version` imports for readability of assertions
// above without every test needing to construct one directly.
#[allow(dead_code)]
fn _use_committed_state(c: CommittedState<ParentWorkspace>) -> Version {
    Version(c.harness.step)
}
