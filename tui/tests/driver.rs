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
    Budgets, Capabilities, MaintainError, MaintainOutput, MemoryLog, MockMaintainer, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, Skill, SkillSpec, StepSink, Tool,
    ToolCtx, ToolError, ToolOutputFull, ToolSet,
};
use statie_models::{MockModel, MockTurn};
use statie_patch::JsonMergePatch;
use statie_skills::{ToySkill, ToyWorkspace};
use statie_store::{InMemoryStore, SqliteStore};
use statie_tools::{ToolConfig, default_tools};
use statie_tui::{DynSkill, RunOptions, SkillRegistry, run_driver, run_headless};

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
            notes: 400,
            working_set: 6000,
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
        Arc::new(ArcModel(model)),
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
        patches: Vec::new(),
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
    // Step 1 carries the inbox message, later steps carry a tool
    // observation instead; the heuristic tokenizer's rounding on that swap
    // is a few tokens. Anything growing with history would be tens.
    //
    // Since the working set (statie-spec.md "10. Working set") is on by
    // default (`Session::new` starts with `WorkingSetConfig::default()`,
    // and this test's `make_session` never calls `with_working_set`), the
    // "recent calls:" block grows by one line per step until it plateaus
    // at `recent_calls` (6) lines -- this run only reaches 3 model-calling
    // steps, so it never plateaus, and each added recent-call line costs
    // a few dozen tokens under `HeuristicTokenizer`'s char/4 estimate
    // (observed: [714, 756, 766], a 52-token spread). 12 is too tight for
    // that; 80 still catches real regrowth of the *other* sections (the
    // ones this test is actually about) while giving the recent-calls
    // block room to grow across 3 short steps.
    assert!(
        max - min <= 80,
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
        Arc::new(ArcModel(model)),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        Arc::new(ArcModel(model)),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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
        patches: Vec::new(),
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

/// `/patch` (`DriverCmd::Patch`) on a parked session applies directly via
/// `Active::apply_user_patch`, bypassing the model: state changes
/// immediately (no model call for the patch itself), a `patched: keys
/// [...]` `Info` event is emitted, and the *next* prompt's Observation
/// section mentions `operator patched` (a plain mention, not a rejection).
#[tokio::test]
async fn patch_command_on_parked_session_changes_state_and_next_observation_mentions_it() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "did the thing"},
                "action": {"type": "yield"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "yield"}, "ack_inbox": [2]}).to_string(),
        ),
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
        patches: Vec::new(),
    };

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel,
    ));

    // Drive to a parked (yield) state first.
    cmd_tx
        .send(statie_tui::DriverCmd::User("go".to_string()))
        .await
        .unwrap();
    let parked = tokio::time::timeout(Duration::from_secs(2), async {
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
    assert!(parked.contains("paused"), "message = {parked}");

    // Now send the operator patch: bumps `counter` directly, no model call.
    cmd_tx
        .send(statie_tui::DriverCmd::Patch(
            json!({"counter": 42, "notes": ["operator note"]}),
        ))
        .await
        .unwrap();

    let patched_info = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Info(text)) if text.starts_with("patched:") => {
                    return text;
                }
                Some(_) => continue,
                None => panic!("driver event channel closed before a patched Info event arrived"),
            }
        }
    })
    .await
    .expect("a `patched:` Info event should arrive within 2s");
    assert!(patched_info.contains("counter"), "info = {patched_info}");
    assert!(patched_info.contains("notes"), "info = {patched_info}");
    assert!(patched_info.contains("(version"), "info = {patched_info}");

    // Send another message to trigger the next model call, then inspect
    // its recorded prompt's Observation section.
    cmd_tx
        .send(statie_tui::DriverCmd::User("continue".to_string()))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ev_rx.recv().await {
                Some(statie_tui::UiEvent::Parked(msg)) => return msg,
                Some(_) => continue,
                None => panic!("driver event channel closed before a second Parked event arrived"),
            }
        }
    })
    .await
    .expect("a second Parked event should arrive within 2s");

    let prompts = model.prompts();
    assert_eq!(
        prompts.len(),
        2,
        "expected exactly 2 model calls (patch bypassed the model)"
    );
    let obs_idx = prompts[1]
        .user
        .find("## Observation")
        .expect("second prompt should carry an Observation section");
    let heading = "## Observation\n";
    let observation_body = prompts[1].user[obs_idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading");
    assert!(
        observation_body.starts_with("operator patched the workspace: keys"),
        "got: {observation_body}"
    );
    assert!(
        !observation_body.to_lowercase().contains("rejected"),
        "observation should read as a plain mention, not a rejection: {observation_body}"
    );

    drop(cmd_tx);
    let _ = driver.await;
}

// --- Context maintenance rendering (statie-spec.md "9. Context
// maintenance"): the `[maintainer]`/`[maintainer error]` headless lines
// fire from the `StepSink::maintainer_done`/`maintainer_failed` hooks
// after every continued step, regardless of whether the skill's workspace
// actually renders `notes` anywhere. ---

/// Builds a `ToyWorkspace` session, like `make_session`, but with an
/// external `log` handle (so the test can inspect `maintainer_error` system
/// entries after the run) and `maintainer` attached via `with_maintainer`.
fn make_session_with_maintainer(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    log: Arc<dyn SessionLog>,
    maintainer: Arc<dyn statie_core::Maintainer>,
) -> Session<ToyWorkspace> {
    Session::new(
        Box::new(InMemoryStore::<ToyWorkspace>::new()),
        log,
        Arc::new(ArcModel(model)),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
    .with_maintainer(maintainer)
}

/// A `MockMaintainer` scripted to return `Ok` (a plain `notes` string) makes
/// the maintainer's own call succeed, but the patch it then tries to apply
/// -- `{"notes": "<string>"}` -- is rejected: `ToyWorkspace.notes` is typed
/// `Vec<String>`, not `String`, so the JSON merge patch fails to
/// deserialize. Per the task brief, that means only the `[maintainer
/// error]` path is exercised here (not `[maintainer] notes: `), and the
/// rejection reason is asserted instead of a token count.
#[tokio::test]
async fn maintainer_ok_output_against_incompatible_notes_field_surfaces_as_maintainer_error() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 1"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 1"),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the anchor text".to_string(),
        next: None,
    })]));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_session_with_maintainer(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&log),
        Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>,
    );

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(1));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let mut out: Vec<u8> = Vec::new();
    let _summary = run_headless(session, registry, opts, &mut out)
        .await
        .unwrap();

    // The maintainer call itself ran (recorded by the mock).
    assert_eq!(maintainer.calls().len(), 1);

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[maintainer error]"),
        "expected a [maintainer error] line in headless output:\n{printed}"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error")),
        "expected a maintainer_error system log entry"
    );
}

/// A `MockMaintainer` scripted to return `Err` surfaces directly as a
/// `[maintainer error]` headless line and a `maintainer_error` system log
/// entry, with no patch attempt at all.
#[tokio::test]
async fn maintainer_backend_error_surfaces_as_maintainer_error() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 1"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 1"),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));

    let maintainer = Arc::new(MockMaintainer::new(vec![Err(MaintainError::Backend(
        "maintainer backend unreachable".to_string(),
    ))]));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_session_with_maintainer(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&log),
        Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>,
    );

    let skill: DynSkill<ToyWorkspace> = Arc::new(ToySkill::new(1));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let mut out: Vec<u8> = Vec::new();
    let _summary = run_headless(session, registry, opts, &mut out)
        .await
        .unwrap();

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[maintainer error]"),
        "expected a [maintainer error] line in headless output:\n{printed}"
    );
    assert!(
        printed.contains("maintainer backend unreachable"),
        "expected the scripted backend error reason in the output:\n{printed}"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error")),
        "expected a maintainer_error system log entry"
    );
}

// --- Maintainer success path: a local skill whose workspace has a
// `notes: Option<String>` field it declares harness-owned, so a
// `MockMaintainer`'s `{"notes": "<string>"}` patch actually applies. ---

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct NotesWorkspace {
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    recent_summary: String,
    #[serde(default)]
    notes: Option<String>,
}

struct NotesSkill {
    spec: Arc<SkillSpec>,
}

impl NotesSkill {
    fn new() -> Self {
        NotesSkill {
            spec: Arc::new(SkillSpec::new(
                "notes",
                "Notes skill: increments counter, calls echo, then done.",
            )),
        }
    }
}

impl Skill for NotesSkill {
    type Workspace = NotesWorkspace;
    type Patch = JsonMergePatch<NotesWorkspace>;

    fn name(&self) -> &str {
        "notes"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        NotesWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["echo".to_string()]
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.counter >= 1
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn notes(&self, ws: &Self::Workspace) -> Option<String> {
        ws.notes.clone()
    }
}

fn make_notes_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    log: Arc<dyn SessionLog>,
    maintainer: Arc<dyn statie_core::Maintainer>,
) -> Session<NotesWorkspace> {
    Session::new(
        Box::new(InMemoryStore::<NotesWorkspace>::new()),
        log,
        Arc::new(ArcModel(model)),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
    .with_maintainer(maintainer)
}

/// A `MockMaintainer` scripted with a real `notes` string, against a
/// workspace shaped to accept it (`notes: Option<String>`, harness-owned),
/// produces the `[maintainer] notes: N tokens` headless line after the
/// step it follows.
#[tokio::test]
async fn maintainer_success_renders_notes_token_count() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi 1"}},
            })
            .to_string(),
        )
        .with_reasoning("thinking 1"),
    ];
    let model = Arc::new(MockModel::new(turns));

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the anchor text from the reasoning".to_string(),
        next: None,
    })]));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_notes_session(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&log),
        Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>,
    );

    let skill: DynSkill<NotesWorkspace> = Arc::new(NotesSkill::new());
    let registry: Arc<dyn SkillRegistry<NotesWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "notes".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let mut out: Vec<u8> = Vec::new();
    let _summary = run_headless(session, registry, opts, &mut out)
        .await
        .unwrap();

    assert_eq!(maintainer.calls().len(), 1);

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[maintainer] notes: "),
        "expected a [maintainer] notes: line in headless output:\n{printed}"
    );
    assert!(
        !printed.contains("[maintainer error]"),
        "did not expect a [maintainer error] line:\n{printed}"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer")),
        "expected a maintainer system log entry"
    );
}

// =======================================================================
// Green checkpoint at the autonomous step cap (the gap this unit closes):
// the cap is produced by the driver (`run_one_step`'s `Outcome::Continue`
// branch), not `Active::step`/`execute`, so it needs its own call to
// `Active::restore_green_if_stale` to pick up a stale checkpoint. Fixture
// style mirrors `core/tests/green_checkpoint.rs`: a minimal skill with
// `edit_file`/`write_file`/`test` active, real sandboxed tool calls against
// a tempdir, driven with a scripted model.
// =======================================================================

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GreenWorkspace {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    done: bool,
}

struct GreenSkill {
    spec: Arc<SkillSpec>,
}

impl GreenSkill {
    fn new() -> Self {
        GreenSkill {
            spec: Arc::new(SkillSpec::new(
                "green",
                "a skill for the driver-level green checkpoint test",
            )),
        }
    }
}

impl Skill for GreenSkill {
    type Workspace = GreenWorkspace;
    type Patch = JsonMergePatch<GreenWorkspace>;

    fn name(&self) -> &str {
        "green"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        GreenWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "edit_file".to_string(),
            "write_file".to_string(),
            "test".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.done
    }
}

fn green_tool_set() -> ToolSet {
    default_tools(&ToolConfig {
        shell_timeout_secs: 30,
        allow_shell: true,
        max_output_bytes: 200_000,
    })
}

fn make_green_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    log: Arc<dyn SessionLog>,
) -> Session<GreenWorkspace> {
    Session::new(
        Box::new(InMemoryStore::<GreenWorkspace>::new()),
        log,
        Arc::new(ArcModel(model)),
        green_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    )
}

fn green_write_action(content: &str) -> Value {
    json!({
        "type": "tool",
        "name": "write_file",
        "args": {"path": "a.txt", "content": content},
    })
}

fn green_test_action(cmd: &str) -> Value {
    json!({"type": "tool", "name": "test", "args": {"cmd": cmd}})
}

fn green_step_output(action: Value) -> Value {
    json!({"patch": {}, "action": action})
}

/// Runs `run_driver` to completion against a single `DriverCmd::User`
/// kickoff, collecting every `UiEvent` until the driver task ends (the
/// command channel is dropped right after sending, so the driver parks and
/// then blocks on `cmd_rx.recv()` until the sender drop wakes it with
/// `None`, at which point it returns and the spawned task joins).
async fn drive_to_park(
    session: Session<GreenWorkspace>,
    registry: Arc<dyn SkillRegistry<GreenWorkspace>>,
    opts: RunOptions,
) -> Vec<statie_tui::UiEvent> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<statie_tui::DriverCmd>(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<statie_tui::UiEvent>();

    let cancel = session.cancel_handle();
    let driver = tokio::spawn(run_driver(session, registry, opts, cmd_rx, ev_tx, cancel));

    // `initial_messages` in `opts` drives the first activation autonomously
    // to the cap; nothing further needs to be sent. Drop the sender so the
    // driver's command loop (reached only if it parks and waits, which is
    // exactly what the step cap does) ends when the channel closes.
    drop(cmd_tx);

    let mut events = Vec::new();
    while let Some(ev) = ev_rx.recv().await {
        events.push(ev);
    }
    driver.await.unwrap().unwrap();
    events
}

/// The file the model wrote and passed a test against is checkpointed, then
/// broken by a later edit and a failing test; the model keeps returning
/// plain tool actions (further `test` calls, all `Outcome::Continue`) past
/// `max_autonomous_steps`, so the driver's own step-cap park is what has to
/// restore the checkpoint -- `Active::step` never sees a park here at all.
#[tokio::test]
async fn step_cap_restores_green_checkpoint_when_last_test_failed() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(green_step_output(green_write_action("PASS\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(green_step_output(green_write_action("BROKEN\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        // Padding: keep returning `Outcome::Continue`-shaped tool actions
        // (a no-op-ish test rerun) until the cap is hit.
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_green_session(Arc::clone(&model), config(&root), Arc::clone(&log));

    let skill: DynSkill<GreenWorkspace> = Arc::new(GreenSkill::new());
    let registry: Arc<dyn SkillRegistry<GreenWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "green".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 5,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let events = drive_to_park(session, registry, opts).await;

    let parked = events
        .iter()
        .find_map(|e| match e {
            statie_tui::UiEvent::Parked(msg) => Some(msg.clone()),
            _ => None,
        })
        .expect("expected a Parked event");

    assert!(
        parked.contains("autonomous step cap reached (5 steps in a row without pausing)"),
        "parked message must keep the original cap text as a prefix: {parked}"
    );
    assert!(
        parked.contains("Restored the files to the state where the test passed at step"),
        "parked message must carry the restore sentence: {parked}"
    );

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS\n",
        "the file must be restored to the checkpointed (byte-identical) content"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("restored_green")),
        "expected a restored_green entry in the log: {system_entries:?}"
    );
}

/// Same shape, but the last `test` before the cap passes: no restore is
/// needed (disk already matches the checkpoint), so the cap message carries
/// no restore sentence and the newer content is left in place.
#[tokio::test]
async fn step_cap_does_not_restore_when_last_test_passed() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(green_step_output(green_write_action("PASS\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(green_step_output(green_write_action("PASS TOO\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        // Padding: further passing `test` calls, all `Outcome::Continue`.
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_green_session(Arc::clone(&model), config(&root), Arc::clone(&log));

    let skill: DynSkill<GreenWorkspace> = Arc::new(GreenSkill::new());
    let registry: Arc<dyn SkillRegistry<GreenWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "green".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 5,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let events = drive_to_park(session, registry, opts).await;

    let parked = events
        .iter()
        .find_map(|e| match e {
            statie_tui::UiEvent::Parked(msg) => Some(msg.clone()),
            _ => None,
        })
        .expect("expected a Parked event");

    assert!(
        parked.contains("autonomous step cap reached (5 steps in a row without pausing)"),
        "parked message must keep the original cap text as a prefix: {parked}"
    );
    assert!(
        !parked.contains("Restored the files"),
        "parked message must not carry a restore sentence when the last test passed: {parked}"
    );

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS TOO\n",
        "the newer (post-checkpoint) content must be kept"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        !system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("restored_green")),
        "must not restore when the last test passed: {system_entries:?}"
    );
}

/// Same gap as the step-cap tests above, but for the driver's step-ERROR
/// path (`report_step_error`, wired into every `Err(e)` arm in `run_driver`
/// that produces the "step failed; fix the problem ..." parked message): a
/// green checkpoint is taken, broken by a later edit and a failing test,
/// and then the model backend itself fails (`MockTurn::error`, simulating
/// the crash a live run hit) -- `Active::step` returns `Err`, so neither
/// `Active::execute`'s own park-time restores nor the step-cap block ever
/// run. Before this unit, the parked message and the file on disk both
/// stayed stuck at the broken, post-checkpoint state.
#[tokio::test]
async fn step_error_restores_green_checkpoint_when_last_test_failed() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(green_step_output(green_write_action("PASS\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        MockTurn::new(green_step_output(green_write_action("BROKEN\n")).to_string()),
        MockTurn::new(
            green_step_output(green_test_action("sh -c 'grep -q PASS a.txt'")).to_string(),
        ),
        // The model backend itself fails on the step right after the
        // failing test -- simulates the crash a live run hit.
        MockTurn::error("simulated model backend crash"),
    ];
    let model = Arc::new(MockModel::new(turns));

    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = make_green_session(Arc::clone(&model), config(&root), Arc::clone(&log));

    let skill: DynSkill<GreenWorkspace> = Arc::new(GreenSkill::new());
    let registry: Arc<dyn SkillRegistry<GreenWorkspace>> = Arc::new(MapRegistry::new(vec![skill]));

    let opts = RunOptions {
        initial_skill: "green".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 20,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
        patches: Vec::new(),
    };

    let events = drive_to_park(session, registry, opts).await;

    let errors: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            statie_tui::UiEvent::Error(msg) => Some(msg.clone()),
            _ => None,
        })
        .collect();
    assert!(
        errors.iter().any(|msg| msg.contains("step failed")),
        "expected a `step failed: ...` Error event: {errors:?}"
    );

    let parked = events
        .iter()
        .find_map(|e| match e {
            statie_tui::UiEvent::Parked(msg) => Some(msg.clone()),
            _ => None,
        })
        .expect("expected a Parked event");

    assert!(
        parked.contains("step failed"),
        "parked message must keep the original step-failed text: {parked}"
    );
    assert!(
        parked.contains("Restored the files to the state where the test passed at step"),
        "parked message must carry the restore sentence: {parked}"
    );

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS\n",
        "the file must be restored to the checkpointed (byte-identical) content"
    );

    let system_entries = log.entries(Some(statie_core::LogKind::System), 10_000);
    assert!(
        system_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("restored_green")),
        "expected a restored_green entry in the log: {system_entries:?}"
    );
}
