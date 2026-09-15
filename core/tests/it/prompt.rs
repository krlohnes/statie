use std::sync::Arc;

use statie_core::{
    AgentState, Augmentation, Budgets, Capabilities, Commitment, HarnessState, HarnessView,
    HeuristicTokenizer, InboxItem, LogEntry, LogKind, MemoryLog, OverBudget, Pinned, PromptBuilder,
    Promptable, Schema, SessionLog, SkillSpec, StepOutput, SystemPrompt, ToolSet, Version,
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
        last_failure_summary: None,
        last_skip: None,
        last_tool_error: None,
        last_stall: None,
        last_tool: None,
        fails_since_green: 0,
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
    // unit 36: Agent State, ..., Augmentation, Harness View, Observation --
    // the harness's per-step counters move to the end of the user block.
    assert!(agent_idx < aug_idx);
    assert!(aug_idx < harness_idx);
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
fn working_set_section_renders_between_agent_state_and_augmentation() {
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
    let agent_idx = user_text
        .find("## Agent State")
        .expect("Agent State heading");
    let harness_idx = user_text
        .find("## Harness View")
        .expect("Harness View heading");
    let ws_idx = user_text
        .find("## working set")
        .expect("working set heading");
    let aug_idx = user_text
        .find("## Augmentation")
        .expect("Augmentation heading");
    // unit 36: Agent State, WorkingSet, ..., Augmentation, Harness View.
    assert!(agent_idx < ws_idx);
    assert!(ws_idx < aug_idx);
    assert!(aug_idx < harness_idx);
    assert!(
        user_text
            .contains("working set (what you read recently; derived from the log, not state):")
    );
}

// =======================================================================
// Unit 36 acceptance (a): a cacheable prompt layout. `Observation` and
// `Notes` have no public constructor (this file's own top comment notes
// the same restriction for `Observation` on the section-budget test just
// above), so the only way to get real ones from here is a live
// `Session<W>` step. Core cannot depend on `statie-skills`/`statie-store`/
// `statie-models`, so -- in the same spirit as `core/tests/it/after_tool.rs`
// and `core/tests/it/maintainer.rs` -- this submodule builds its own
// minimal `StateStore`, `Model`, `Skill`, and `Tool` stubs.
// =======================================================================
mod prefix_stability {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use statie_core::{
        Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
        LoggedPatch, Model, ModelError, NullSink, Outcome, Park, PatchError, PatchStrategy,
        RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill,
        SkillSpec, StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet, Version,
    };

    // -------------------------------------------------------------
    // A minimal in-memory `StateStore<W>` (copied in spirit from
    // `core/tests/it/after_tool.rs`/`maintainer.rs`; core has no
    // test-only shared crate).
    // -------------------------------------------------------------

    struct Inner<W> {
        initial: CommittedState<W>,
        current: CommittedState<W>,
        version: Version,
        log: Vec<LoggedPatch>,
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
                agent =
                    apply(&agent, &entry.patch).map_err(|e| StoreError::Backend(e.to_string()))?;
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

    // -------------------------------------------------------------
    // A scripted `Model` that records the raw user-block text it was
    // given for every step.
    // -------------------------------------------------------------

    struct RecordingModel {
        turns: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl RecordingModel {
        fn new(turns: Vec<Value>) -> Arc<RecordingModel> {
            Arc::new(RecordingModel {
                turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
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

    // -------------------------------------------------------------
    // `probe`: a real registered `Tool` whose output differs on every
    // call (an internal counter), so a real `Observation` naturally
    // varies step to step without needing `Observation`'s pub(crate)
    // constructors.
    // -------------------------------------------------------------

    struct ProbeTool {
        calls: AtomicU32,
    }

    impl ProbeTool {
        fn new() -> Self {
            ProbeTool {
                calls: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "returns a distinct output on every call"
        }
        fn args_schema(&self) -> Schema {
            Schema::of::<Value>()
        }
        async fn call(
            &self,
            _args: Value,
            _ctx: &ToolCtx,
        ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(statie_core::ToolOutputFull::new(format!(
                "probe output #{n}"
            )))
        }
    }

    // -------------------------------------------------------------
    // Test workspace: `value: i64` (a plain, always-empty-patch field so
    // `AgentState`'s rendered JSON never changes across the two compared
    // prompts) and `notes: String`, marked `skip_serializing` so setting
    // it (to flip `Skill::notes` from `None` to `Some`) never itself
    // changes the rendered `AgentState` JSON -- only `Skill::notes`'s
    // derived Notes *section* differs, precisely isolating the two
    // things unit 36 says may still change step to step (harness view,
    // observation) from everything the new prompt layout claims is
    // stable (agent state, working set, notes).
    // -------------------------------------------------------------

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct Workspace {
        #[serde(default)]
        value: i64,
        #[serde(default, skip_serializing)]
        notes: String,
        #[serde(default)]
        next: Option<Commitment>,
        #[serde(default)]
        done: bool,
    }

    struct MergePatch;

    impl PatchStrategy<Workspace> for MergePatch {
        type Patch = Value;

        fn name() -> &'static str {
            "merge"
        }

        fn describe() -> &'static str {
            "A JSON merge patch over the top-level workspace fields."
        }

        fn apply(cur: &Workspace, patch: &Value) -> Result<Workspace, PatchError> {
            // Built from `cur`'s fields directly (not `to_value(cur)`,
            // which would omit `notes` -- `skip_serializing` only hides
            // it from the rendered AgentState/schema output, not from
            // this merge-patch base) so a `{"notes": ...}` patch still
            // lands on the struct.
            let mut cur_map = serde_json::Map::new();
            cur_map.insert("value".to_string(), serde_json::json!(cur.value));
            cur_map.insert("notes".to_string(), serde_json::json!(cur.notes));
            cur_map.insert("next".to_string(), serde_json::to_value(&cur.next).unwrap());
            cur_map.insert("done".to_string(), serde_json::json!(cur.done));
            let mut cur_value = Value::Object(cur_map);
            if let (Value::Object(cur_map), Value::Object(patch_map)) = (&mut cur_value, patch) {
                for (k, v) in patch_map {
                    cur_map.insert(k.clone(), v.clone());
                }
            } else {
                return Err(PatchError::Malformed("patch must be an object".to_string()));
            }
            serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
        }
    }

    /// Active tool `probe`; `notes(ws)` surfaces `ws.notes` as the Notes
    /// section (empty -> no section, matching `PromptBuilder::notes`'s
    /// own "empty renders nothing" rule) -- no maintainer needed, the
    /// model's own patch sets it directly.
    struct ProbeSkill;

    impl Skill for ProbeSkill {
        type Workspace = Workspace;
        type Patch = MergePatch;

        fn name(&self) -> &str {
            "probe"
        }

        fn spec(&self) -> Arc<SkillSpec> {
            Arc::new(SkillSpec::new(
                "probe",
                "a skill for the prefix-stability test",
            ))
        }

        fn initial_workspace(&self) -> Workspace {
            Workspace::default()
        }

        fn tools(&self) -> Vec<String> {
            vec!["probe".to_string()]
        }

        fn is_terminal(&self, s: &Workspace) -> bool {
            s.done
        }

        fn notes(&self, ws: &Workspace) -> Option<String> {
            if ws.notes.is_empty() {
                None
            } else {
                Some(ws.notes.clone())
            }
        }
    }

    fn session_config(root: &tempfile::TempDir) -> SessionConfig {
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

    fn probe_tool_call() -> Value {
        serde_json::json!({"type": "tool", "name": "probe", "args": {}})
    }

    /// Byte length of the longest common prefix of two strings.
    fn common_prefix_len(a: &str, b: &str) -> usize {
        a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
    }

    #[tokio::test]
    async fn user_block_prefix_stable_when_only_harness_view_and_observation_change() {
        let root = tempfile::tempdir().unwrap();
        let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

        // Step 1: a real tool call (`probe`) -- commits, seeds the working
        // set with a `read`-shaped recent-calls line naming `probe`'s real
        // (first) output, and sets `pending_observation` to that output.
        // Step 2: `ask_user` with an empty patch except for `notes`, which
        // it sets for the first time -- `ask_user` is not `type: "tool"`,
        // so `WorkingSet::from_log` (which only looks at tool actions)
        // renders identically whether it's built before or after this
        // step; and `ask_user` never sets a new `pending_observation`
        // (`core/src/session.rs`'s `Action::AskUser` arm), so by the time
        // the third prompt is built there is none pending -- the
        // Observation section is omitted from it entirely.
        let model = RecordingModel::new(vec![
            serde_json::json!({
                "patch": {},
                "action": probe_tool_call(),
                "ack_inbox": []
            }),
            serde_json::json!({
                "patch": {"notes": "steady notes text"},
                "action": {"type": "ask_user", "message": "?"},
                "ack_inbox": []
            }),
            serde_json::json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "?"},
                "ack_inbox": []
            }),
        ]);

        let mut session = Session::new(
            Box::new(TestStore::<Workspace>::new()),
            Arc::clone(&log),
            Arc::clone(&model) as Arc<dyn Model>,
            {
                let mut tools = ToolSet::new();
                tools.register(Arc::new(ProbeTool::new()));
                tools
            },
            Box::new(statie_core::HeuristicTokenizer),
            session_config(&root),
        );

        let skill = Arc::new(ProbeSkill);
        let mut active = session.activate(skill).await.unwrap();

        // Step 1: probe tool call. Commits (Continue).
        let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
        assert!(matches!(
            log.append_record(rec1).unwrap(),
            Outcome::Continue
        ));

        // Step 2: the prompt just built (for this step) is prompt A --
        // Observation present (probe's output), no Notes yet, harness
        // step 1. The action taken here (`ask_user`) parks.
        let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
        assert!(matches!(
            log.append_record(rec2).unwrap(),
            Outcome::Parked(Park::AskUser(_))
        ));

        // Step 3: the prompt just built for this step is prompt B --
        // Notes now present (set by step 2's patch), no Observation
        // (nothing pending after an `ask_user` park), harness step 2.
        let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
        assert!(matches!(
            log.append_record(rec3).unwrap(),
            Outcome::Parked(Park::AskUser(_))
        ));

        let prompts = model.prompts();
        assert_eq!(prompts.len(), 3);
        let user_a = &prompts[1];
        let user_b = &prompts[2];

        // Sanity on the premise: `a` carries an Observation and no Notes;
        // `b` carries Notes and no Observation; both carry a Harness View
        // with a different `step:` line.
        assert!(user_a.contains("probe output #1"), "{user_a}");
        assert!(
            !user_a.contains("notes (kept by the harness from your previous reasoning)"),
            "{user_a}"
        );
        assert!(
            user_b.contains("notes (kept by the harness from your previous reasoning)"),
            "{user_b}"
        );
        assert!(user_b.contains("steady notes text"), "{user_b}");
        // `b` has no Observation section -- but the working set's own
        // recent-calls line for step 1 (shared, unaffected content) still
        // legitimately says "probe output #1", so check for the
        // Observation heading specifically, not the tool's output text.
        assert!(!user_b.contains("## Observation"), "{user_b}");
        assert!(user_a.contains("## Observation"), "{user_a}");
        assert!(user_a.contains("step: 1"), "{user_a}");
        assert!(user_b.contains("step: 2"), "{user_b}");

        // The AgentState + WorkingSet prefix the two prompts share must be
        // byte-identical: find where each prompt's Harness/Notes content
        // begins by locating the working-set header's end is unnecessary
        // -- the shared prefix computed directly proves it.
        let prefix_len = common_prefix_len(user_a, user_b);

        // `b`'s notes heading: everything up to and including it is
        // shared with `a` up to the point the two headings' common
        // "## " diverges (`a`'s next section there is "## Harness View",
        // `b`'s is "## notes ..."). So the exact, computed relationship
        // is: prefix_len == notes_offset + len("## ") -- the working-set
        // section (and everything before it) is exactly reproduced, and
        // the divergence is pinned to the first byte that actually
        // differs between the two prompts' next section.
        let notes_heading = "## notes (kept by the harness from your previous reasoning)";
        let notes_offset = user_b
            .find(notes_heading)
            .expect("expected a notes heading in prompt b");
        let shared_heading_marker_len = "## ".len();
        assert_eq!(
            prefix_len,
            notes_offset + shared_heading_marker_len,
            "expected the common prefix to extend through the end of the working-set section \
             (up to where prompt b's notes heading begins, plus the 3-byte '## ' every section \
             heading starts with) and diverge there, exact:\na={user_a:?}\nb={user_b:?}"
        );

        // And the divergence point is confirmed structurally too: right
        // before it, `a` continues with "Harness View" and `b` continues
        // with "notes (kept", so byte `prefix_len` itself is the first
        // differing byte.
        assert_ne!(
            user_a.as_bytes()[prefix_len],
            user_b.as_bytes()[prefix_len],
            "expected the two prompts to actually diverge at prefix_len"
        );
    }
}
