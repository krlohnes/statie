//! Integration tests for 7.8's `read_file` paging special case (this unit):
//! a repeated identical `read_file` call whose previous window was
//! truncated pages forward (`offset = b + 1`) instead of being skipped.
//! Fixture style copied from `core/tests/guards.rs` (core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models`, so this test builds its
//! own minimal `StateStore`, `Model`, `Skill`, and `Tool` stubs).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, Guards,
    HarnessState, LogKind, LoggedPatch, Model, ModelError, NullSink, Outcome, PatchError,
    PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog,
    SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet,
    Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/guards.rs`).
// ---------------------------------------------------------------------

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

// ---------------------------------------------------------------------
// A scripted `Model` that replays a fixed sequence of raw step outputs
// (copied from `core/tests/guards.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<Value>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    /// Every prompt's user text, in call order -- a harness note appended
    /// to an observation (like 7.8's paging note) lands in the *next*
    /// prompt, not in the already-logged `tool_output` entry for the call
    /// that triggered it.
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for ScriptedModel {
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

// ---------------------------------------------------------------------
// Test workspace: same shape as `core/tests/guards.rs`'s, minus `value`
// (unused here).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
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
        let mut cur_value =
            serde_json::to_value(cur).map_err(|e| PatchError::Malformed(e.to_string()))?;
        if let (Value::Object(cur_map), Value::Object(patch_map)) = (&mut cur_value, patch) {
            for (k, v) in patch_map {
                if v.is_null() {
                    cur_map.remove(k);
                } else {
                    cur_map.insert(k.clone(), v.clone());
                }
            }
        } else {
            return Err(PatchError::Malformed("patch must be an object".to_string()));
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

struct GuardSkill;

impl Skill for GuardSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "guarded"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("guarded", "a skill for read-paging tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["read_file".to_string(), "search".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 2000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

// ---------------------------------------------------------------------
// A scripted `read_file` tool: a 250-line file. Renders exactly the same
// header shape as `tools/src/read_file.rs` (`"{path} ({total} lines,
// showing {from}-{to})"`), windowed 100 lines at a time so the header
// windows line up with the acceptance criteria (`showing 1-100`, `showing
// 101-200`, `showing 201-250`).
// ---------------------------------------------------------------------

struct ScriptedReadFile {
    calls: Arc<AtomicU64>,
    total: u64,
}

#[async_trait]
impl Tool for ScriptedReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "reads a window of a file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100);
        let from = offset.min(self.total.max(1));
        let to = (from + limit - 1).min(self.total);
        let mut out = format!("{path} ({} lines, showing {from}-{to})\n", self.total);
        for n in from..=to {
            out.push_str(&format!("{n:>5} | line {n}\n"));
        }
        Ok(statie_core::ToolOutputFull::new(out))
    }
}

struct NoopSearch;

#[async_trait]
impl Tool for NoopSearch {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "searches"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        Ok(statie_core::ToolOutputFull::new("no matches".to_string()))
    }
}

fn tool_set(read_calls: Arc<AtomicU64>, total: u64) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(ScriptedReadFile {
        calls: read_calls,
        total,
    }));
    tools.register(Arc::new(NoopSearch));
    tools
}

fn read_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": path}})
}

fn search_action(pattern: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "search", "args": {"pattern": pattern}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn find_event<'a>(
    rec: &'a statie_core::StepRecord,
    event: &str,
) -> Option<&'a statie_core::LogEntry> {
    rec.entries().iter().find(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn action_args(rec: &statie_core::StepRecord) -> Option<Value> {
    rec.entries()
        .iter()
        .find(|e| e.kind == LogKind::Action)
        .and_then(|e| e.payload.get("args").cloned())
}

fn tool_output_text(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| e.kind == LogKind::ToolOutput)
        .and_then(|e| e.payload.get("output").and_then(Value::as_str))
        .map(str::to_string)
}

// =======================================================================
// The main scenario: three identical `read_file {"path": "a.rs"}` repeats
// (no offset given by the model each time) page forward through a
// 250-line file, then the fourth repeat -- once the window has reached the
// end of the file -- is skipped as today.
// =======================================================================

#[tokio::test]
async fn repeated_identical_read_pages_forward_then_skips_at_the_end() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));

    // Step 1: first read, offset defaulted (1-100).
    // Step 2: identical repeat -- must page to 101-200, not skip.
    // Step 3: identical repeat again (model still sends no offset) -- must
    //         page to 201-250.
    // Step 4: identical repeat once more -- the window is now at the end of
    //         the file (250 of 250), so this must be skipped as today.
    // Step 5: harmless filler, just so the prompt built from step 4's
    //         observation can be inspected.
    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), search_action("foo")),
    ]);
    let model_handle = Arc::clone(&model);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), 250),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    // Default guards (all on) -- no `with_guards` call.

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: ordinary first read.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(!has_event(&rec1, "skipped_redundant"));
    assert!(!has_event(&rec1, "paged_read"));
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        1,
        "call count after step 1"
    );

    // Step 2: identical repeat, previous window (1-100) was truncated
    // (100 < 250) -> pages forward, does not skip. Tool call count 2.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec2, "skipped_redundant"),
        "the truncated-window repeat must not be skipped: {:?}",
        rec2.entries()
    );
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        2,
        "the repeat must actually reach the tool (tool call count 2)"
    );
    let paged_entry = find_event(&rec2, "paged_read").expect("expected a paged_read System entry");
    assert_eq!(
        paged_entry.payload.get("path").and_then(Value::as_str),
        Some("a.rs")
    );
    assert_eq!(
        paged_entry.payload.get("from").cloned(),
        Some(serde_json::json!([1, 100]))
    );
    assert_eq!(
        paged_entry.payload.get("to_offset").and_then(Value::as_u64),
        Some(101)
    );
    // The paged_read entry keeps the model's original args "for the
    // record" -- no offset, since the model never sent one.
    let original_args = paged_entry.payload.get("args").cloned().unwrap();
    assert_eq!(original_args.get("offset"), None);

    // The logged Action entry must carry the REWRITTEN args (offset: 101),
    // not the model's original (offset-less) args.
    let logged_args = action_args(&rec2).expect("expected an Action log entry with args");
    assert_eq!(
        logged_args.get("offset").and_then(Value::as_u64),
        Some(101),
        "the committed Action entry must carry offset: 101, got {logged_args:?}"
    );

    // The raw ToolOutput log entry is the tool's output unchanged (the
    // paging note is only added to the *observation*, i.e. shown in the
    // next prompt -- not stored in this entry, the same way the loop-guard
    // note works).
    let raw_output = tool_output_text(&rec2).expect("expected a real ToolOutput entry");
    assert!(raw_output.contains("showing 101-200"), "{raw_output}");

    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: repeated again (model still sends no offset). Now the
    // previous window (from the harness's point of view, keyed on the
    // REWRITTEN args) is 101-200, still truncated (200 < 250) -> pages to
    // 201-250.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec3, "skipped_redundant"),
        "the second truncated-window repeat must also page, not skip: {:?}",
        rec3.entries()
    );
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        3,
        "call count after step 3"
    );
    let logged_args3 = action_args(&rec3).unwrap();
    assert_eq!(
        logged_args3.get("offset").and_then(Value::as_u64),
        Some(201),
        "got {logged_args3:?}"
    );
    let output3 = tool_output_text(&rec3).unwrap();
    assert!(output3.contains("showing 201-250"), "{output3}");
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    // The prompt built for step 3 (from step 2's observation) is the one
    // that shows the "note: you repeated the read" line and the second
    // window (101-200): `prompts()[0]` is step 1's prompt (no observation
    // yet), `prompts()[1]` is step 2's (built from step 1's plain
    // observation), `prompts()[2]` is step 3's (built from step 2's paged
    // observation).
    let prompts = model_handle.prompts();
    let prompt_after_step2 = prompts
        .get(2)
        .expect("expected a third prompt (built from step 2's observation)");
    assert!(
        prompt_after_step2.contains(
            "note: you repeated the read of lines 1-100; showing the next window instead \
             (lines 101-200)."
        ),
        "expected the paging note in the prompt built from step 2's observation, got:\n{prompt_after_step2}"
    );
    assert!(
        prompt_after_step2.contains("showing 101-200"),
        "{prompt_after_step2}"
    );

    // Step 4: repeated once more. The window now at 201-250 has reached
    // the end of the 250-line file (250 == 250) -> falls through to the
    // ordinary skip.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec4, "skipped_redundant"),
        "the repeat past the end of the file must be skipped as today: {:?}",
        rec4.entries()
    );
    assert!(!has_event(&rec4, "paged_read"));
    let outcome = SessionLogExt::append_record(log.as_ref(), rec4).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        3,
        "the skipped call must not reach the tool"
    );
}

/// `search` repeats are unaffected by the `read_file`-only paging special
/// case: an identical repeat is still skipped exactly as before.
#[tokio::test]
async fn search_repeats_are_still_skipped() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), search_action("foo")),
        step_output(serde_json::json!({}), search_action("foo")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), 250),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec2, "skipped_redundant"),
        "search repeats must still be skipped: {:?}",
        rec2.entries()
    );
    assert!(!has_event(&rec2, "paged_read"));
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
}

/// `guards.redundant_reads = false` disables 7.8 entirely, including the
/// paging special case: the repeat runs unchanged, with no paging and no
/// skip -- and, crucially, no rewritten offset either (the model's original
/// args are used verbatim).
#[tokio::test]
async fn redundant_reads_off_disables_paging_too() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), 250),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        redundant_reads: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec2, "skipped_redundant"),
        "must not skip when the guard is off: {:?}",
        rec2.entries()
    );
    assert!(
        !has_event(&rec2, "paged_read"),
        "must not page when the guard is off: {:?}",
        rec2.entries()
    );
    let logged_args = action_args(&rec2).unwrap();
    assert_eq!(
        logged_args.get("offset"),
        None,
        "the call must run with the model's own (offset-less) args unchanged"
    );
    let output = tool_output_text(&rec2).unwrap();
    assert!(
        !output.starts_with("note: you repeated the read"),
        "must carry no paging note when the guard is off: {output}"
    );
    assert!(output.contains("showing 1-100"), "{output}");
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        2,
        "the repeat must actually reach the tool"
    );
}
