//! Integration tests for `Skill::after_tool` (milestone 10): a hook called
//! by the session right after a tool call has executed successfully, whose
//! returned patch is applied as one extra harness-trusted commit (source
//! `"harness"`, reason `"after_tool"`). Fixture style copied from
//! `core/tests/read_paging.rs`/`core/tests/maintainer.rs` (core cannot
//! depend on `statie-skills`/`statie-store`/`statie-models`, so this test
//! builds its own minimal `StateStore`, `Model`, `Skill`, and `Tool`
//! stubs).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogEntry, LogKind, LoggedPatch, Model, ModelError, NullSink, Outcome, Patch, PatchError,
    PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog,
    SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet,
    Version,
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
// A scripted `Model`.
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
// Test workspace: `tag: String` (what `after_tool` writes), `next` (the
// commitment slot), `done`.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    tag: String,
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

/// A skill whose `after_tool` writes `{"tag": "<name>@<step>"}` whenever the
/// tool named `hook_tool` (given at construction) ran successfully; `None`
/// for every other tool.
struct HookSkill {
    hook_tool: &'static str,
}

impl Skill for HookSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "hooked"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("hooked", "a skill for after_tool tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "ok_tool".to_string(),
            "err_tool".to_string(),
            "search".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }

    fn after_tool(
        &self,
        _ws: &Workspace,
        _harness: &HarnessState,
        step: u64,
        name: &str,
        _args: &Value,
        _output: &str,
    ) -> Option<Patch> {
        if name == self.hook_tool {
            Some(serde_json::json!({"tag": format!("{name}@{step}")}))
        } else {
            None
        }
    }
}

/// A skill with no `after_tool` override at all (the default `None`).
struct PlainSkill;

impl Skill for PlainSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "plain"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("plain", "a skill with no after_tool hook"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["ok_tool".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
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

/// A tool that always succeeds with a fixed output.
struct OkTool;

#[async_trait]
impl Tool for OkTool {
    fn name(&self) -> &str {
        "ok_tool"
    }
    fn description(&self) -> &str {
        "always succeeds"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        Ok(statie_core::ToolOutputFull::new("ok".to_string()))
    }
}

/// A tool that always fails.
struct ErrTool;

#[async_trait]
impl Tool for ErrTool {
    fn name(&self) -> &str {
        "err_tool"
    }
    fn description(&self) -> &str {
        "always fails"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(
        &self,
        _args: Value,
        _ctx: &ToolCtx,
    ) -> Result<statie_core::ToolOutputFull, statie_core::ToolError> {
        Err(statie_core::ToolError::ExecutionFailed("boom".to_string()))
    }
}

/// A read-only tool (named `search`, in `READ_ONLY_TOOLS`) that always
/// succeeds with a fixed output -- used to exercise the redundant-
/// identical-call skip guard (7.7/7.8), which only fires for
/// `READ_ONLY_TOOLS`.
struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "always succeeds"
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

fn tool_set() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(OkTool));
    tools.register(Arc::new(ErrTool));
    tools.register(Arc::new(SearchTool));
    tools
}

fn tool_action(name: &str) -> Value {
    serde_json::json!({"type": "tool", "name": name, "args": {}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn patch_entries(log: &Arc<dyn SessionLog>) -> Vec<LogEntry> {
    log.entries(Some(LogKind::Patch), 100)
}

fn find_after_tool_entry(log: &Arc<dyn SessionLog>) -> Option<LogEntry> {
    patch_entries(log)
        .into_iter()
        .find(|e| e.payload.get("reason").and_then(Value::as_str) == Some("after_tool"))
}

// =======================================================================
// 1. A tool call that triggers the hook: workspace reflects the patch, the
//    log carries a Patch entry with source "harness" / reason "after_tool",
//    and the version advances by one extra beyond the step's own commit.
// =======================================================================

#[tokio::test]
async fn after_tool_patch_is_applied_as_one_extra_harness_commit() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        tool_action("ok_tool"),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(HookSkill {
        hook_tool: "ok_tool",
    });
    let mut active = session.activate(skill).await.unwrap();

    let (_, version_before) = active.session().state().await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let (cur_after, version_after) = active.session().state().await.unwrap();
    assert_eq!(
        cur_after.agent.tag, "ok_tool@1",
        "after_tool's patch should have landed in the committed workspace"
    );

    // The step's own commit (1) plus the after_tool commit (1) = 2 extra
    // versions beyond the starting version.
    assert_eq!(
        version_after.0,
        version_before.0 + 2,
        "expected one version for the model's own step-commit and one more \
         for the after_tool patch"
    );

    let entry = find_after_tool_entry(&log).expect("expected an after_tool Patch log entry");
    assert_eq!(entry.kind, LogKind::Patch);
    assert_eq!(
        entry.payload.get("source").and_then(Value::as_str),
        Some("harness")
    );
    assert_eq!(
        entry.payload.get("reason").and_then(Value::as_str),
        Some("after_tool")
    );
    assert_eq!(
        entry.payload.get("patch"),
        Some(&serde_json::json!({"tag": "ok_tool@1"}))
    );
}

// =======================================================================
// 2. A tool error: the hook is never called (no after_tool Patch entry,
//    workspace `tag` stays empty).
// =======================================================================

#[tokio::test]
async fn after_tool_hook_not_called_on_tool_error() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        tool_action("err_tool"),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(HookSkill {
        hook_tool: "err_tool",
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let (cur_after, _version) = active.session().state().await.unwrap();
    assert_eq!(
        cur_after.agent.tag, "",
        "after_tool must not run on a tool error"
    );
    assert!(
        find_after_tool_entry(&log).is_none(),
        "no after_tool Patch entry should be logged on a tool error"
    );
}

// =======================================================================
// 3. A skipped-redundant call: the hook is never called.
// =======================================================================

#[tokio::test]
async fn after_tool_hook_not_called_on_skipped_redundant_call() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    // Same read-only call ("search", args identical) twice in a row: the
    // second is recognized as redundant (7.7's guard, keyed on tool +
    // args) and skipped rather than re-run.
    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), tool_action("search")),
        step_output(serde_json::json!({}), tool_action("search")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(HookSkill {
        hook_tool: "search",
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let (cur1, _v1) = active.session().state().await.unwrap();
    assert_eq!(cur1.agent.tag, "search@1");

    // Step 2 repeats the identical call; if the hook wrongly ran again it
    // would overwrite `tag` to "search@2" instead of leaving it as-is.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let skipped = rec2.entries().iter().any(|e| {
        e.kind == LogKind::System
            && e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")
    });
    assert!(skipped, "second identical call should be skipped");
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (cur2, _v2) = active.session().state().await.unwrap();
    assert_eq!(
        cur2.agent.tag, "search@1",
        "after_tool must not re-run on a skipped-redundant call"
    );

    let entries = patch_entries(&log);
    let after_tool_count = entries
        .iter()
        .filter(|e| e.payload.get("reason").and_then(Value::as_str) == Some("after_tool"))
        .count();
    assert_eq!(
        after_tool_count, 1,
        "exactly one after_tool commit (from step 1); step 2 was skipped"
    );
}

// =======================================================================
// 4. The default skill (no `after_tool` override): nothing is applied.
// =======================================================================

#[tokio::test]
async fn default_skill_applies_nothing() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        tool_action("ok_tool"),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(PlainSkill);
    let mut active = session.activate(skill).await.unwrap();

    let (_, version_before) = active.session().state().await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec).unwrap();

    let (cur_after, version_after) = active.session().state().await.unwrap();
    assert_eq!(cur_after.agent.tag, "");
    assert_eq!(
        version_after.0,
        version_before.0 + 1,
        "no after_tool commit: only the step's own commit"
    );
    assert!(find_after_tool_entry(&log).is_none());
}
