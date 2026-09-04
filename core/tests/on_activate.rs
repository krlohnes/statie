//! Integration tests for `Skill::on_activate`: a hook called once right
//! after an activation's state/harness commit (and the `activate` System
//! entry is logged), whose returned patch is applied as one extra
//! harness-trusted commit (source `"harness"`, reason `"on_activate"`) --
//! the same mechanics as `Skill::after_tool`, just triggered by activation
//! instead of a successful tool call. Added to fix the "stale `next_skill`
//! after a handoff" defect: a handoff is requested by setting `next_skill`
//! (paired with action `done`); the session consumes it by activating the
//! named skill next, but nothing else cleared the field, so the new
//! activation used to run with the old `next_skill` still committed.
//!
//! Fixture style copied from `core/tests/after_tool.rs` (core cannot depend
//! on `statie-skills`/`statie-store`/`statie-models`, so this test builds
//! its own minimal `StateStore`, `Model`, `Skill`, and `Tool` stubs).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, CommittedState, Event, HarnessState, LogEntry, LogKind,
    LoggedPatch, Model, ModelError, NullSink, Patch, PatchError, PatchStrategy, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, Tool, ToolCtx, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/after_tool.rs`).
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
// Test workspace: `tag: String` (what `on_activate`/`after_tool` write),
// `done`.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    tag: String,
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

/// A skill whose `on_activate` writes `{"tag": "activated:<name>"}` every
/// time it's called (i.e. whenever the workspace's `tag` isn't already
/// exactly that value -- see the field doc on `tag_on_activate` below for
/// why this is gated rather than unconditional).
struct HookSkill {
    /// When `true`, `on_activate` always returns a patch; when `false`,
    /// mirrors the default (`None`) so `default_skill_applies_nothing`
    /// exercises a real "no override" skill instead of one that always
    /// declines.
    tag_on_activate: bool,
}

impl Skill for HookSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "hooked"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("hooked", "a skill for on_activate tests"))
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

    fn on_activate(&self, ws: &Workspace) -> Option<Patch> {
        if !self.tag_on_activate {
            return None;
        }
        let tag = format!("activated:{}", self.name());
        if ws.tag == tag {
            None
        } else {
            Some(serde_json::json!({"tag": tag}))
        }
    }
}

/// A skill with no `on_activate` override at all (the default `None`).
struct PlainSkill;

impl Skill for PlainSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "plain"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("plain", "a skill with no on_activate hook"))
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

/// A skill whose `on_activate` always returns a patch, but whose `rules`
/// unconditionally rejects the candidate next state -- `apply_after_tool_
/// patch` still runs `Skill::rules` on the merged workspace before
/// committing (the same way it does for an `after_tool` patch), so this
/// always errors, exercising the "never fails activation" contract.
struct FailingHookSkill;

impl Skill for FailingHookSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "failing"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "failing",
            "a skill whose on_activate patch never applies",
        ))
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

    fn on_activate(&self, _ws: &Workspace) -> Option<Patch> {
        Some(serde_json::json!({"tag": "would-be-set"}))
    }

    fn rules(&self, _cur: &CommittedState<Workspace>, _next: &Workspace) -> Result<(), PatchError> {
        Err(PatchError::RuleViolation(
            "this skill's rules always reject, for the on_activate-error test".to_string(),
        ))
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

fn tool_set() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(OkTool));
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

fn find_on_activate_entry(log: &Arc<dyn SessionLog>) -> Option<LogEntry> {
    patch_entries(log)
        .into_iter()
        .find(|e| e.payload.get("reason").and_then(Value::as_str) == Some("on_activate"))
}

// =======================================================================
// 1. A skill whose `on_activate` returns a patch: applied once, right at
//    activation (before any step runs), with the right log entry fields.
// =======================================================================

#[tokio::test]
async fn on_activate_patch_is_applied_once_at_activation() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(HookSkill {
        tag_on_activate: true,
    });
    let mut active = session.activate(skill).await.unwrap();

    // No step has run yet -- the patch must already be committed by
    // activation itself.
    let (cur, version) = active.session().state().await.unwrap();
    assert_eq!(
        cur.agent.tag, "activated:hooked",
        "on_activate's patch should have landed in the committed workspace \
         right after activation, before any step"
    );
    // Version 0 (init) + 1 (the on_activate commit).
    assert_eq!(version.0, 1);

    let entry = find_on_activate_entry(&log).expect("expected an on_activate Patch log entry");
    assert_eq!(entry.kind, LogKind::Patch);
    assert_eq!(
        entry.payload.get("source").and_then(Value::as_str),
        Some("harness")
    );
    assert_eq!(
        entry.payload.get("reason").and_then(Value::as_str),
        Some("on_activate")
    );
    assert_eq!(
        entry.payload.get("patch"),
        Some(&serde_json::json!({"tag": "activated:hooked"}))
    );

    // Exactly one on_activate Patch entry: it must not run again on its
    // own (only re-activating would trigger it a second time).
    let count = patch_entries(&log)
        .iter()
        .filter(|e| e.payload.get("reason").and_then(Value::as_str) == Some("on_activate"))
        .count();
    assert_eq!(count, 1);
}

// =======================================================================
// 2. The default skill (no `on_activate` override): nothing is applied,
//    no Patch entry is logged, and the version advances by zero.
// =======================================================================

#[tokio::test]
async fn default_skill_applies_nothing_on_activate() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![]);

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

    let (cur, version) = active.session().state().await.unwrap();
    assert_eq!(cur.agent.tag, "");
    assert_eq!(
        version.0, 0,
        "no on_activate commit: activation itself is not a commit for a \
         fresh store"
    );
    assert!(find_on_activate_entry(&log).is_none());
}

// =======================================================================
// 3. A skill whose `on_activate` declines the second time (patch would be a
//    no-op): re-activating the same skill after its tag already matches
//    produces no second Patch entry.
// =======================================================================

#[tokio::test]
async fn on_activate_does_not_reapply_once_already_set() {
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
        tag_on_activate: true,
    });
    {
        let mut active = session.activate(Arc::clone(&skill)).await.unwrap();
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    assert_eq!(
        patch_entries(&log)
            .iter()
            .filter(|e| e.payload.get("reason").and_then(Value::as_str) == Some("on_activate"))
            .count(),
        1
    );

    // Re-activate the same skill: `tag` already equals what `on_activate`
    // would write, so `HookSkill::on_activate` returns `None` this time.
    let mut active2 = session.activate(skill).await.unwrap();
    let (cur2, _v2) = active2.session().state().await.unwrap();
    assert_eq!(cur2.agent.tag, "activated:hooked");

    let count = patch_entries(&log)
        .iter()
        .filter(|e| e.payload.get("reason").and_then(Value::as_str) == Some("on_activate"))
        .count();
    assert_eq!(
        count, 1,
        "on_activate must not commit a second entry when its own patch would be a no-op"
    );
}

// =======================================================================
// 4. An `on_activate` patch that fails to apply: activation still
//    succeeds (never fails), and a System `note` records the error instead
//    of a Patch entry.
// =======================================================================

#[tokio::test]
async fn on_activate_error_is_logged_as_a_note_and_does_not_fail_activation() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(FailingHookSkill);
    let active = session.activate(skill).await;
    assert!(
        active.is_ok(),
        "activation must succeed even when on_activate's patch fails to apply"
    );
    let mut active = active.unwrap();

    let (cur, version) = active.session().state().await.unwrap();
    assert_eq!(cur.agent.tag, "", "the failing patch must not have applied");
    assert_eq!(version.0, 0, "no commit landed for the failed patch");

    assert!(
        find_on_activate_entry(&log).is_none(),
        "no on_activate Patch entry should be logged when the patch failed to apply"
    );

    let has_note = log.entries(Some(LogKind::System), 100).iter().any(|e| {
        e.payload.get("event").and_then(Value::as_str) == Some("note")
            && e.payload
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("on_activate"))
    });
    assert!(
        has_note,
        "expected a System note recording the on_activate failure: {:?}",
        log.entries(Some(LogKind::System), 100)
    );
}
