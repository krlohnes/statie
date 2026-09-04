//! Integration tests for the two commitment-slot harness fixes that came out
//! of run 21 (statie-spec.md "10. After it works: the rule audit", the
//! paragraph starting "Run 21 (the first with the working set)"):
//!
//! - A: a call the redundancy guards (7.7/7.8) skip as an exact repeat must
//!   still discharge a matching commitment -- otherwise `next` never
//!   clears, and every following call is rejected with "you committed to
//!   ..." (run 21 steps 19-27, a deadlock). See `Session::step`'s "10.
//!   Commitment slot follow-up" (`core/src/session.rs`).
//! - B: a `shell`/`test` commitment matches the mutating call on the tool
//!   name alone -- `c.path` is ignored for these two tools, since a shell
//!   command carries no `path` argument. `edit_file`/`write_file` keep the
//!   path comparison. See `matches_commitment` in the same function.
//!
//! Fixture style copied from `core/tests/guards.rs` (core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models`, so every core test file
//! builds its own minimal `StateStore`, `Model`, `Skill`, and `Tool`
//! fixtures): a `Workspace` with a JSON-merge `next: Option<Commitment>`
//! slot, a `ScriptedModel` that replays raw step outputs and records every
//! prompt, and counting `read_file`/`shell`/`edit_file` tools sandboxed via
//! `SessionConfig`.

use std::sync::atomic::{AtomicU64, Ordering};
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
    StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet, Version,
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
// A scripted `Model` that replays a fixed sequence of raw step outputs.
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
// Test workspace (copied from `core/tests/guards.rs`): `value: i64`,
// `notes: String` (harness-owned, unused here), `next: Option<Commitment>`
// (the commitment slot), `done: bool`, over a real JSON-merge-patch
// strategy (reimplemented since core cannot depend on `statie_patch`).
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

/// A skill with `next` as the commitment slot, active tools
/// `["read_file", "shell", "edit_file"]`.
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
            "a skill for commitment-fix tests",
        ))
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
// Real tools (copied from `core/tests/guards.rs`): `read_file(path)` and
// `shell(cmd)` return a canned line plus a per-call sequence number (so a
// re-run is distinguishable from a skipped call, which never reaches the
// tool at all); `edit_file` is a trivial mutation.
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

fn shell_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "shell", "args": {"cmd": cmd}})
}

fn read_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": path}})
}

fn commitment_json(tool: &str, path: Option<&str>, what: &str) -> Value {
    match path {
        Some(p) => serde_json::json!({"tool": tool, "path": p, "what": what}),
        None => serde_json::json!({"tool": tool, "what": what}),
    }
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn rejected_reason(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
        .map(str::to_string)
}

/// Pending observation text delivered in the *next* prompt, i.e. the last
/// recorded prompt's user text on `model`. Panics if no prompt has been
/// recorded yet.
fn last_prompt(model: &ScriptedModel) -> String {
    model
        .prompts()
        .last()
        .cloned()
        .expect("expected at least one recorded prompt")
}

// =======================================================================
// A. A skipped committed call discharges the commitment.
// =======================================================================

/// Step 1 sets a `shell` commitment and immediately runs it (spending it via
/// the ordinary fulfilment path -- this also seeds `last_verify` so step 2's
/// identical command is caught by 7.7). Step 2 re-commits to the very same
/// `shell` command (nothing has changed since, so 7.7 will skip it) --
/// exercising the discharge path. Step 3 is a harmless `read_file` so the
/// test can assert `commitment_reads` bookkeeping does not reject it (it
/// would, if the commitment were still open from step 2).
#[tokio::test]
async fn skipped_call_matching_the_commitment_discharges_it() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Step 1: commit to `cargo test`, then run it (fulfils normally).
        step_output(
            serde_json::json!({"next": commitment_json("shell", None, "run the tests")}),
            shell_action("cargo test"),
        ),
        // Step 2: re-commit to the identical `cargo test` (still nothing
        // mutated since step 1's run), then run it again -- 7.7 skips this
        // exact repeat, but it still matches the just-set commitment.
        step_output(
            serde_json::json!({"next": commitment_json("shell", None, "confirm nothing broke")}),
            shell_action("cargo test"),
        ),
        // Step 3: an ordinary read, to prove `commitment_reads` bookkeeping
        // was reset and does not reject it.
        step_output(serde_json::json!({}), read_action("a.rs")),
    ]);
    let model_handle = Arc::clone(&model);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: commitment set and fulfilled by the same step's own `shell`
    // call.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec1, "skipped_redundant"),
        "the first run must actually execute: {:?}",
        rec1.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert_eq!(shell_calls.load(Ordering::SeqCst), 1);
    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "step 1's own matching shell call should already have cleared `next`"
    );

    // Step 2: re-commits, then repeats the identical shell command -- 7.7
    // skips the call, but it matches the freshly-set commitment.
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

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "a skipped call that matches the commitment must discharge it (`next` must be null)"
    );

    // Step 3: an ordinary read must not be rejected by stale
    // `commitment_reads` bookkeeping (the discharge must have reset it the
    // same way a real fulfilment does). Its prompt is built from step 2's
    // observation, which is where the discharge prefix lands.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        rejected_reason(&rec3).is_none(),
        "the read after a discharge must not be rejected: {:?}",
        rec3.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);

    let prompt = last_prompt(&model_handle);
    assert!(
        prompt.contains("commitment discharged:"),
        "expected the observation to start with the discharge prefix, got: {prompt}"
    );
}

/// Negative case: a skipped call that does NOT match the open commitment
/// (different tool) must leave `next` set -- the discharge is narrow to the
/// call that was actually committed to.
#[tokio::test]
async fn skipped_call_not_matching_the_commitment_leaves_next_set() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Step 1: run `cargo test` with no commitment set (seeds
        // `last_verify`).
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Step 2: commit to `edit_file` (a different tool from the shell
        // command about to repeat), then repeat the identical shell
        // command -- 7.7 skips it, but it does not match the `edit_file`
        // commitment.
        step_output(
            serde_json::json!({"next": commitment_json("edit_file", Some("a.rs"), "fix the bug")}),
            shell_action("cargo test"),
        ),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill);
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

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("a skipped call that does not match the commitment must leave `next` set");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("a.rs"));
}

// =======================================================================
// B. shell/test commitments match on the tool alone.
// =======================================================================

/// A commitment `{tool: "shell", path: "some/file", what: ...}` (the model
/// wrote a `path` it copied from elsewhere -- a `shell` call carries no
/// `path` argument at all) is fulfilled by a plain `shell` call with no
/// `path` in its args.
#[tokio::test]
async fn shell_commitment_with_a_path_is_fulfilled_by_a_pathless_shell_call() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Step 1: commit to `shell` with a (spurious) `path`, no tool call.
        step_output(
            serde_json::json!({
                "next": commitment_json("shell", Some("some/file"), "run cargo fmt")
            }),
            read_action("some/file"),
        ),
        // Step 2: the shell call itself, with no `path` argument.
        step_output(serde_json::json!({}), shell_action("cargo fmt")),
        // Step 3: a no-op read, just so step 2's pending observation
        // (carrying the "commitment fulfilled" prefix) gets rendered into a
        // prompt the test can inspect.
        step_output(serde_json::json!({}), read_action("some/file")),
    ]);
    let model_handle = Arc::clone(&model);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next.as_ref().map(|c| c.tool.as_str()),
        Some("shell"),
        "the commitment should have been accepted"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !has_event(&rec2, "skipped_redundant"),
        "the shell call must actually run: {:?}",
        rec2.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert_eq!(shell_calls.load(Ordering::SeqCst), 1);

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "a pathless `shell` call must fulfil a `shell` commitment that named a path"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    let prompt = last_prompt(&model_handle);
    assert!(
        prompt.contains("commitment fulfilled:"),
        "expected the observation to start with the fulfilled prefix, got: {prompt}"
    );
}

/// Unaffected control: an `edit_file` commitment for `a.rs` is still a
/// mismatch when the actual `edit_file` call touches a different path
/// (`b.rs`) -- the path comparison for `edit_file`/`write_file` is
/// untouched by the shell/test carve-out.
#[tokio::test]
async fn edit_file_commitment_on_a_different_path_is_still_a_mismatch() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let read_calls = Arc::new(AtomicU64::new(0));
    let shell_calls = Arc::new(AtomicU64::new(0));

    let edit_action = |path: &str| serde_json::json!({"type": "tool", "name": "edit_file", "args": {"path": path}});

    let model = ScriptedModel::new(vec![
        step_output(
            serde_json::json!({"next": commitment_json("edit_file", Some("a.rs"), "fix a.rs")}),
            read_action("a.rs"),
        ),
        step_output(serde_json::json!({}), edit_action("b.rs")),
        // Step 3: a no-op read, just so step 2's pending observation
        // (carrying the mismatch note) gets rendered into a prompt the
        // test can inspect.
        step_output(serde_json::json!({}), read_action("a.rs")),
    ]);
    let model_handle = Arc::clone(&model);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&read_calls), Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(GuardSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("an edit_file call on a different path than the commitment must not clear `next`");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("a.rs"));

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    let prompt = last_prompt(&model_handle);
    assert!(
        prompt.contains("note: `next` still says"),
        "expected the mismatch note in the observation, got: {prompt}"
    );
}
