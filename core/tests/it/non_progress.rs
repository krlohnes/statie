//! Integration tests for the mixed-shape non-progress guard
//! (`NON_PROGRESS_CAP`, `core/src/session.rs`): unlike `CONTINUE_REPEAT_CAP`
//! (which only catches a streak of the identical `Continue` note, or the
//! identical skipped call, recurring), this guard catches ANY run of
//! non-progress steps regardless of shape -- a live run alternated a
//! skipped repeat of the test command, an empty `ask_user`, and a premature
//! `done` for roughly 30 steps with neither of the existing caps ever
//! firing, since no single shape ever recurred three times running.
//!
//! Core cannot depend on `statie-skills`/`statie-store`/`statie-models`, so
//! this test builds its own minimal `StateStore`, `Model`, `Skill`, and
//! `Tool` stubs, copied in spirit from `core/tests/guards.rs` and
//! `core/tests/continue_action.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, CommittedState, Event, HarnessState, LogKind,
    LoggedPatch, Model, ModelError, NullSink, Outcome, Park, ParsedOutput, PatchError,
    PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog,
    SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet,
    ValidatedStep, Version, validate_default,
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
// (copied from `core/tests/guards.rs`/`core/tests/continue_action.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    // Every prompt's user text this model has been asked to complete, in
    // call order -- additive to the original fixture (unit 22 part (b))
    // so the repeated-tool-error guard's prefix text can be asserted
    // directly from the NEXT prompt, the same way `core/tests/
    // try_fixes.rs`'s `ScriptedModel::prompts()` does. Unused by (and
    // harmless to) every test that doesn't call `prompts()`.
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
// Test workspace + a JSON-merge-patch strategy (`Patch = Value`, so a raw
// patch round trips through it byte for byte -- copied in spirit from
// `core/tests/guards.rs`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    done: bool,
    /// Stands in for "the plan has an open (not-yet-done) step": a bare
    /// `done` sent while this is `true` is premature, mirroring the coding
    /// skills' plan-non-empty check.
    #[serde(default)]
    open_step: bool,
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

/// A skill whose `validate` override converts two shapes into the
/// harness-internal `Action::Continue` (mirroring the coding skills'
/// `skills/src/coding/mod.rs` premature-`done`/empty-`ask_user`
/// conversions, without depending on `statie-skills`):
///
/// - `ask_user` with an empty `message` -> `Continue` ("empty ask_user").
/// - `done` while `open_step` is `true` -> `Continue` ("done with an open
///   step").
///
/// Everything else (tool calls, a non-empty `ask_user`, `done` once
/// `open_step` is `false`) falls through to `validate_default` unchanged.
struct NonProgressSkill;

impl Skill for NonProgressSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "non-progress"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "non-progress",
            "a skill for the non-progress cap tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "shell".to_string(),
            "edit_file".to_string(),
            "lint".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn validate(
        &self,
        cur: &CommittedState<Workspace>,
        mut out: ParsedOutput<Workspace>,
    ) -> Result<ValidatedStep<Workspace>, PatchError> {
        if let Action::AskUser { message } = out.action()
            && message.trim().is_empty()
        {
            out.set_action(Action::Continue {
                note: "empty ask_user: `ask_user` had no question, so nothing was asked; your \
                       patch was kept. Keep working with a tool call."
                    .to_string(),
            });
        } else if matches!(out.action(), Action::Done) {
            // Peek at the merged (post-patch) workspace to see whether
            // `open_step` is still true, the same way the coding skills'
            // bare-`done` handling peeks at the merged plan.
            let merged_open = serde_json::from_value::<Value>(out.patch().0.clone())
                .ok()
                .and_then(|patch| MergePatch::apply(&cur.agent, &patch).ok())
                .map(|next| next.open_step)
                .unwrap_or(cur.agent.open_step);
            if merged_open {
                out.set_action(Action::Continue {
                    note: "done with an open step: the plan still has an open step; your \
                           patch was kept. Finish it before sending `done`."
                        .to_string(),
                });
            }
        }
        validate_default::<Workspace, MergePatch>(self, cur, out)
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
// Real tools: `shell(cmd)` returns a canned result (used both to execute
// normally and, via an identical repeat, to trigger the 7.7
// redundant-verification skip); `edit_file` is a trivial mutation.
// ---------------------------------------------------------------------

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

/// A tool named `edit_file` that always fails with the SAME SHAPE of error
/// (the exact wording `tools/src/edit_file.rs`'s `check_no_duplicate_
/// definition` reports for a duplicate-insert refusal), but with a
/// DIFFERENT line number each call, read from `args.line` -- used by unit
/// 25's own acceptance test to drive the repeated-tool-error guard's
/// identical-SHAPE-error streak (`error_shape`, `core/src/session.rs`) with
/// reasons that are never byte-identical to each other, only same-shaped.
struct ShapeVaryingEditFile;

#[async_trait]
impl Tool for ShapeVaryingEditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "always fails with a same-shape, different-line-number error"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        let line = args.get("line").and_then(Value::as_u64).unwrap_or(1);
        Err(statie_core::ToolError::ExecutionFailed(format!(
            "line {line} of src/lib.rs already has `pub fn new(...)`; your insert would \
             duplicate it. Use old/new to replace the existing code instead."
        )))
    }
}

/// A tool that always errors with the SAME `ExecutionFailed` reason,
/// regardless of its arguments -- used to drive the repeated-tool-error
/// guard's identical-error streak without depending on a specific tool's
/// argument-validation wording (unit 22 part (b)).
struct AlwaysErrorsTool;

#[async_trait]
impl Tool for AlwaysErrorsTool {
    fn name(&self) -> &str {
        "lint"
    }
    fn description(&self) -> &str {
        "always fails, for the repeated-tool-error guard tests"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        Err(statie_core::ToolError::ExecutionFailed(
            "lint config not found".to_string(),
        ))
    }
}

fn tool_set(shell_calls: Arc<AtomicU64>) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(CountingShell { calls: shell_calls }));
    tools.register(Arc::new(NoopEditFile));
    tools.register(Arc::new(AlwaysErrorsTool));
    tools
}

/// Same shape as `tool_set`, but registers `ShapeVaryingEditFile` in place
/// of the always-succeeding `NoopEditFile` -- used by unit 25's own
/// acceptance test, which needs `edit_file` itself to fail with a
/// same-shape, different-line-number error.
fn shape_varying_tool_set(shell_calls: Arc<AtomicU64>) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(CountingShell { calls: shell_calls }));
    tools.register(Arc::new(ShapeVaryingEditFile));
    tools.register(Arc::new(AlwaysErrorsTool));
    tools
}

fn shell_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "shell", "args": {"cmd": cmd}})
}

fn lint_action() -> Value {
    serde_json::json!({"type": "tool", "name": "lint", "args": {}})
}

fn edit_action() -> Value {
    serde_json::json!({"type": "tool", "name": "edit_file", "args": {"path": "f", "content": "x"}})
}

/// An `edit_file` call naming a specific `line` argument -- read by
/// `ShapeVaryingEditFile` to build a same-shape error citing that line
/// number, so consecutive calls with DIFFERENT `line`s still count as the
/// SAME error shape (unit 25's own acceptance test).
fn edit_action_with_line(line: u64) -> Value {
    serde_json::json!({
        "type": "tool",
        "name": "edit_file",
        "args": {"path": "src/lib.rs", "line": line},
    })
}

fn empty_ask_user_action() -> Value {
    serde_json::json!({"type": "ask_user", "message": ""})
}

fn done_action() -> Value {
    serde_json::json!({"type": "done"})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn non_progress_cap_shapes(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("non_progress_cap")
        })
        .and_then(|e| e.payload.get("shapes").and_then(Value::as_str))
        .map(str::to_string)
}

/// Six consecutive non-progress steps of alternating shapes -- a skipped
/// repeat of a `shell` command (7.7), an empty `ask_user` (converted to
/// `Continue`), and a premature `done` (also converted to `Continue`),
/// repeated -- reaches `NON_PROGRESS_CAP` (6) even though no single shape
/// ever recurs three times running (so neither `CONTINUE_REPEAT_CAP`-backed
/// guard fires first): steps 1-5 are `Outcome::Continue`, step 6 parks as
/// `Park::AskUser` with a message listing all six shapes in order, and logs
/// a `non_progress_cap` System entry with the same shapes list.
#[tokio::test]
async fn six_alternating_non_progress_shapes_park_with_shapes_in_order() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Step 0 (setup, not part of the streak): run `shell` once so step
        // 1 can repeat it identically and be skipped.
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Step 1: identical repeat -> skipped_redundant (non-progress #1).
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Step 2: empty ask_user -> Continue (non-progress #2).
        step_output(serde_json::json!({}), empty_ask_user_action()),
        // Step 3: premature done (open_step still true) -> Continue
        // (non-progress #3).
        step_output(serde_json::json!({}), done_action()),
        // Step 4: identical shell repeat again -> skipped_redundant
        // (non-progress #4). Nothing mutated since step 1's real run, so
        // 7.7 still treats this as redundant.
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Step 5: empty ask_user again -> Continue (non-progress #5).
        step_output(serde_json::json!({}), empty_ask_user_action()),
        // Step 6: premature done again -> Continue (non-progress #6) ->
        // NON_PROGRESS_CAP reached, parks.
        step_output(serde_json::json!({}), done_action()),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    // `open_step` starts `true` (default is `false` for bool, so seed it via
    // the workspace's initial value) -- use a skill whose initial workspace
    // sets it, since `Workspace::default()` would make `done` non-premature.
    let skill = Arc::new(NonProgressSkill);
    let mut active = session.activate(skill).await.unwrap();
    // Seed `open_step: true` via a direct patch commit so `done` (steps 3
    // and 6) is genuinely premature.
    active
        .apply_user_patch(serde_json::json!({"open_step": true}))
        .await
        .unwrap();

    let mut outcomes = Vec::new();
    for _ in 0..7 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        // `StepRecord` isn't `Clone` and its `outcome` is only reachable via
        // `append_record`, so pull out what's needed from `entries()` first.
        let is_non_progress_cap_entry = has_event(&rec, "non_progress_cap");
        let shapes = non_progress_cap_shapes(&rec);
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push((outcome, is_non_progress_cap_entry, shapes));
    }

    // Step 0 (setup): ordinary executed tool call.
    assert!(
        matches!(outcomes[0].0, Outcome::Continue),
        "step 0 (setup shell run): got {:?}",
        outcomes[0].0
    );

    // Steps 1-5 (0-indexed 1..=5): each is a non-progress conversion/skip,
    // none of them identical to the one right before it, so neither
    // `CONTINUE_REPEAT_CAP`-backed guard fires; every one is
    // `Outcome::Continue`.
    for (i, (outcome, ..)) in outcomes.iter().enumerate().take(6).skip(1) {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; alternating non-progress shapes must not trip the \
             identical-repeat caps before the mixed-shape cap does"
        );
    }

    // Step 6 (the sixth non-progress step in the streak): parks, with a
    // non_progress_cap log entry naming all six shapes in order.
    let (outcome6, logged_cap, shapes) = &outcomes[6];
    assert!(
        logged_cap,
        "step 6 should log a non_progress_cap System entry"
    );
    let shapes = shapes
        .as_ref()
        .expect("non_progress_cap entry should carry a shapes string");
    assert_eq!(
        shapes.matches(',').count(),
        5,
        "shapes list should name all 6 non-progress steps (5 commas): {shapes}"
    );
    match outcome6 {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("6 steps in a row without progress"),
                "park message should say 6 steps in a row without progress: {msg}"
            );
            assert!(
                msg.contains(shapes.as_str()),
                "park message should contain the shapes list: {msg}\nshapes: {shapes}"
            );
            assert!(
                msg.contains("Reply with what it should do next."),
                "park message should end with the standard reply prompt: {msg}"
            );
        }
        other => panic!("step 6: expected Parked(AskUser(_)), got {other:?}"),
    }
}

/// An executed tool call in the middle of the streak resets the count: five
/// non-progress steps, then a real `edit_file` call, then five more
/// non-progress steps -- never six *consecutive* non-progress steps, so the
/// cap never fires.
#[tokio::test]
async fn an_executed_tool_call_midway_resets_the_count_so_no_park_at_six() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Setup: run `shell` once so it can be repeated.
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Non-progress #1: skipped repeat.
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Non-progress #2: empty ask_user.
        step_output(serde_json::json!({}), empty_ask_user_action()),
        // Non-progress #3: premature done.
        step_output(serde_json::json!({}), done_action()),
        // Step 4 (the reset): a real, executed edit_file call.
        step_output(serde_json::json!({}), edit_action()),
        // Non-progress #1 again (streak restarted): empty ask_user.
        step_output(serde_json::json!({}), empty_ask_user_action()),
        // Non-progress #2: premature done.
        step_output(serde_json::json!({}), done_action()),
        // Non-progress #3: empty ask_user.
        step_output(serde_json::json!({}), empty_ask_user_action()),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(NonProgressSkill);
    let mut active = session.activate(skill).await.unwrap();
    active
        .apply_user_patch(serde_json::json!({"open_step": true}))
        .await
        .unwrap();

    let mut outcomes = Vec::new();
    for _ in 0..8 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        outcomes.push(SessionLogExt::append_record(log.as_ref(), rec).unwrap());
    }

    for (i, outcome) in outcomes.iter().enumerate() {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; the executed edit_file call at step 4 must reset the \
             non-progress streak so 6 consecutive non-progress steps never accumulate"
        );
    }
}

// ---------------------------------------------------------------------
// Unit 22 part (b): the repeated-tool-error guard. Identity is `(tool
// name, reason string)`, read from the `tool_error` System entry's own
// payload. At the second identical error, the observation is prefixed with
// "[harness] This is the same `{tool}` error as the previous step;
// repeating the call will not change it." At `CONTINUE_REPEAT_CAP` (3), the
// step parks through the mixed-shape non-progress guard's own park path: a
// `non_progress_cap` System entry, with a shape string
// "repeated tool error: {tool}" naming the tool.
// ---------------------------------------------------------------------

/// Three consecutive identical `lint` tool errors (same `(tool, reason)`
/// pair every time, since `AlwaysErrorsTool` always fails the same way)
/// park at the third: a `non_progress_cap` System entry is logged, its
/// `shapes` payload names the tool, and the park message says so too.
#[tokio::test]
async fn three_consecutive_identical_tool_errors_park_at_the_third() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), lint_action()),
        step_output(serde_json::json!({}), lint_action()),
        step_output(serde_json::json!({}), lint_action()),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        tool_set(Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(NonProgressSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    let mut cap_shapes: Vec<Option<String>> = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        cap_shapes.push(non_progress_cap_shapes(&rec));
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push(outcome);
    }

    // Steps 0 and 1: ordinary tool-error `Continue` outcomes (the cap has
    // not been reached yet).
    for (i, outcome) in outcomes.iter().enumerate().take(2) {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; the first two identical tool errors must not park"
        );
    }

    // `prompts[0]` is built before anything ran (no observation yet).
    // `prompts[1]` is built right after step 0's FIRST lint error (repeat
    // count 1, no prefix yet). `prompts[2]` is built right after step 1's
    // SECOND identical lint error (repeat count 2), and must carry the
    // "[harness] This is the same ... error" prefix line, verbatim.
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 3,
        "expected at least 3 prompts (one per step run, plus the initial one): {}",
        prompts.len()
    );
    assert!(
        !prompts[1].contains("This is the same `lint` error"),
        "prompts[1] (right after the FIRST lint error) must not carry the repeated-error \
         prefix yet:\n{}",
        prompts[1]
    );
    assert!(
        prompts[2].contains(
            "[harness] This is the same `lint` error as the previous step; repeating the call \
             will not change it."
        ),
        "prompts[2] (right after the SECOND identical lint error) should carry the \
         repeated-tool-error prefix line verbatim:\n{}",
        prompts[2]
    );

    // Step 2 (the third identical error): parks, with a non_progress_cap
    // log entry whose shapes name the tool.
    let shapes = cap_shapes[2]
        .as_ref()
        .expect("step 2 should log a non_progress_cap System entry with a shapes string");
    assert_eq!(
        shapes, "repeated tool error: lint",
        "the shape must name the tool exactly as the spec's example does"
    );
    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("3 steps in a row without progress"),
                "park message should say 3 steps in a row without progress: {msg}"
            );
            assert!(
                msg.contains("repeated tool error: lint"),
                "park message should contain the shape: {msg}"
            );
            assert!(
                msg.contains("Reply with what it should do next."),
                "park message should end with the standard reply prompt: {msg}"
            );
        }
        other => panic!("step 2: expected Parked(AskUser(_)), got {other:?}"),
    }

    // No non_progress_cap entry at steps 0 or 1.
    assert!(
        cap_shapes[0].is_none(),
        "step 0 must not log non_progress_cap"
    );
    assert!(
        cap_shapes[1].is_none(),
        "step 1 must not log non_progress_cap"
    );
}

/// Two identical `lint` tool errors separated by a successful `shell` call
/// do not park: the intervening success breaks the streak, the same as
/// `last_skip_key`'s own reset rule.
#[tokio::test]
async fn two_identical_tool_errors_separated_by_a_success_do_not_park() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        // Step 0: lint error #1.
        step_output(serde_json::json!({}), lint_action()),
        // Step 1: a successful shell call breaks the streak.
        step_output(serde_json::json!({}), shell_action("cargo test")),
        // Step 2: lint error again (streak restarted at 1, not 2).
        step_output(serde_json::json!({}), lint_action()),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(NonProgressSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let is_cap = has_event(&rec, "non_progress_cap");
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push((outcome, is_cap));
    }

    for (i, (outcome, is_cap)) in outcomes.iter().enumerate() {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; two identical tool errors separated by a success must \
             not park"
        );
        assert!(
            !is_cap,
            "step {i}: must not log non_progress_cap when the identical-error streak never \
             reaches the cap"
        );
    }
}

// ---------------------------------------------------------------------
// Unit 25: `error_shape` -- the repeated-tool-error guard keys on the SHAPE
// of the reason, not its exact text, so a model that re-issues the same
// KIND of failing edit with a different line number is still recognized as
// repeating itself.
// ---------------------------------------------------------------------

/// Three consecutive `edit_file` errors with the SAME SHAPE ("line # of
/// <path> already has `…`") but DIFFERENT line numbers (1, 2, 3) park at the
/// third, proving `error_shape` -- not the exact reason text -- is what the
/// repeated-tool-error guard's identity is keyed on. Before unit 25 this did
/// NOT park (each error's exact reason string differed, so `(tool, reason)`
/// never matched twice in a row): this test proves the "today they do not"
/// half explicitly by asserting `HarnessState.last_tool_error.count` goes
/// 1, 2, 3 across the three steps, not 1, 1, 1.
#[tokio::test]
async fn three_edit_file_errors_with_the_same_shape_but_different_line_numbers_park_at_the_third() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let shell_calls = Arc::new(AtomicU64::new(0));

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), edit_action_with_line(1)),
        step_output(serde_json::json!({}), edit_action_with_line(2)),
        step_output(serde_json::json!({}), edit_action_with_line(3)),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        shape_varying_tool_set(Arc::clone(&shell_calls)),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(NonProgressSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    let mut counts = Vec::new();
    let mut shapes = Vec::new();
    let mut reasons = Vec::new();
    let mut cap_shapes: Vec<Option<String>> = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        cap_shapes.push(non_progress_cap_shapes(&rec));
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push(outcome);

        let (state, _v) = active.session().state().await.unwrap();
        let err = state
            .harness
            .last_tool_error
            .clone()
            .expect("each edit_file error must set last_tool_error");
        counts.push(err.count);
        shapes.push(err.shape);
        reasons.push(err.reason);
    }

    // The count goes 1, 2, 3 -- proving the three DIFFERENT-line-number
    // errors are recognized as ONE streak (today, before unit 25, three
    // differently-worded reasons would never match `(tool, reason)` twice
    // in a row, so this would read 1, 1, 1 and never park).
    assert_eq!(
        counts,
        vec![1, 2, 3],
        "the repeated-tool-error guard must advance the SAME streak across all three \
         different-line-number errors, not start a fresh streak each time"
    );

    // The reason text itself is NOT identical across the three steps (each
    // names its own line number) -- proving the streak survived DESPITE the
    // exact text differing, because the SHAPE stayed the same.
    assert_eq!(
        reasons[0],
        "tool execution failed: line 1 of src/lib.rs already has `pub fn new(...)`; \
                     your insert would duplicate it. Use old/new to replace the existing code \
                     instead."
    );
    assert_eq!(
        reasons[2],
        "tool execution failed: line 3 of src/lib.rs already has `pub fn new(...)`; \
                     your insert would duplicate it. Use old/new to replace the existing code \
                     instead."
    );
    assert_ne!(
        reasons[0], reasons[2],
        "the three errors' exact reason text must differ (different line numbers) even though \
         they are the SAME shape"
    );

    // All three shapes are identical -- the SAME shape recorded every time.
    // Note `error_shape` treats `old/new` (from the tool's own message text)
    // as path-like too (it contains `/`, the same rule that normalizes a
    // real file path), which is why `<path>` appears twice below -- and the
    // 120-character cut lands mid-word ("exist..."), matching `error_shape`'s
    // own doc'd behaviour exactly.
    assert_eq!(shapes[0], shapes[1]);
    assert_eq!(shapes[1], shapes[2]);
    assert_eq!(
        shapes[0],
        "tool execution failed: line # of <path> already has `…`; your insert would duplicate \
         it. Use <path> to replace the exist",
        "the recorded shape must be error_shape's own normalized form, cut to 120 characters"
    );
    assert_eq!(shapes[0].chars().count(), 120);

    // Steps 0 and 1: ordinary `Continue` outcomes (the cap has not been
    // reached yet).
    assert!(
        matches!(outcomes[0], Outcome::Continue),
        "step 0: got {:?}",
        outcomes[0]
    );
    assert!(
        matches!(outcomes[1], Outcome::Continue),
        "step 1: got {:?}",
        outcomes[1]
    );
    assert!(
        cap_shapes[0].is_none(),
        "step 0 must not log non_progress_cap"
    );
    assert!(
        cap_shapes[1].is_none(),
        "step 1 must not log non_progress_cap"
    );

    // Step 2 (the third same-shape error): parks, with a non_progress_cap
    // log entry naming the tool.
    let shape = cap_shapes[2]
        .as_ref()
        .expect("step 2 should log a non_progress_cap System entry with a shapes string");
    assert_eq!(shape, "repeated tool error: edit_file");
    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("3 steps in a row without progress"),
                "park message should say 3 steps in a row without progress: {msg}"
            );
            assert!(
                msg.contains("repeated tool error: edit_file"),
                "park message should contain the shape: {msg}"
            );
        }
        other => panic!("step 2: expected Parked(AskUser(_)), got {other:?}"),
    }
}
