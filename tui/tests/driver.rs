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
use statie_store::InMemoryStore;
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
    W: Clone + serde::Serialize + serde::de::DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static,
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

fn make_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> Session<ToyWorkspace> {
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
    };

    let mut out: Vec<u8> = Vec::new();
    let summary = run_headless(session, registry, opts, &mut out).await.unwrap();

    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[reasoning]"),
        "expected a [reasoning] line in headless output:\n{printed}"
    );

    assert_eq!(summary.steps, 4, "expected 4 steps, got {}: {printed}", summary.steps);
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
        assert!(!p.system.contains("thinking"), "system leaked reasoning: {}", p.system);
        assert!(!p.user.contains("thinking"), "user leaked reasoning: {}", p.user);
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
            spec: Arc::new(SkillSpec::new("a", "Skill A: set notes to [\"handoff\"] then done.")),
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
    let registry: Arc<dyn SkillRegistry<HandoffWorkspace>> =
        Arc::new(MapRegistry::new(vec![a, b]));

    let opts = RunOptions {
        initial_skill: "a".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 200,
    };

    let mut out: Vec<u8> = Vec::new();
    let summary = run_headless(session, registry, opts, &mut out).await.unwrap();

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
    };

    let driver = tokio::spawn(statie_tui::run_driver(
        session, registry, opts, cmd_rx, ev_tx, cancel.clone(),
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

/// A driver failure (here: the mock script is empty, so the model errors on
/// the first step) must surface as `Err` from `run_headless`, never as an
/// empty successful summary.
#[tokio::test]
async fn headless_surfaces_driver_errors() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![]));
    let session = make_session(model, config(&root));
    let registry: Arc<dyn SkillRegistry<ToyWorkspace>> =
        Arc::new(MapRegistry::new(vec![Arc::new(ToySkill::new(3)) as DynSkill<ToyWorkspace>]));
    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["go".to_string()],
        max_autonomous_steps: 50,
    };
    let mut out: Vec<u8> = Vec::new();
    let result = run_headless(session, registry, opts, &mut out).await;
    let err = match result {
        Err(e) => e.to_string(),
        Ok(summary) => panic!("expected an error, got summary steps={}", summary.steps),
    };
    assert!(err.contains("mock script exhausted"), "err = {err}");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("[error]"), "output = {text}");
}
