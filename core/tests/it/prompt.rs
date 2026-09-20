use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, AgentState, ApplyFn, Augmentation, Budgets, Capabilities, Commitment, CommittedState,
    Event, HarnessState, HarnessView, HeuristicTokenizer, InboxItem, LogEntry, LogKind,
    MaintainOutput, Maintainer, MemoryLog, MockMaintainer, Model, ModelError, NullSink, OverBudget,
    Pinned, PromptBuilder, Promptable, RawOutput, SandboxRoot, Schema, Session, SessionConfig,
    SessionLog, SessionLogExt, Skill, SkillSpec, StateStore, StepOutput, StepSink, StoreError,
    SystemPrompt, ToolSet, Version, WorkingSet, WorkingSetConfig, restrict_action_types,
};

fn budgets() -> Budgets {
    Budgets {
        total: 1_000_000,
        observation: 50,
        augmentation: 50,
        notes: 400,
        working_set: 1_000_000,
    }
}

fn system_prompt() -> SystemPrompt {
    SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        None,
        &[],
    )
}

fn harness_state() -> HarnessState {
    HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: budgets(),
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
        consecutive_continues: 0,
        last_failure_summary: None,
        last_skip: None,
        last_tool_error: None,
        last_stall: None,
        last_tool: None,
        fails_since_green: 0,
        notes_step: None,
    }
}

// The Observation-over-budget scenario is exercised as an in-crate unit
// test in `core/src/prompt.rs` because `Observation`'s constructors are
// `pub(crate)` and cannot be reached from an integration test.

#[test]
fn total_over_budget_reports_a_section() {
    let tok = HeuristicTokenizer;
    let b = Budgets {
        total: 5,
        observation: 1_000_000,
        augmentation: 1_000_000,
        notes: 1_000_000,
        working_set: 1_000_000,
    };
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);

    let result = PromptBuilder::new(&tok, b, Version(0))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build();

    let err: OverBudget = result.expect_err("expected OverBudget");
    assert!(err.tokens > err.limit);
    assert_eq!(err.limit, 5);
}

#[test]
fn within_budget_prompt_reports_tokens_and_version() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);

    let prompt = PromptBuilder::new(&tok, b, Version(7))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build()
        .expect("should be within budget");

    assert!(prompt.tokens() > 0);
    assert_eq!(prompt.version(), Version(7));
}

#[test]
fn sections_render_in_fixed_order_regardless_of_call_order() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);
    let augmentation = Augmentation::default();

    // Call builder methods in reverse order.
    let prompt = PromptBuilder::new(&tok, b, Version(0))
        .augmentation(&augmentation)
        .harness(&harness_view)
        .agent(&agent_state)
        .tools(&tools)
        .spec(&spec)
        .system(&sys)
        .build()
        .expect("within budget");

    let system_text = prompt.system();
    let sys_idx = system_text.find("## System").expect("System heading");
    let spec_idx = system_text.find("## Spec").expect("Spec heading");
    let tools_idx = system_text.find("## Tools").expect("Tools heading");
    assert!(sys_idx < spec_idx);
    assert!(spec_idx < tools_idx);

    let user_text = prompt.user();
    let agent_idx = user_text
        .find("## Agent State")
        .expect("Agent State heading");
    let harness_idx = user_text
        .find("## Harness View")
        .expect("Harness View heading");
    let aug_idx = user_text
        .find("## Augmentation")
        .expect("Augmentation heading");
    assert!(agent_idx < harness_idx);
    assert!(harness_idx < aug_idx);
}

#[test]
fn harness_view_renders_only_unacked_inbox_items() {
    let mut harness = harness_state();
    harness.inbox = vec![
        InboxItem {
            id: 1,
            text: "acked message".to_string(),
            acked: true,
        },
        InboxItem {
            id: 2,
            text: "unacked message".to_string(),
            acked: false,
        },
    ];
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(rendered.contains("unacked message"));
    assert!(!rendered.contains("[1] acked message"));
    // The rendered inbox listing must not carry an entry for id 1 at all.
    assert!(!rendered.contains("[1]"));
}

#[test]
fn harness_view_renders_last_observation_when_set() {
    let mut harness = harness_state();
    harness.last_overflow = Some("step 3: `echo` output overflowed".to_string());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        rendered.contains("last_observation: step 3: `echo` output overflowed"),
        "expected a last_observation line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_last_observation_when_none() {
    let harness = harness_state();
    assert!(harness.last_overflow.is_none());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        !rendered.contains("last_observation:"),
        "expected no last_observation line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_renders_pinned_when_set() {
    let mut harness = harness_state();
    harness.step = 4;
    harness.pinned = Some(Pinned {
        name: "ui-review".to_string(),
        expires_at_step: 10,
    });
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        rendered.contains("pinned: ui-review (6 steps left)"),
        "expected a pinned line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_pinned_when_none() {
    let harness = harness_state();
    assert!(harness.pinned.is_none());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        !rendered.contains("pinned:"),
        "expected no pinned line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_renders_commitment_with_reads_left() {
    let harness = harness_state();
    let commitment = Commitment {
        tool: "edit_file".to_string(),
        path: Some("a.rs".to_string()),
        what: "replace `old` with `new`".to_string(),
    };
    let view = HarnessView::new(&harness).with_commitment(Some((commitment, 2)));
    let rendered = view.render();
    assert!(
        rendered.contains(
            "next: edit_file a.rs — replace `old` with `new` (2 reads left before you must act)"
        ),
        "expected a next: line with reads-left, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_next_line_when_no_commitment() {
    let harness = harness_state();
    let view = HarnessView::new(&harness).with_commitment(None);
    let rendered = view.render();
    assert!(
        !rendered.contains("next:"),
        "expected no next: line, got:\n{rendered}"
    );
}

#[test]
fn for_harness_omits_delegate_text_when_no_delegates() {
    let sys = system_prompt();
    let rendered = sys.render();
    assert!(!rendered.contains("\"type\": \"delegate\""));
    assert!(!rendered.contains("delegate`"));
}

/// `SystemPrompt::for_harness` given a schema whose `action.type` enum is
/// `["tool", "done"]` (the shape `restrict_action_types` produces for a
/// skill with `allows_ask_user() == false` and `allows_yield() == false`,
/// e.g. `RouterSkill`, item 28): the "Exact shapes" block must contain no
/// `ask_user` example line, and must still contain the `tool` and `done`
/// example lines.
#[test]
fn for_harness_with_tool_and_done_only_schema_omits_ask_user_example() {
    let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
    restrict_action_types(&mut schema, &["tool", "done"]);

    let sys = SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        Some(&schema),
        &[],
    );
    let rendered = sys.render();

    assert!(
        !rendered.contains(r#""type": "ask_user""#),
        "expected no ask_user example shape, got:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "{\"patch\": {...}, \"action\": {\"type\": \"tool\", \"name\": \"list_files\", \"args\": {\"path\": \".\"}, \"message\": \"\"}, \"ack_inbox\": [1]}"
        ),
        "expected the tool example shape, got:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "{\"patch\": {...}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": {}, \"message\": \"\"}, \"ack_inbox\": []}"
        ),
        "expected the done example shape, got:\n{rendered}"
    );
}

#[test]
fn for_harness_includes_delegate_text_when_delegates_given() {
    let sys = SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        None,
        &["ui-review".to_string()],
    );
    let rendered = sys.render();
    assert!(rendered.contains("\"type\": \"delegate\""));
    assert!(rendered.contains("ui-review"));
}

#[test]
fn working_set_section_renders_between_harness_view_and_augmentation() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);
    let augmentation = Augmentation::default();

    let log = MemoryLog::new();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::Action,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": "a.rs"}}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::ToolOutput,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"name": "read_file", "output": "1: fn a() {}", "truncated": false}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 2,
        kind: LogKind::Action,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"type": "tool", "name": "search", "args": {"pattern": "fn"}}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 2,
        kind: LogKind::ToolOutput,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"name": "search", "output": "a.rs:1: fn a() {}", "truncated": false}),
        agent: None,
    })
    .unwrap();

    let cfg = WorkingSetConfig::default();
    let ws =
        WorkingSet::from_log(&log, &tok, b.working_set, &cfg, 3).expect("expected a working set");

    let prompt = PromptBuilder::new(&tok, b, Version(0))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .working_set(Some(&ws))
        .augmentation(&augmentation)
        .build()
        .expect("within budget");

    let user_text = prompt.user();
    let harness_idx = user_text
        .find("## Harness View")
        .expect("Harness View heading");
    let ws_idx = user_text
        .find("## working set")
        .expect("working set heading");
    let aug_idx = user_text
        .find("## Augmentation")
        .expect("Augmentation heading");
    assert!(harness_idx < ws_idx);
    assert!(ws_idx < aug_idx);
    assert!(
        user_text
            .contains("working set (what you read recently; derived from the log, not state):")
    );
}

// ---------------------------------------------------------------------
// Unit 43: the notes section's heading carries ` (as of step {n})` once a
// maintainer call has committed notes. `Notes` has no public constructor
// (`core/src/promptable.rs`'s `Notes::new` is `pub(crate)`), so the only way
// to reach a real one from this integration test binary is to drive a full
// `Session` through a committed maintainer call, the same minimal scaffold
// `core/tests/it/maintainer.rs` uses (copied in spirit, per that file's own
// header comment about `core/tests/delegate.rs`/`core/tests/user_patch.rs`
// -- core has no test-only shared crate).
// ---------------------------------------------------------------------

struct Inner<W> {
    initial: CommittedState<W>,
    current: CommittedState<W>,
    version: Version,
    log: Vec<statie_core::LoggedPatch>,
}

struct TestStore<W> {
    inner: Mutex<Option<Inner<W>>>,
}

impl<W> TestStore<W> {
    fn new() -> Self {
        TestStore {
            inner: Mutex::new(None),
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
        inner.log.push(statie_core::LoggedPatch {
            version: new_version,
            patch: raw,
            action: action.clone(),
            harness,
        });
        Ok(new_version)
    }

    async fn patch_log(&self) -> Result<Vec<statie_core::LoggedPatch>, StoreError> {
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

struct RecordingModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl RecordingModel {
    fn new(turns: Vec<String>) -> Arc<RecordingModel> {
        Arc::new(RecordingModel {
            turns: Mutex::new(turns),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for RecordingModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        self.prompts.lock().unwrap().push(prompt.user().to_string());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    next: Option<Commitment>,
    #[serde(default)]
    done: bool,
}

struct MergePatch;

impl statie_core::PatchStrategy<Workspace> for MergePatch {
    type Patch = Value;

    fn name() -> &'static str {
        "merge"
    }

    fn describe() -> &'static str {
        "A JSON merge patch over the top-level workspace fields."
    }

    fn apply(cur: &Workspace, patch: &Value) -> Result<Workspace, statie_core::PatchError> {
        let mut cur_value = serde_json::to_value(cur)
            .map_err(|e| statie_core::PatchError::Malformed(e.to_string()))?;
        if let (Value::Object(cur_map), Value::Object(patch_map)) = (&mut cur_value, patch) {
            for (k, v) in patch_map {
                cur_map.insert(k.clone(), v.clone());
            }
        } else {
            return Err(statie_core::PatchError::Malformed(
                "patch must be an object".to_string(),
            ));
        }
        serde_json::from_value(cur_value)
            .map_err(|e| statie_core::PatchError::Malformed(e.to_string()))
    }
}

/// A skill with `notes` harness-owned, active tool `bump`.
struct MaintainedSkill;

impl Skill for MaintainedSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "maintained"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "maintained",
            "a skill for notes-heading tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["bump".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn notes(&self, ws: &Workspace) -> Option<String> {
        if ws.notes.is_empty() {
            None
        } else {
            Some(ws.notes.clone())
        }
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn notes_heading_session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 1000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn bump_tool_call() -> Value {
    serde_json::json!({"type": "tool", "name": "bump", "args": {}})
}

/// Unit 43 spec item 3: once a maintainer call has committed notes, the
/// prompt's notes section heading is exactly
/// "notes (kept by the harness from your previous reasoning) (as of step
/// {n})", where `{n}` is `HarnessState.notes_step` -- the step whose
/// maintainer call produced the notes text now shown, not the step the
/// notes were actually committed at (unit 43: the maintainer for step N is
/// drained and committed only after step N+1 commits).
#[tokio::test]
async fn notes_heading_carries_the_maintainer_step_once_committed() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    // Three scripted turns: step 1 (bump), step 2 (bump, drains+commits
    // step 1's maintainer), step 3 (bump, whose prompt is built AFTER step
    // 2's drain committed step 1's notes -- this is the prompt asserted on).
    let model = RecordingModel::new(vec![
        serde_json::json!({"patch": {"value": 1}, "action": bump_tool_call(), "ack_inbox": []})
            .to_string(),
        serde_json::json!({"patch": {"value": 2}, "action": bump_tool_call(), "ack_inbox": []})
            .to_string(),
        serde_json::json!({"patch": {"value": 3}, "action": bump_tool_call(), "ack_inbox": []})
            .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![
        Ok(MaintainOutput {
            notes: "notes from step 1".to_string(),
            next: None,
        }),
        Ok(MaintainOutput {
            notes: "notes from step 2".to_string(),
            next: None,
        }),
    ]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(HeuristicTokenizer),
        notes_heading_session_config(&root),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Each worker turn's own commit bumps `HarnessState.step` by one, and
    // (once a maintainer is draining) the drained maintainer's own notes
    // patch bumps it again within that same `step()` call -- so the raw
    // step counter does not equal the worker's turn number once a
    // maintainer is configured; only `notes_step` (asserted below) is what
    // this test actually cares about.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let (s1, _) = active.session().state().await.unwrap();
    assert_eq!(
        s1.harness.notes_step, None,
        "no maintainer call has been drained yet after the first worker turn"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (s2, _) = active.session().state().await.unwrap();
    assert_eq!(
        s2.harness.notes_step,
        Some(1),
        "the second worker turn's own step() call drains and commits the \
         first turn's maintainer result before spawning its own"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.notes_step,
        Some(2),
        "the third worker turn's own step() call drains and commits the \
         second turn's maintainer result"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts[2]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 1)"),
        "expected the notes heading to carry ` (as of step 1)`:\n{}",
        prompts[2]
    );
    assert!(
        prompts[2].contains("notes from step 1"),
        "expected step 1's notes text in step 3's prompt:\n{}",
        prompts[2]
    );
    assert!(
        !prompts[1].contains("(as of step"),
        "step 2's prompt is built before any maintainer result has been \
         drained, so its notes heading must carry no step suffix:\n{}",
        prompts[1]
    );
}
