//! Integration tests for the `guards` config switches (statie-spec.md "10.
//! After it works: the rule audit", milestone 12): `Session::with_guards`.
//! Core cannot depend on `statie-skills`/`statie-store`/`statie-models`, so
//! this test builds its own minimal `StateStore`, `Model`, `Skill`, and
//! `Tool` stubs, in the same spirit as `core/tests/maintainer.rs` and
//! `core/tests/user_patch.rs` (the `TestStore`/`RecordingModel` fixtures
//! below are copied from those files).
//!
//! Each guard gets one pair of tests: the rule fires with the switch on
//! (default, i.e. `Guards::default()`/no call to `with_guards`), and the
//! exact same scripted scenario does NOT fire with the switch off --
//! asserted by the positive behaviour (the call actually ran and produced
//! a fresh observation, the plan step reopened, etc.), not merely the
//! absence of a rejection.

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
// A minimal in-memory `StateStore<W>` (copied in spirit from
// `core/tests/maintainer.rs`; core has no test-only shared crate).
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
// A scripted `Model` that replays a fixed sequence of raw step outputs.
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    /// Every prompt's user text, in call order, so a test can assert on
    /// what the *next* prompt (i.e. the pending observation) said -- a
    /// harness note appended to an observation lands there, not in the
    /// already-logged `tool_output` entry for the call that triggered it.
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<Value>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        })
    }

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
// Test workspace: `value: i64` (bumped by a "recorded" tool call to prove
// a read wrote something), `notes: String` (harness-owned, unused by these
// tests but required by the shared fixture shape), `next:
// Option<Commitment>` (the commitment slot), `done: bool`. A real
// JSON-merge-patch strategy over top-level keys (reimplemented here since
// core cannot depend on `statie_patch`), so a partial patch only touches
// the key(s) it names, and `"next": null` clears the commitment.
// ---------------------------------------------------------------------

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

/// A skill with `next` as the commitment slot and an optional silent-read
/// cap, active tools `["read_file", "shell", "edit_file"]`.
struct GuardSkill {
    silent_read_cap: Option<u32>,
}

impl Skill for GuardSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "guarded"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("guarded", "a skill for guard-switch tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "read_file".to_string(),
            "shell".to_string(),
            "edit_file".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }

    fn silent_read_cap(&self) -> Option<u32> {
        self.silent_read_cap
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
// Real tools: `read_file(path)` returns a canned line for that path plus a
// per-call sequence number (so a re-run is distinguishable from a skipped
// call, which never reaches the tool at all), `shell(cmd)` likewise, and
// `edit_file` is a trivial mutation used to spend a commitment / reset the
// guards.
// ---------------------------------------------------------------------

struct CountingReadFile {
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl Tool for CountingReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "reads a file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        Ok(statie_core::ToolOutputFull::new(format!(
            "call #{n}: contents of {path}"
        )))
    }
}

struct CountingShell {
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl Tool for CountingShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "runs a command"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let cmd = args.get("cmd").and_then(Value::as_str).unwrap_or("?");
        Ok(statie_core::ToolOutputFull::new(format!(
            "run #{n}: exit: 0\n--- stdout ---\nok ({cmd})"
        )))
    }
}

struct NoopEditFile;

#[async_trait]
impl Tool for NoopEditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "edits a file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        Ok(statie_core::ToolOutputFull::new("edited".to_string()))
    }
}

fn tool_set(read_calls: Arc<AtomicU64>, shell_calls: Arc<AtomicU64>) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(CountingReadFile { calls: read_calls }));
    tools.register(Arc::new(CountingShell { calls: shell_calls }));
    tools.register(Arc::new(NoopEditFile));
    tools
}

fn read_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": path}})
}

fn shell_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "shell", "args": {"cmd": cmd}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn rejected_reason(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
        .map(str::to_string)
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

// =======================================================================
// 1. guards.redundant_reads (session rule 7.8)
// =======================================================================

#[tokio::test]
async fn redundant_reads_on_skips_the_identical_repeat() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    // Default guards (all on) -- no `with_guards` call.

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec2, "skipped_redundant"),
        "expected the second identical read to be skipped: {:?}",
        rec2.entries()
    );
    let outcome = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        1,
        "the skipped call must not reach the tool"
    );
}

#[tokio::test]
async fn redundant_reads_off_reruns_the_identical_repeat() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("a.rs")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        redundant_reads: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec2, "skipped_redundant"),
        "the repeat must not be skipped when the guard is off: {:?}",
        rec2.entries()
    );
    let tool_output = rec2
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::ToolOutput)
        .expect("expected a real tool_output entry (the call actually ran)");
    let output_text = tool_output
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        output_text.contains("call #2"),
        "expected a fresh (second) tool call output, got: {output_text}"
    );
    let outcome = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        2,
        "the repeat must actually reach the tool"
    );
}

// =======================================================================
// 2. guards.silent_reads
// =======================================================================

#[tokio::test]
async fn silent_reads_on_rejects_past_the_cap() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    // Two different paths (so 7.8 redundant-reads never fires) with an
    // empty patch each time; cap is 1, so the second silent read rejects.
    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("b.rs")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill {
        silent_read_cap: Some(1),
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let reason = rejected_reason(&rec2).expect("expected a rejection past the silent-read cap");
    assert!(reason.contains("wrote nothing"), "{reason}");
    let outcome = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        1,
        "a rejected step must not reach the tool"
    );
}

#[tokio::test]
async fn silent_reads_off_runs_past_the_cap() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.rs")),
        step_output(serde_json::json!({}), read_action("b.rs")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        silent_reads: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill {
        silent_read_cap: Some(1),
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        rejected_reason(&rec2).is_none(),
        "must not reject when the guard is off: {:?}",
        rec2.entries()
    );
    let tool_output = rec2
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::ToolOutput)
        .expect("expected a real tool_output entry (the call actually ran)");
    let output_text = tool_output
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        output_text.contains("call #2") && output_text.contains("b.rs"),
        "expected a fresh second read of b.rs, got: {output_text}"
    );
    let outcome = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
    assert_eq!(read_calls.load(Ordering::SeqCst), 2);
}

// =======================================================================
// 3. guards.read_guard (four reads of one path, no edit in between)
// =======================================================================

/// Builds a model script of `n` reads of `path`, each writing a distinct
/// one-line note into `notes` (so 7.8/silent-reads never fire -- only the
/// read-guard's four-reads-of-one-path rule is under test) with a distinct
/// `offset` per call so the (tool, args) key differs too.
fn read_guard_script(path: &str, n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            step_output(
                serde_json::json!({"notes": format!("looked at {path} ({i})")}),
                serde_json::json!({
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": path, "offset": i}
                }),
            )
        })
        .collect()
}

#[tokio::test]
async fn read_guard_on_notes_the_fourth_read() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(read_guard_script("a.rs", 4));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let mut last_had_read_guard = false;
    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        last_had_read_guard = has_event(&rec, "read_guard");
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    assert!(
        last_had_read_guard,
        "expected a read_guard note on the fourth read"
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn read_guard_off_never_notes() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    // One extra read past the threshold to prove the rule never fires,
    // not just that it hasn't fired yet.
    let model = ScriptedModel::new(read_guard_script("a.rs", 6));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        read_guard: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    for i in 0..6 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        assert!(
            !has_event(&rec, "read_guard"),
            "read {i} must not carry a read_guard note when the guard is off: {:?}",
            rec.entries()
        );
        let tool_output = rec
            .entries()
            .iter()
            .find(|e| e.kind == LogKind::ToolOutput)
            .expect("expected a real tool_output entry for every read");
        let output_text = tool_output
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            output_text.contains(&format!("call #{}", i + 1)),
            "expected read {i} to actually run and produce a fresh observation, got: {output_text}"
        );
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    assert_eq!(read_calls.load(Ordering::SeqCst), 6);
}

// =======================================================================
// 4. guards.redundant_verify (session rule 7.7)
// =======================================================================

#[tokio::test]
async fn redundant_verify_on_skips_the_identical_repeat() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), shell_action("cargo test")),
        step_output(serde_json::json!({}), shell_action("cargo test")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(read_calls, Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(shell_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec2, "skipped_redundant"),
        "expected the repeat `shell` run to be skipped: {:?}",
        rec2.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert_eq!(
        shell_calls.load(Ordering::SeqCst),
        1,
        "the skipped run must not reach the tool"
    );
}

#[tokio::test]
async fn redundant_verify_off_reruns_the_identical_repeat() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), shell_action("cargo test")),
        step_output(serde_json::json!({}), shell_action("cargo test")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(read_calls, Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        redundant_verify: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(shell_calls.load(Ordering::SeqCst), 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec2, "skipped_redundant"),
        "the repeat must not be skipped when the guard is off: {:?}",
        rec2.entries()
    );
    let tool_output = rec2
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::ToolOutput)
        .expect("expected a real tool_output entry (the call actually ran)");
    let output_text = tool_output
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        output_text.contains("run #2"),
        "expected a fresh (second) shell run, got: {output_text}"
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert_eq!(
        shell_calls.load(Ordering::SeqCst),
        2,
        "the repeat must actually reach the tool"
    );
}

// =======================================================================
// 5. guards.commitment_read_cap
// =======================================================================

fn commitment_json(tool: &str, path: &str, what: &str) -> Value {
    serde_json::json!({"tool": tool, "path": path, "what": what})
}

#[tokio::test]
async fn commitment_read_cap_on_rejects_the_third_read() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    // Step 1 sets the commitment and reads once (uses read #1 of the cap).
    // Steps 2/3 read again with a different offset each time (so 7.8 never
    // eclipses the commitment-cap rejection).
    let model = ScriptedModel::new(vec![
        step_output(
            serde_json::json!({"next": commitment_json("edit_file", "a.rs", "fix the bug")}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 1}
            }),
        ),
        step_output(
            serde_json::json!({}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 2}
            }),
        ),
        step_output(
            serde_json::json!({}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 3}
            }),
        ),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        2,
        "both reads within the cap should have run"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let reason = rejected_reason(&rec3).expect("expected the third read to be rejected");
    assert!(reason.contains("you committed to"), "{reason}");
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        2,
        "a rejected step must not reach the tool"
    );
}

#[tokio::test]
async fn commitment_read_cap_off_runs_reads_and_notes_the_pending_commitment() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(
            serde_json::json!({"next": commitment_json("edit_file", "a.rs", "fix the bug")}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 1}
            }),
        ),
        step_output(
            serde_json::json!({}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 2}
            }),
        ),
        step_output(
            serde_json::json!({}),
            serde_json::json!({
                "type": "tool", "name": "read_file", "args": {"path": "a.rs", "offset": 3}
            }),
        ),
        // A no-op fourth turn, just so the third step's pending observation
        // (carrying the pending-commitment note) gets rendered into a
        // prompt the test can inspect.
        step_output(serde_json::json!({}), serde_json::json!({"type": "yield"})),
    ]);
    let model_handle = Arc::clone(&model);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), shell_calls),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_guards(Guards {
        commitment_read_cap: false,
        ..Guards::default()
    });

    let skill = Arc::new(GuardSkill {
        silent_read_cap: None,
    });
    let mut active = session.activate(skill).await.unwrap();

    for i in 1..=3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        assert!(
            rejected_reason(&rec).is_none(),
            "read {i} must not be rejected when the cap is off: {:?}",
            rec.entries()
        );
        let tool_output = rec
            .entries()
            .iter()
            .find(|e| e.kind == LogKind::ToolOutput)
            .unwrap_or_else(|| panic!("expected read {i} to actually reach the tool"));
        let output_text = tool_output
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            output_text.contains(&format!("call #{i}")),
            "expected read {i} to actually run, got: {output_text}"
        );
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    assert_eq!(
        read_calls.load(Ordering::SeqCst),
        3,
        "every read must actually reach the tool when the cap is off"
    );

    // One more step so the model sees the prompt built from read #3's
    // pending observation (the note is appended there, not into the
    // already-logged tool_output entry for read #3 itself).
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();
    let prompts = model_handle.prompts();
    let prompt_after_third_read = prompts
        .get(3)
        .expect("expected a fourth prompt (built from read #3's observation)");
    assert!(
        prompt_after_third_read.contains("next")
            && prompt_after_third_read
                .to_lowercase()
                .contains("still pending"),
        "expected a pending-commitment note in the prompt after read #3, got: \
         {prompt_after_third_read}"
    );

    // The commitment itself must still be set (not cleared by a read).
    drop(active);
    let (state, _version) = session.state().await.unwrap();
    assert!(
        state.agent.next.is_some(),
        "the commitment must survive reads past the (disabled) cap"
    );
}
