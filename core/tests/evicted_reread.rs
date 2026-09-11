//! Integration tests for item 19: a repeat read whose earlier window has
//! been evicted from the rendered working set must run again (logged as
//! `reread_evicted`), while one whose window is still rendered is refused
//! exactly as before (`skipped_redundant`). Fixture style copied from
//! `core/tests/read_paging.rs` (core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models`, so this test builds its
//! own minimal `StateStore`, `Model`, `Skill`, and `Tool` stubs).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, Model, ModelError, NullSink, PatchError, PatchStrategy, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet, Version, WorkingSetConfig,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/read_paging.rs`).
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
// (copied from `core/tests/read_paging.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<Value>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
        })
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
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace: same shape as `core/tests/read_paging.rs`'s.
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
        Arc::new(SkillSpec::new(
            "guarded",
            "a skill for evicted-reread guard tests",
        ))
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
// A scripted `read_file` tool: whole-file contents keyed by path, each
// short enough to never be truncated by the window header logic (renders
// the same `"{path} ({total} lines, showing 1-{total})"` header shape as
// `tools/src/read_file.rs`, but since every file here is only a couple of
// lines, `showing 1-{total}` always reaches the end -- no paging special
// case can fire, keeping these tests purely about the eviction guard).
// ---------------------------------------------------------------------

struct ScriptedReadFile {
    calls: Mutex<u64>,
    files: HashMap<String, String>,
}

impl ScriptedReadFile {
    fn new(files: &[(&str, &str)]) -> Self {
        ScriptedReadFile {
            calls: Mutex::new(0),
            files: files
                .iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
        }
    }
}

#[async_trait]
impl Tool for ScriptedReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "reads a whole small file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        *self.calls.lock().unwrap() += 1;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let body = self
            .files
            .get(path)
            .cloned()
            .unwrap_or_else(|| "".to_string());
        let total = body.lines().count().max(1) as u64;
        let mut out = format!("{path} ({total} lines, showing 1-{total})\n");
        for (i, line) in body.lines().enumerate() {
            out.push_str(&format!("{:>5} | {line}\n", i + 1));
        }
        Ok(statie_core::ToolOutputFull::new(out))
    }
}

fn tool_set(files: &[(&str, &str)]) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(ScriptedReadFile::new(files)));
    tools
}

fn read_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": path}})
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

fn has_tool_output(rec: &statie_core::StepRecord) -> bool {
    rec.entries().iter().any(|e| e.kind == LogKind::ToolOutput)
}

fn tool_output_text(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| e.kind == LogKind::ToolOutput)
        .and_then(|e| e.payload.get("output").and_then(Value::as_str))
        .map(str::to_string)
}

const FILES: &[(&str, &str)] = &[
    ("a.txt", "alpha one\nalpha two"),
    ("b.txt", "bravo one\nbravo two"),
    ("c.txt", "charlie one\ncharlie two"),
    ("d.txt", "delta one\ndelta two"),
];

// =======================================================================
// (1) `max_windows: 2`: read a.txt (step 1), then b.txt (step 2), c.txt
// (step 3), and d.txt (step 4) -- when the working set is built for step
// 5's prompt, the newest output (d.txt, step 4) is the current-step
// observation (never itself shown as a window), so the two window
// candidates are the newest two of the REMAINING reads: c.txt (step 3) and
// b.txt (step 2) -- evicting a.txt's window (step 1) -- then read a.txt
// again (step 5): the read must actually RUN (a `tool_output` entry with
// a.txt's content), the log must carry exactly one `reread_evicted` entry
// naming `at_step: 1`, and no `skipped_redundant` entry.
// =======================================================================

#[tokio::test]
async fn evicted_window_reread_runs_again_and_logs_reread_evicted() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        step_output(serde_json::json!({}), read_action("b.txt")), // step 2
        step_output(serde_json::json!({}), read_action("c.txt")), // step 3
        step_output(serde_json::json!({}), read_action("d.txt")), // step 4
        step_output(serde_json::json!({}), read_action("a.txt")), // step 5: repeat, evicted
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(FILES),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 2,
        recent_calls: 6,
    });

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: read a.txt.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(!has_event(&rec1, "skipped_redundant"));
    assert!(!has_event(&rec1, "reread_evicted"));
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: read b.txt -- a newer distinct window.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(!has_event(&rec2, "skipped_redundant"));
    assert!(!has_event(&rec2, "reread_evicted"));
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: read c.txt.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(!has_event(&rec3, "skipped_redundant"));
    assert!(!has_event(&rec3, "reread_evicted"));
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    // Step 4: read d.txt. With max_windows: 2, the working set built for
    // step 5's prompt excludes d.txt itself (it's the current-step
    // observation) and renders the newest 2 of the rest: c.txt (step 3)
    // and b.txt (step 2) -- a.txt's window (step 1) has been evicted.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(!has_event(&rec4, "skipped_redundant"));
    assert!(!has_event(&rec4, "reread_evicted"));
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();

    // Step 5: repeat the read of a.txt. Its window (step 1) is evicted, so
    // the read must run again for real.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec5, "skipped_redundant"),
        "an evicted-window repeat must not be skipped: {:?}",
        rec5.entries()
    );
    assert!(
        has_tool_output(&rec5),
        "an evicted-window repeat must actually run the tool and log a tool_output entry: {:?}",
        rec5.entries()
    );
    let output = tool_output_text(&rec5).expect("expected a real ToolOutput entry");
    assert!(
        output.contains("alpha one") && output.contains("alpha two"),
        "expected a.txt's real content in the re-run output, got: {output}"
    );

    let reread_entries: Vec<&statie_core::LogEntry> = rec5
        .entries()
        .iter()
        .filter(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("reread_evicted")
        })
        .collect();
    assert_eq!(
        reread_entries.len(),
        1,
        "expected exactly one reread_evicted entry: {:?}",
        rec5.entries()
    );
    let entry = reread_entries[0];
    assert_eq!(
        entry.payload.get("name").and_then(Value::as_str),
        Some("read_file")
    );
    assert_eq!(
        entry.payload.get("at_step").and_then(Value::as_u64),
        Some(1)
    );
    SessionLogExt::append_record(log.as_ref(), rec5).unwrap();
}

// =======================================================================
// (2) `max_windows: 4`: the same four-step sequence never evicts a.txt's
// window (it still fits among the newest 4 windows) -- the repeat at step
// 4 is refused exactly as today: one `skipped_redundant` entry, no
// `tool_output`, no `reread_evicted`.
// =======================================================================

#[tokio::test]
async fn still_rendered_window_reread_is_refused_as_today() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        step_output(serde_json::json!({}), read_action("b.txt")), // step 2
        step_output(serde_json::json!({}), read_action("c.txt")), // step 3
        step_output(serde_json::json!({}), read_action("a.txt")), // step 4: repeat, still rendered
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(FILES),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 4,
        recent_calls: 6,
    });

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec4, "skipped_redundant"),
        "with max_windows: 4, a.txt's window is still rendered, so the repeat must be skipped \
         as today: {:?}",
        rec4.entries()
    );
    assert!(
        !has_tool_output(&rec4),
        "a genuinely skipped repeat must not log a tool_output entry: {:?}",
        rec4.entries()
    );
    assert!(
        !has_event(&rec4, "reread_evicted"),
        "a genuinely skipped repeat must not log reread_evicted: {:?}",
        rec4.entries()
    );
    let skip_entry =
        find_event(&rec4, "skipped_redundant").expect("expected a skipped_redundant entry");
    let reason = skip_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("that window is in your working set above"),
        "expected the working-set-aware refusal text, got: {reason}"
    );
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();
}

// =======================================================================
// (3) `max_windows: 0`: the working-set section is off entirely, so the
// eviction check is bypassed -- a repeat still refuses exactly as before,
// with the "that output is in the log" wording.
// =======================================================================

#[tokio::test]
async fn windows_off_still_refuses_with_the_log_wording() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), read_action("a.txt")), // step 1
        step_output(serde_json::json!({}), read_action("a.txt")), // step 2: repeat
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(FILES),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_working_set(WorkingSetConfig {
        max_windows: 0,
        recent_calls: 0,
    });

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        has_event(&rec2, "skipped_redundant"),
        "with windows off, the repeat must still be refused: {:?}",
        rec2.entries()
    );
    assert!(!has_tool_output(&rec2));
    assert!(!has_event(&rec2, "reread_evicted"));
    let skip_entry =
        find_event(&rec2, "skipped_redundant").expect("expected a skipped_redundant entry");
    let reason = skip_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("that output is in the log"),
        "expected the plain log wording when windows are off, got: {reason}"
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
}
