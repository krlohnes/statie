use std::collections::HashMap;
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
    SystemPrompt, Tokenizer, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet, Version,
    WorkingSet, WorkingSetConfig, restrict_action_types,
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

// ---------------------------------------------------------------------
// Unit 44: the prompt fits by evicting from the working set (or, failing
// that, truncating the observation) before it fails.
//
// Live evidence (item 44 brief): qwen_json_long_b26, five of ten runs ended
// with "step failed: prompt section System over budget: 14005..14986 >
// 14000" after item 40 added ~800 tokens of tool descriptions to the
// system block. The working set is the one elastic section, sized before
// the other sections are known, so when the fixed sections grow the total
// can overflow on a budget the harness could still meet by evicting from
// its own elastic section. These tests drive a real `Session` (fixture
// copied in spirit from `core/tests/it/evicted_reread.rs`, since core has
// no test-only shared crate) through scripted `read_file` calls that fill
// the working set with real windows, with budgets dialed so a later step's
// prompt exceeds `total` -- via the working set (a), the fixed sections
// alone (b), or the observation (c) -- and checks how `PromptBuilder::
// build()` failing over budget is (or isn't) recovered from.
// ---------------------------------------------------------------------

struct EvictionReadFile {
    files: HashMap<String, String>,
}

impl EvictionReadFile {
    fn new(files: &[(&str, &str)]) -> Self {
        EvictionReadFile {
            files: files
                .iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
        }
    }
}

#[async_trait]
impl Tool for EvictionReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "reads a whole small file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let body = self.files.get(path).cloned().unwrap_or_default();
        Ok(ToolOutputFull::new(body))
    }
}

fn eviction_tool_set(files: &[(&str, &str)]) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(EvictionReadFile::new(files)));
    tools
}

fn read_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": path}})
}

fn eviction_step_output(patch: Value, action: Value) -> String {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []}).to_string()
}

/// A skill whose spec instructions text is caller-supplied, so a test can
/// dial the fixed Spec section's token count precisely.
struct EvictionSkill {
    instructions: String,
}

impl Skill for EvictionSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "eviction"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("eviction", self.instructions.clone()))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["read_file".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn eviction_session_config(root: &tempfile::TempDir, budgets: Budgets) -> SessionConfig {
    SessionConfig {
        budgets,
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn shrink_entries<'a>(rec: &'a statie_core::StepRecord, event: &str) -> Vec<&'a LogEntry> {
    rec.entries()
        .iter()
        .filter(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some(event)
        })
        .collect()
}

/// (a) Fixed sections plus a full working set exceed `total`; the eviction
/// loop shrinks the working set until the prompt fits, logging exactly one
/// `working_set_shrunk` entry whose numbers are asserted exactly, and the
/// prompt actually sent to the model is at or under `total`.
#[tokio::test]
async fn working_set_shrunk_lets_an_otherwise_over_budget_step_build() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tok = HeuristicTokenizer;

    // A single large file, read at step 1; a second, tiny file read at
    // step 2 (its own tool output becomes step 2's prompt observation, not
    // a window -- the working set excludes the single newest tool output).
    // Step 3's prompt is the one under test: its pending observation is
    // step 2's tiny `b.txt` output (well within the small observation
    // budget below), and its working set is built from the log's
    // now-not-newest `a.txt` output -- a real ~1000-token window, rendered
    // just under the working_set budget below so it survives unclamped
    // there. Only the working set's own size drives step 3 over `total`.
    let body = "x".repeat(4000); // 1000 tokens at chars/4 (ceil).
    let files: &[(&str, &str)] = &[("a.txt", &body), ("b.txt", "hi"), ("c.txt", "yo")];

    let model = RecordingModel::new(vec![
        eviction_step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        eviction_step_output(serde_json::json!({}), read_action("b.txt")), // step 2
        eviction_step_output(serde_json::json!({}), read_action("c.txt")), // step 3
    ]);

    // Fixed sections (System/Spec/Tools/AgentState/HarnessView -- no
    // Notes/Situational/Augmentation configured) small enough to fit
    // comfortably under `total` on their own; the observation budget (100
    // tokens) keeps step 2's a.txt-derived observation small (a.txt is
    // truncated to fit it) so step 2 never needs any shrink; the working
    // set (rendered at its full 1050-token budget on step 3, once a.txt's
    // window is no longer the newest output) is what pushes step 3's
    // prompt over `total`.
    let instructions = "small instructions".to_string();
    let budgets = Budgets {
        total: 700,
        observation: 100,
        augmentation: 500,
        notes: 400,
        working_set: 1050,
    };

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        eviction_tool_set(files),
        Box::new(HeuristicTokenizer),
        eviction_session_config(&root, budgets),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    });

    let skill = Arc::new(EvictionSkill { instructions });
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: read a.txt (large). Its output is step 2's pending
    // observation, not yet a working-set window.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec1, "working_set_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: read b.txt (tiny). Its own prompt observation is a.txt's
    // output, clamped on the way in to the 100-token observation budget
    // (well within `total`), and no working set exists yet -- a.txt is
    // still the single newest tool output, excluded from windows -- so
    // this step must build cleanly with no shrink of either kind.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec2, "working_set_shrunk").is_empty());
    assert!(shrink_entries(&rec2, "observation_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: read c.txt. Its prompt's pending observation is b.txt's tiny
    // output, and its working set now renders a.txt's ~1000-token window
    // (b.txt is the newest output, excluded) at up to the full 1050-token
    // working_set budget -- fixed sections plus that window exceed `total`
    // (700); the step must still build a prompt rather than failing, with
    // the working set evicted down to fit.
    let rec3 = active
        .step(Event::Tick, &mut NullSink)
        .await
        .expect("step must succeed by evicting the working set, not fail OverBudget");

    let entries = shrink_entries(&rec3, "working_set_shrunk");
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one working_set_shrunk entry: {:?}",
        rec3.entries()
    );
    let from = entries[0]
        .payload
        .get("from")
        .and_then(Value::as_u64)
        .unwrap();
    let to = entries[0]
        .payload
        .get("to")
        .and_then(Value::as_u64)
        .unwrap();
    let rounds = entries[0]
        .payload
        .get("rounds")
        .and_then(Value::as_u64)
        .unwrap();
    let overage = entries[0]
        .payload
        .get("overage")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(
        from, 1050,
        "expected the shrink to start from the configured working_set budget"
    );
    assert_eq!(
        to,
        from.saturating_sub(overage + 64),
        "`to` must equal the spec's exact formula: budget.saturating_sub(overage + 64) \
         (round 1's plain cut drops the single window entirely and fits, so \
         no halving is needed)"
    );
    assert_eq!(
        rounds, 1,
        "the single window is dropped whole on the first cut, which frees \
         far more than the overage, so exactly one round is needed"
    );
    assert!(overage > 0, "expected a nonzero measured overage");

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    let final_tokens = tok.count(&prompts[2]);
    assert!(
        final_tokens <= budgets.total,
        "expected the rebuilt prompt to fit within total ({}), got {}",
        budgets.total,
        final_tokens
    );
}

/// (b) Fixed sections alone (no working set, no observation) exceed
/// `total`: eviction has nothing to evict, and the step must still fail
/// with the exact `OverBudget` error text naming the largest section.
#[tokio::test]
async fn fixed_sections_alone_over_budget_still_fails() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    let model = RecordingModel::new(vec![eviction_step_output(
        serde_json::json!({}),
        read_action("a.txt"),
    )]);

    // No working set at all (max_windows/recent_calls both zero) and no
    // pending observation on step 1 (nothing has run yet): the fixed
    // sections, inflated by a very long spec instructions string, exceed
    // `total` on their own.
    let instructions = "y".repeat(20_000); // 5000 tokens.
    let budgets = Budgets {
        total: 100,
        observation: 2000,
        augmentation: 500,
        notes: 400,
        working_set: 2000,
    };

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        eviction_tool_set(&[]),
        Box::new(HeuristicTokenizer),
        eviction_session_config(&root, budgets),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 0,
        recent_calls: 0,
    });

    let skill = Arc::new(EvictionSkill { instructions });
    let mut active = session.activate(skill).await.unwrap();

    let result = active.step(Event::Tick, &mut NullSink).await;
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("fixed sections alone over budget must still fail"),
    };

    match err {
        statie_core::SessionError::OverBudget(OverBudget {
            section,
            tokens,
            limit,
        }) => {
            assert_eq!(
                section,
                statie_core::Section::Spec,
                "expected the Spec section (the largest -- it carries the inflated skill \
                 instructions text) to be named"
            );
            assert_eq!(limit, 100);
            assert!(tokens > limit);
        }
        other => panic!("expected SessionError::OverBudget, got {other:?}"),
    }
}

/// (c) The working set is already empty/zero (`max_windows: 0`) and the
/// observation alone overflows: it is truncated with exactly one
/// `observation_shrunk` entry, and the resulting prompt fits.
#[tokio::test]
async fn observation_shrunk_lets_an_otherwise_over_budget_step_build() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tok = HeuristicTokenizer;

    // A large file: its read output becomes step 2's pending observation
    // (clamped to the 2000-token observation budget on the way in), large
    // enough on its own to blow the small `total` even with no working set
    // and small fixed sections.
    let body = "x".repeat(6000); // 1500 tokens at chars/4 (ceil).
    let files: &[(&str, &str)] = &[("a.txt", &body)];

    let model = RecordingModel::new(vec![
        eviction_step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        eviction_step_output(serde_json::json!({}), read_action("a.txt")), // step 2
    ]);

    let instructions = "small instructions".to_string();
    let budgets = Budgets {
        total: 700,
        observation: 2000,
        augmentation: 500,
        notes: 400,
        working_set: 2000,
    };

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        eviction_tool_set(files),
        Box::new(HeuristicTokenizer),
        eviction_session_config(&root, budgets),
    )
    // No working set at all: `max_windows: 0` with `recent_calls: 0`
    // makes `WorkingSet::from_log` return `None` unconditionally, so the
    // eviction loop's working-set stage has nothing to shrink and step 2
    // goes straight to the observation stage.
    .with_working_set(WorkingSetConfig {
        max_windows: 0,
        recent_calls: 0,
    });

    let skill = Arc::new(EvictionSkill { instructions });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let rec2 = active
        .step(Event::Tick, &mut NullSink)
        .await
        .expect("step must succeed by truncating the observation, not fail OverBudget");

    assert!(
        shrink_entries(&rec2, "working_set_shrunk").is_empty(),
        "no working set was ever rendered, so no working_set_shrunk entry is expected: {:?}",
        rec2.entries()
    );
    let entries = shrink_entries(&rec2, "observation_shrunk");
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one observation_shrunk entry: {:?}",
        rec2.entries()
    );
    let from = entries[0]
        .payload
        .get("from")
        .and_then(Value::as_u64)
        .unwrap();
    let to = entries[0]
        .payload
        .get("to")
        .and_then(Value::as_u64)
        .unwrap();
    let overage = entries[0]
        .payload
        .get("overage")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(
        from, 1500,
        "expected the shrink to start from the observation's clamped (2000-budget) token count"
    );
    assert_eq!(
        to,
        from.saturating_sub(overage + 64),
        "`to` must equal the spec's exact formula: budget.saturating_sub(overage + 64)"
    );
    assert!(overage > 0, "expected a nonzero measured overage");

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let final_tokens = tok.count(&prompts[1]);
    assert!(
        final_tokens <= budgets.total,
        "expected the rebuilt prompt to fit within total ({}), got {}",
        budgets.total,
        final_tokens
    );
}

/// (a2) Item 54: a working set of two whole windows (a big one and a
/// medium one, ~1023 tokens together) sits comfortably under its budget
/// (1200) on its own -- only the *other* sections pushed step 3's prompt
/// over `total`. The first `overage + 64` cut (round 1: 1200 -> 1066)
/// still leaves enough room for both windows (1023 <= 1066), so it frees
/// nothing and the measured working-set size is unchanged; round 2 must
/// therefore escalate to halving (`min(plain_cut, budget / 2)`) rather
/// than repeat the same ineffective `overage + 64` cut. Exactly one
/// `working_set_shrunk` entry is logged for the whole step, carrying the
/// exact round count and the first overage.
#[tokio::test]
async fn working_set_shrunk_halves_the_budget_when_a_cut_frees_nothing() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tok = HeuristicTokenizer;

    // Two windows of very different sizes: a big one and a medium one,
    // both comfortably under the working_set budget on their own (~1023
    // tokens together vs. a 1200-token budget) so a small cut
    // (overage + 64) doesn't cross the threshold needed to drop either
    // window whole.
    let a_body = "a".repeat(3600); // 900 tokens
    let d_body = "d".repeat(400); // 100 tokens
    let files: &[(&str, &str)] = &[
        ("a.txt", &a_body),
        ("d.txt", &d_body),
        ("b.txt", "hi"),
        ("c.txt", "yo"),
    ];

    let model = RecordingModel::new(vec![
        eviction_step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        eviction_step_output(serde_json::json!({}), read_action("d.txt")), // step 2
        eviction_step_output(serde_json::json!({}), read_action("b.txt")), // step 3
        eviction_step_output(serde_json::json!({}), read_action("c.txt")), // step 4
    ]);

    // Fixed sections plus `total` dialed (by measurement, since the exact
    // rendered size of fixed sections isn't hand-computable) so step 3's
    // prompt -- working set of both windows at their full 1200-token
    // budget, pending observation d.txt's tiny output -- overflows `total`
    // by exactly 70 tokens.
    let instructions = "small instructions".to_string();
    let budgets = Budgets {
        total: 1450,
        observation: 100,
        augmentation: 500,
        notes: 400,
        working_set: 1200,
    };

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        eviction_tool_set(files),
        Box::new(HeuristicTokenizer),
        eviction_session_config(&root, budgets),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 2,
        recent_calls: 0,
    });

    let skill = Arc::new(EvictionSkill { instructions });
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: read a.txt (large). Its output is step 2's pending
    // observation, not yet a working-set window.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec1, "working_set_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: read d.txt (medium). Its own prompt observation is a.txt's
    // output, clamped to the 100-token observation budget; no working set
    // exists yet (a.txt is still the single newest output, excluded from
    // windows), so this step builds cleanly.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec2, "working_set_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: read b.txt. Its prompt's pending observation is d.txt's
    // tiny output, and its working set now renders both a.txt's and
    // d.txt's windows (b.txt's own not-yet-committed output is excluded)
    // at up to the full 1200-token budget -- fixed sections plus that
    // exceed `total` (1450) by 70 tokens, and the working set alone
    // (1023 tokens) doesn't shrink under the first cut.
    let rec3 = active
        .step(Event::Tick, &mut NullSink)
        .await
        .expect("step must succeed by evicting the working set, not fail OverBudget");

    let entries = shrink_entries(&rec3, "working_set_shrunk");
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one working_set_shrunk entry for the whole step: {:?}",
        rec3.entries()
    );
    let from = entries[0]
        .payload
        .get("from")
        .and_then(Value::as_u64)
        .unwrap();
    let to = entries[0]
        .payload
        .get("to")
        .and_then(Value::as_u64)
        .unwrap();
    let rounds = entries[0]
        .payload
        .get("rounds")
        .and_then(Value::as_u64)
        .unwrap();
    let overage = entries[0]
        .payload
        .get("overage")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(
        from, 1200,
        "expected the shrink to start from the configured working_set budget"
    );
    assert_eq!(overage, 70, "expected the first measured overage");
    assert_eq!(
        rounds, 2,
        "round 1's plain overage+64 cut (1200 -> 1066) still leaves room for \
         both windows (1023 tokens), freeing nothing, so round 2 must escalate \
         to halving"
    );
    // Round 1: plain cut, 1200 - (70 + 64) = 1066 (frees nothing: the
    // working set is still 1023 tokens, well under 1066). Round 2:
    // min(1066 - (70 + 64), 1066 / 2) = min(932, 533) = 533.
    assert_eq!(
        to, 533,
        "expected round 2 to halve (533) rather than repeat the plain cut (932)"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    let final_tokens = tok.count(&prompts[2]);
    assert!(
        final_tokens <= budgets.total,
        "expected the rebuilt prompt to fit within total ({}), got {}",
        budgets.total,
        final_tokens
    );
}

/// (b) Item 54's bound: a working set whose content the eviction loop can
/// never shrink to fit within `WORKING_SET_EVICTION_ROUNDS` rounds of pure
/// halving (the budget starts at ~650x the working set's own content size,
/// so 8 halvings only bring it down to ~2601 -- still well above the
/// ~1023-token content, so every round frees nothing and none of them ever
/// drops a window). The loop must stop at exactly
/// `WORKING_SET_EVICTION_ROUNDS` rounds, log exactly one `working_set_shrunk`
/// entry for the whole step, and fall through to the existing observation
/// clamp -- which, with the remaining (small) overage, succeeds here.
#[tokio::test]
async fn working_set_shrunk_stops_at_the_round_bound_and_falls_through() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tok = HeuristicTokenizer;

    // Two windows summing to ~1023 tokens, plus a final read (c.txt) whose
    // step is the one under test.
    let a_body = "a".repeat(3600); // 900 tokens
    let d_body = "d".repeat(400); // 100 tokens
    let files: &[(&str, &str)] = &[("a.txt", &a_body), ("d.txt", &d_body), ("c.txt", "yo")];

    let model = RecordingModel::new(vec![
        eviction_step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        eviction_step_output(serde_json::json!({}), read_action("d.txt")), // step 2
        eviction_step_output(serde_json::json!({}), read_action("c.txt")), // step 3
    ]);

    // `total` dialed (by measurement) so that after 8 rounds of halving
    // (666_000 -> ... -> 5202, never dropping a window) the remaining
    // overage (30 tokens) is small enough for the fallback observation
    // clamp to absorb by shrinking the 100-token pending observation.
    let instructions = "small instructions".to_string();
    let budgets = Budgets {
        total: 1490,
        observation: 100,
        augmentation: 500,
        notes: 400,
        working_set: 666_000,
    };

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        eviction_tool_set(files),
        Box::new(HeuristicTokenizer),
        eviction_session_config(&root, budgets),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 2,
        recent_calls: 0,
    });

    let skill = Arc::new(EvictionSkill { instructions });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec1, "working_set_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(shrink_entries(&rec2, "working_set_shrunk").is_empty());
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: working set is both windows (~1023 tokens) at up to the
    // 666_000-token budget; 8 rounds of halving never gets the budget
    // below the content size, so the loop stops at the bound and falls
    // through to the observation clamp, which succeeds.
    let rec3 = active
        .step(Event::Tick, &mut NullSink)
        .await
        .expect("step must succeed via the observation-clamp fallback after the round bound");

    let ws_entries = shrink_entries(&rec3, "working_set_shrunk");
    assert_eq!(
        ws_entries.len(),
        1,
        "expected exactly one working_set_shrunk entry for the whole step: {:?}",
        rec3.entries()
    );
    let from = ws_entries[0]
        .payload
        .get("from")
        .and_then(Value::as_u64)
        .unwrap();
    let to = ws_entries[0]
        .payload
        .get("to")
        .and_then(Value::as_u64)
        .unwrap();
    let rounds = ws_entries[0]
        .payload
        .get("rounds")
        .and_then(Value::as_u64)
        .unwrap();
    let overage = ws_entries[0]
        .payload
        .get("overage")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(from, 666_000, "expected the configured working_set budget");
    assert_eq!(
        rounds,
        statie_core::WORKING_SET_EVICTION_ROUNDS as u64,
        "the loop must stop at exactly the round bound, never fitting by pure halving"
    );
    assert_eq!(to, 5202, "expected the exact budget after 8 halving rounds");
    assert_eq!(overage, 30, "expected the first measured overage");

    // The bound alone didn't close the gap: the working set section is
    // still logically "over" relative to what would be needed, so the
    // existing observation clamp fallback ran once and closed it.
    let obs_entries = shrink_entries(&rec3, "observation_shrunk");
    assert_eq!(
        obs_entries.len(),
        1,
        "expected the observation clamp fallback to run exactly once after \
         the working-set round bound was hit: {:?}",
        rec3.entries()
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    let final_tokens = tok.count(&prompts[2]);
    assert!(
        final_tokens <= budgets.total,
        "expected the rebuilt prompt to fit within total ({}), got {}",
        budgets.total,
        final_tokens
    );
}
