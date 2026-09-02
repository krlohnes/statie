//! Integration tests for `statie-tui`'s driver engine: headless run against
//! `ToySkill` + `MockModel`, a handoff between two locally-defined skills,
//! and cancellation mid-step.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, MemoryLog, RawOutput, SandboxRoot, Schema, Session, SessionConfig,
    SessionLog, Skill, SkillSpec, StepSink, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet,
};
use statie_models::{MockModel, MockTurn};
use statie_patch::JsonMergePatch;
use statie_skills::{ToySkill, ToyWorkspace};
use statie_store::{InMemoryStore, SqliteStore};
use statie_tui::{DynSkill, RunOptions, SkillRegistry, run_headless};

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

fn config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

/// A `Model` wrapper around `Arc<MockModel>` so tests can keep an `Arc`
/// handle to introspect `prompts()`/`calls()` after the model has been
/// moved (boxed) into a `Session` (mirrors `skills/tests/toy_loop.rs`).
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

/// A registry backed by a fixed set of `Arc<dyn Skill<...>>` for a single
/// workspace type `W`.
struct MapRegistry<W> {
    skills: HashMap<String, DynSkill<W>>,
}

impl<W> MapRegistry<W>
where
    W: Clone
        + serde::Serialize
        + serde::de::DeserializeOwned
        + schemars::JsonSchema
        + Send
        + Sync
        + 'static,
{
    fn new(skills: Vec<DynSkill<W>>) -> Self {
        let mut map = HashMap::new();
        for s in skills {
            map.insert(s.name().to_string(), s);
        }
        MapRegistry { skills: map }
    }
}

impl<W: Send + Sync> SkillRegistry<W> for MapRegistry<W> {
    fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    fn get(&self, name: &str) -> Option<DynSkill<W>> {
        self.skills.get(name).cloned()
    }
}

fn make_session(model: Arc<MockModel>, cfg: SessionConfig) -> Session<ToyWorkspace> {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    Session::new(
        Box::new(InMemoryStore::<ToyWorkspace>::new()),
        log,
        Box::new(ArcModel(model)),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
}

#[tokio::test]
async fn headless_toy_run_completes_and_stays_flat() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 1"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 1")
        .with_usage(101, 11),
        MockTurn::new(
            json!({
                "patch": {"counter": 2, "recent_summary": "step 2"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 2"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 2")
        .with_usage(102, 12),
        MockTurn::new(
            json!({
                "patch": {"counter": 3, "recent_summary": "step 3"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 3"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 3")
        .with_usage(103, 13),
        MockTurn::new(json!({"patch": {"counter": 3}, "action": {"type": "done"}}).to_string())
            .with_reasoning("thinking 4"),
    ];

    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(3));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let mut out: Vec<u8> = Vec::new();
    let summary = run_headless(session, registry, opts, &mut out)
        .await
        .unwrap();

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[reasoning]"),
        "expected a [reasoning] line in headless output:\n{printed}"
    );

    assert_eq!(
        summary.steps, 4,
        "expected 4 steps, got {}: {printed}",
        summary.steps
    );
    assert_eq!(summary.final_outcome, "done");
    assert_eq!(summary.prompt_tokens.len(), 3);
    // 3 of the 4 model-calling steps scripted usage; the `done` turn did not.
    assert_eq!(
        summary.server_prompt_tokens,
        vec![101, 102, 103],
        "expected one server_prompt_tokens entry per usage-bearing mock turn"
    );
    assert_eq!(summary.server_completion_tokens, vec![11, 12, 13]);
    let max = *summary.prompt_tokens.iter().max().unwrap();
    let min = *summary.prompt_tokens.iter().min().unwrap();
    assert!(
        max - min <= 8,
        "prompt token series should stay flat: {:?}",
        summary.prompt_tokens
    );
    assert_eq!(summary.final_state["agent"]["counter"], json!(3));

    // Reasoning must never reach the model's prompts.
    for p in model.prompts() {
        assert!(
            !p.system.contains("thinking"),
            "system leaked reasoning: {}",
            p.system
        );
        assert!(
            !p.user.contains("thinking"),
            "user leaked reasoning: {}",
            p.user
        );
    }
}

// --- Handoff test: two locally-defined skills, A hands off to B. ---

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct HandoffWorkspace {
    #[serde(default)]
    notes: Vec<String>,
    #[serde(default)]
    done: bool,
}

struct SkillA {
    spec: Arc<SkillSpec>,
}

impl SkillA {
    fn new() -> Self {
        SkillA {
            spec: Arc::new(SkillSpec::new(
                "a",
                "Skill A: set notes to [\"handoff\"] then done.",
            )),
        }
    }
}

impl Skill for SkillA {
    type Workspace = HandoffWorkspace;
    type Patch = JsonMergePatch<HandoffWorkspace>;

    fn name(&self) -> &str {
        "a"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        HandoffWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        vec!["b".to_string()]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        if s.notes.iter().any(|n| n == "handoff") {
            Some("b".to_string())
        } else {
            None
        }
    }
}

struct SkillB {
    spec: Arc<SkillSpec>,
}

impl SkillB {
    fn new() -> Self {
        SkillB {
            spec: Arc::new(SkillSpec::new("b", "Skill B: terminal immediately.")),
        }
    }
}

impl Skill for SkillB {
    type Workspace = HandoffWorkspace;
    type Patch = JsonMergePatch<HandoffWorkspace>;

    fn name(&self) -> &str {
        "b"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        HandoffWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        // Terminal immediately: the loop hits `is_terminal` before ever
        // calling the model for skill B.
        true
    }
}

fn make_handoff_session(model: Arc<MockModel>, cfg: SessionConfig) -> Session<HandoffWorkspace> {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    Session::new(
        Box::new(InMemoryStore::<HandoffWorkspace>::new()),
        log,
        Box::new(ArcModel(model)),
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
}

#[tokio::test]
async fn headless_handoff_between_two_skills() {
    let root = tempfile::tempdir().unwrap();

    // One turn: patch notes to ["handoff"] with action done. `handoff()`
    // fires on the *next* step's pre-model check (before is_terminal), so
    // after this commits, the following tick sees `handoff` and switches to
    // "b" without calling the model again.
    let turns = vec![MockTurn::new(
        json!({
            "patch": {"notes": ["handoff"]},
            "action": {"type": "done"},
            "ack_inbox": [1],
        })
        .to_string(),
    )];

    let model = Arc::new(MockModel::new(turns));
    let session = make_handoff_session(Arc::clone(&model), config(&root));

    let a: DynSkill<HandoffWorkspace> = Arc::new(SkillA::new());
    let b: DynSkill<HandoffWorkspace> = Arc::new(SkillB::new());
    let registry: Arc<dyn SkillRegistry<HandoffWorkspace>> = Arc::new(MapRegistry::new(vec![a, b]));

    let opts = RunOptions {
        initial_skill: "a".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let mut out: Vec<u8> = Vec::new();
    let summary = run_headless(session, registry, opts, &mut out)
        .await
        .unwrap();

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[activated] b"),
        "expected an activation of skill b in output:\n{printed}"
    );
    assert_eq!(summary.final_outcome, "done");
    assert_eq!(
        summary.final_state["harness"]["activation"],
        json!("b"),
        "final state should show activation b:\n{printed}"
    );
}

#[tokio::test]
async fn cancel_arrives_as_parked_event_within_timeout() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string())
            .with_delay(Duration::from_secs(10)),
    ];
    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));
    let cancel = session.cancel_handle();

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(100));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let driver = tokio::spawn(statie_tui::run_driver(
        session,
        registry,
        opts,
        cmd_rx,
        ev_tx,
        cancel.clone(),
    ));

    cmd_tx
        .send(statie_tui::DriverCmd::User("x".to_string()))
        .await
        .unwrap();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let found = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Parked(msg)) => return msg,
                Some(_) => continue,
                None => panic!("driver event channel closed before a Parked event arrived"),
            }
        }
    })
    .await
    .expect("a Parked event should arrive within 2s");

    assert_eq!(found, "cancelled");

    drop(cmd_tx);
    let _ = driver.await;
}

/// A *fatal* driver failure (here: `initial_skill` names a skill the
/// registry doesn't have -- a configuration bug, not something retrying can
/// fix) must surface as `Err` from `run_headless`, never as an empty
/// successful summary. Model errors are recoverable (see
/// `step_error_is_reported_and_driver_keeps_running`) and are no longer
/// fatal, so this test uses an unknown-skill scenario instead.
#[tokio::test]
async fn headless_surfaces_driver_errors() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![]));
    let session = make_session(model, config(&root));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![
        Arc::new(ToySkill::new(3)) as DynSkill<ToyWorkspace>,
    ]));
    let opts = RunOptions {
        initial_skill: "no-such-skill".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 50,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let mut out: Vec<u8> = Vec::new();
    let result = run_headless(session, registry, opts, &mut out).await;
    let err = match result {
        Err(e) => e.to_string(),
        Ok(summary) => panic!("expected an error, got summary steps={}", summary.steps),
    };
    assert!(err.contains("no-such-skill"), "err = {err}");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("[error]"), "output = {text}");
}

/// A recoverable step error (the model errors, e.g. an unreachable server)
/// is surfaced as an `Error` event followed by a `Parked` event, and the
/// driver keeps running: a later command still produces a `StepDone`, and
/// `Quit` still ends the task cleanly. This is the fix for the original bug
/// -- a model/network error used to propagate with `?` and silently end the
/// driver task, making the TUI look like it crashed on Enter.
#[tokio::test]
async fn step_error_is_reported_and_driver_keeps_running() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::error("io error: error sending request for url (http://localhost:11434/)"),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(100));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    cmd_tx
        .send(statie_tui::DriverCmd::User("hi".to_string()))
        .await
        .unwrap();

    // Collect events until Parked; an Error event containing "step failed"
    // must have arrived first, and the driver task must still be alive
    // (not returned) afterward.
    let mut saw_error = false;
    let saw_parked = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Error(msg)) => {
                    assert!(
                        msg.contains("step failed"),
                        "unexpected error message: {msg}"
                    );
                    saw_error = true;
                }
                Some(statie_tui::UiEvent::Parked(_)) => return true,
                Some(_) => continue,
                None => panic!("driver event channel closed before a Parked event arrived"),
            }
        }
    })
    .await
    .expect("a Parked event should arrive within 2s");
    assert!(saw_parked);
    assert!(saw_error, "expected an Error event before Parked");
    assert!(
        !driver.is_finished(),
        "driver task should still be running after a recoverable step error"
    );

    // Send another message; the driver is still alive and should process it
    // normally, producing a StepDone.
    cmd_tx
        .send(statie_tui::DriverCmd::User("again".to_string()))
        .await
        .unwrap();

    let saw_step_done = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::StepDone { .. }) => return true,
                Some(_) => continue,
                None => panic!("driver event channel closed before a StepDone event arrived"),
            }
        }
    })
    .await
    .expect("a StepDone event should arrive within 2s");
    assert!(saw_step_done);

    cmd_tx.send(statie_tui::DriverCmd::Quit).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .expect("driver task should return within 2s after Quit");
    assert!(
        matches!(result, Ok(Ok(()))),
        "driver task should return Ok(()): {result:?}"
    );
}

/// When the model parks with `yield` and has set a `recent_summary` in its
/// patch, the `Parked` message should explain that nothing is being asked
/// and surface the summary so the user knows what happened.
#[tokio::test]
async fn yield_park_message_includes_recent_summary() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1, "recent_summary": "did the thing"},
            "action": {"type": "yield"},
            "ack_inbox": [1],
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(100));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    cmd_tx
        .send(statie_tui::DriverCmd::User("go".to_string()))
        .await
        .unwrap();

    let found = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Parked(msg)) => return msg,
                Some(_) => continue,
                None => panic!("driver event channel closed before a Parked event arrived"),
            }
        }
    })
    .await
    .expect("a Parked event should arrive within 2s");

    assert!(found.contains("paused"), "message = {found}");
    assert!(found.contains("did the thing"), "message = {found}");
    assert!(found.contains("continue"), "message = {found}");

    drop(cmd_tx);
    let _ = driver.await;
}

/// When the model parks with `ask_user`, the `Parked` message should include
/// the question and make clear that replying resumes the run.
#[tokio::test]
async fn ask_user_park_message_includes_question() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1},
            "action": {"type": "ask_user", "message": "Which file?"},
            "ack_inbox": [1],
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(100));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    cmd_tx
        .send(statie_tui::DriverCmd::User("go".to_string()))
        .await
        .unwrap();

    let found = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Parked(msg)) => return msg,
                Some(_) => continue,
                None => panic!("driver event channel closed before a Parked event arrived"),
            }
        }
    })
    .await
    .expect("a Parked event should arrive within 2s");

    assert!(found.contains("question: Which file?"), "message = {found}");
    assert!(found.contains("reply"), "message = {found}");

    drop(cmd_tx);
    let _ = driver.await;
}

/// `RunOptions::banner` lines are sent as `Info` events before the first
/// `Activated` event, so a UI can show config/model/sandbox context before
/// any activity.
#[tokio::test]
async fn banner_lines_are_emitted_before_activation() {
    let root = tempfile::tempdir().unwrap();
    let turns = vec![MockTurn::new(
        json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let session = make_session(Arc::clone(&model), config(&root));

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(100));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: vec!["a".to_string(), "b".to_string()],
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    let first = ev_rx.recv().await.expect("first event");
    let second = ev_rx.recv().await.expect("second event");
    let third = ev_rx.recv().await.expect("third event");

    assert!(
        matches!(first, statie_tui::UiEvent::Info(ref s) if s == "a"),
        "{first:?}"
    );
    assert!(
        matches!(second, statie_tui::UiEvent::Info(ref s) if s == "b"),
        "{second:?}"
    );
    assert!(
        matches!(third, statie_tui::UiEvent::Activated(_)),
        "{third:?}"
    );

    drop(cmd_tx);
    let _ = driver.await;
}

fn make_handoff_session_sqlite(
    db_path: &std::path::Path,
    log: Arc<dyn SessionLog>,
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> Session<HandoffWorkspace> {
    let store = SqliteStore::<HandoffWorkspace>::open(db_path).expect("open sqlite store");
    Session::new(
        Box::new(store),
        log,
        Box::new(ArcModel(model)),
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
}

/// Resuming (`RunOptions::resume = true`) against a session that was left
/// activated in the *second* skill of a two-skill handoff must: (1) send a
/// `Replay` event, before the first `Activated`, carrying recent log
/// entries (including the earlier `User` entry) and naming the stored
/// skill; (2) actually start the new driver run in that stored skill, not
/// `initial_skill`.
///
/// `InMemoryStore` can't be shared across two `Session`s (it's not
/// `Clone`/`Arc`-based), so this uses a `SqliteStore` opened from the same
/// on-disk file in both phases -- exactly how the real CLI persists state
/// across two `statie -r` invocations. The `SessionLog` (`MemoryLog`) is
/// shared directly via the same `Arc` across both phases, since only the
/// store needs cross-process-like persistence for this test.
#[tokio::test]
async fn resume_replays_recent_entries_and_starts_in_stored_skill() {
    let root = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("session.db");
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    // --- Phase 1: drive skill "a" to hand off to skill "b". ---
    let turns = vec![MockTurn::new(
        json!({
            "patch": {"notes": ["handoff"]},
            "action": {"type": "done"},
            "ack_inbox": [1],
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let session = make_handoff_session_sqlite(&db_path, Arc::clone(&log), model, config(&root));

    let a: DynSkill<HandoffWorkspace> = Arc::new(SkillA::new());
    let b: DynSkill<HandoffWorkspace> = Arc::new(SkillB::new());
    let registry: Arc<dyn SkillRegistry<HandoffWorkspace>> = Arc::new(MapRegistry::new(vec![a, b]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts = RunOptions {
        initial_skill: "a".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    cmd_tx
        .send(statie_tui::DriverCmd::User("go".to_string()))
        .await
        .unwrap();

    // Wait for activation of "b" (the handoff target).
    let activated_b = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Activated(name)) if name == "b" => return true,
                Some(_) => continue,
                None => panic!("driver event channel closed before activating b"),
            }
        }
    })
    .await
    .expect("skill b should activate within 2s");
    assert!(activated_b);

    cmd_tx.send(statie_tui::DriverCmd::Quit).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .expect("driver task should return within 2s after Quit");
    assert!(matches!(result, Ok(Ok(()))), "phase 1 driver: {result:?}");

    // --- Phase 2: fresh driver over the same persisted state, resuming. ---
    let model2 = Arc::new(MockModel::new(vec![]));
    let session2 = make_handoff_session_sqlite(&db_path, Arc::clone(&log), model2, config(&root));

    let a2: DynSkill<HandoffWorkspace> = Arc::new(SkillA::new());
    let b2: DynSkill<HandoffWorkspace> = Arc::new(SkillB::new());
    let registry2: Arc<dyn SkillRegistry<HandoffWorkspace>> =
        Arc::new(MapRegistry::new(vec![a2, b2]));

    let (cmd_tx2, cmd_rx2) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx2, mut ev_rx2) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let opts2 = RunOptions {
        // Not the stored skill ("b"); resume should override this.
        initial_skill: "a".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: true,
        replay_entries: 40,
        skill_explicit: false,
    };

    let cancel2 = session2.cancel_handle();
    let driver2 = tokio::spawn(statie_tui::run_driver(
        session2, registry2, opts2, cmd_rx2, ev_tx2, cancel2,
    ));

    let first = ev_rx2.recv().await.expect("first event (Replay)");
    let (entries, skill) = match first {
        statie_tui::UiEvent::Replay { entries, skill, .. } => (entries, skill),
        other => panic!("expected Replay as the first event, got {other:?}"),
    };
    assert_eq!(skill, "b", "resume should start in the stored skill");
    assert!(
        entries.iter().any(|e| e.kind == statie_core::LogKind::User),
        "replayed entries should include the earlier User entry: {entries:?}"
    );

    let second = ev_rx2.recv().await.expect("second event (Activated)");
    assert!(
        matches!(second, statie_tui::UiEvent::Activated(ref s) if s == "b"),
        "expected Activated(\"b\") right after Replay, got {second:?}"
    );

    drop(cmd_tx2);
    let _ = driver2.await;
}

/// With `resume: false`, a fresh run never sends a `Replay` event, even if
/// the underlying store happens to have prior state.
#[tokio::test]
async fn fresh_run_sends_no_replay() {
    let root = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("session.db");
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    // Seed some prior state via phase 1, same as the resume test.
    let turns = vec![MockTurn::new(
        json!({
            "patch": {"notes": ["handoff"]},
            "action": {"type": "done"},
            "ack_inbox": [1],
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let session = make_handoff_session_sqlite(&db_path, Arc::clone(&log), model, config(&root));

    let a: DynSkill<HandoffWorkspace> = Arc::new(SkillA::new());
    let b: DynSkill<HandoffWorkspace> = Arc::new(SkillB::new());
    let registry: Arc<dyn SkillRegistry<HandoffWorkspace>> = Arc::new(MapRegistry::new(vec![a, b]));

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();
    let opts = RunOptions {
        initial_skill: "a".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));
    cmd_tx
        .send(statie_tui::DriverCmd::User("go".to_string()))
        .await
        .unwrap();
    let activated_b = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Activated(name)) if name == "b" => return true,
                Some(_) => continue,
                None => panic!("driver event channel closed before activating b"),
            }
        }
    })
    .await
    .expect("skill b should activate within 2s");
    assert!(activated_b);
    cmd_tx.send(statie_tui::DriverCmd::Quit).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .expect("driver task should return within 2s after Quit");

    // Phase 2: fresh (resume: false) driver over the same persisted state.
    let model2 = Arc::new(MockModel::new(vec![]));
    let session2 = make_handoff_session_sqlite(&db_path, Arc::clone(&log), model2, config(&root));
    let a2: DynSkill<HandoffWorkspace> = Arc::new(SkillA::new());
    let b2: DynSkill<HandoffWorkspace> = Arc::new(SkillB::new());
    let registry2: Arc<dyn SkillRegistry<HandoffWorkspace>> =
        Arc::new(MapRegistry::new(vec![a2, b2]));

    let (cmd_tx2, cmd_rx2) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx2, mut ev_rx2) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();
    let opts2 = RunOptions {
        initial_skill: "a".to_string(),
        initial_messages: vec![],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let cancel2 = session2.cancel_handle();
    let driver2 = tokio::spawn(statie_tui::run_driver(
        session2, registry2, opts2, cmd_rx2, ev_tx2, cancel2,
    ));

    // Since `initial_skill` is "a" and skill "a" is not terminal and has no
    // handoff yet (notes empty on this fresh activation's workspace? no --
    // activation only touches harness fields, agent state persists in the
    // store, so `a`'s handoff condition (`notes == ["handoff"]`) is already
    // true from phase 1 and it will hand off to "b" again without a model
    // call). Either way, no Replay event should ever appear.
    let first = ev_rx2.recv().await.expect("first event");
    assert!(
        !matches!(first, statie_tui::UiEvent::Replay { .. }),
        "fresh run (resume: false) should never send Replay, got {first:?}"
    );

    drop(cmd_tx2);
    let _ = driver2.await;
}
