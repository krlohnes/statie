//! Integration tests for `Active::apply_user_patch` (`/patch` /
//! `--patch`): an operator-authored patch applied directly by the harness,
//! bypassing the model entirely. Core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models`, so this test builds its
//! own minimal `StateStore`, `Model`, and `Skill` stubs, in the same spirit
//! as `core/tests/delegate.rs`.

use std::sync::Mutex;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    ApplyFn, Budgets, Capabilities, CommittedState, HarnessState, LogKind, LoggedPatch, Model,
    ModelError, PatchError, PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig,
    SessionError, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError,
    ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied in spirit from
// `core/tests/delegate.rs`; core has no test-only shared crate to put this
// in).
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
        action: &statie_core::Action,
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
// A model that is never called: `apply_user_patch` never reaches the
// model, and every test here only calls it directly. Present only because
// `Session::new` requires one.
// ---------------------------------------------------------------------

struct UnusedModel;

#[async_trait]
impl Model for UnusedModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: false,
            schema_constrained: false,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        _prompt: statie_core::BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        Err(ModelError::Backend(
            "UnusedModel should never be called".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------
// Test workspace: a real JSON-merge-patch strategy (top-level keys only,
// like `statie_patch::JsonMergePatch`, reimplemented here since core
// cannot depend on that crate) so a partial `/patch` only touches the
// key(s) it names -- needed to exercise the "keys [a, b]" reporting.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    notes: String,
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
                cur_map.insert(k.clone(), v.clone());
            }
        } else {
            return Err(PatchError::Malformed("patch must be an object".to_string()));
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

/// A skill whose only rule rejects `notes` longer than 10 characters --
/// gives the rule-violation test something to trip on the *patched* value.
struct PatchSkill;

impl Skill for PatchSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "patchable"
    }

    fn spec(&self) -> std::sync::Arc<SkillSpec> {
        std::sync::Arc::new(SkillSpec::new("patchable", "a skill for /patch tests"))
    }

    fn initial_workspace(&self) -> Self::Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn rules(
        &self,
        _cur: &CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        if next.notes.len() > 10 {
            return Err(PatchError::RuleViolation(
                "notes must be at most 10 characters".to_string(),
            ));
        }
        Ok(())
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 1000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn make_session(root: &tempfile::TempDir) -> (Session<Workspace>, std::sync::Arc<dyn SessionLog>) {
    let log: std::sync::Arc<dyn SessionLog> = std::sync::Arc::new(statie_core::MemoryLog::new());
    let session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        std::sync::Arc::clone(&log),
        std::sync::Arc::new(UnusedModel) as std::sync::Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    );
    (session, log)
}

// ---------------------------------------------------------------------
// 1. A valid patch commits, advances the version, logs a Patch entry with
//    source: "user" and a system entry {"event": "user_patch"}, and sets
//    the pending observation text.
// ---------------------------------------------------------------------

#[tokio::test]
async fn apply_user_patch_commits_logs_and_sets_observation() {
    let root = tempfile::tempdir().unwrap();
    let (mut session, log) = make_session(&root);

    let skill = std::sync::Arc::new(PatchSkill);
    let mut active = session
        .activate(std::sync::Arc::clone(&skill))
        .await
        .unwrap();

    let (_, version_before) = active.session().state().await.unwrap();
    assert_eq!(version_before, Version(0));

    let new_version = active
        .apply_user_patch(serde_json::json!({"counter": 5}))
        .await
        .unwrap();
    assert_eq!(new_version, Version(1));

    // State actually changed.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(state.agent.counter, 5);
    assert_eq!(state.harness.step, 1);

    // Patch log entry carries source: "user".
    let patches = active.session().patch_log().await.unwrap();
    assert_eq!(patches.len(), 1);

    let entries = log.entries(None, 100);
    let patch_entry = entries
        .iter()
        .find(|e| e.kind == LogKind::Patch)
        .expect("expected a Patch log entry");
    assert_eq!(
        patch_entry.payload.get("source").and_then(Value::as_str),
        Some("user")
    );
    assert_eq!(
        patch_entry.payload.get("version").and_then(Value::as_u64),
        Some(1)
    );

    let system_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("user_patch"))
        .expect("expected a system entry with event user_patch");
    assert_eq!(system_entry.kind, LogKind::System);
    assert_eq!(
        system_entry.payload.get("keys").and_then(Value::as_array),
        Some(&vec![Value::String("counter".to_string())])
    );

    // The observation text itself is asserted in
    // `apply_user_patch_observation_is_a_plain_mention_not_a_rejection`
    // below (via a prompt-recording model), so a failure there doesn't
    // also obscure this test's state/log assertions.
    let _ = active;
}

// ---------------------------------------------------------------------
// 2. The pending observation text reaches the next prompt verbatim as a
//    plain mention ("operator patched the workspace: keys [...]"), not a
//    rejection.
// ---------------------------------------------------------------------

struct RecordingModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl RecordingModel {
    fn new(turns: Vec<String>) -> std::sync::Arc<RecordingModel> {
        std::sync::Arc::new(RecordingModel {
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

#[tokio::test]
async fn apply_user_patch_observation_is_a_plain_mention_not_a_rejection() {
    let root = tempfile::tempdir().unwrap();
    let log: std::sync::Arc<dyn SessionLog> = std::sync::Arc::new(statie_core::MemoryLog::new());
    let model = RecordingModel::new(vec![
        serde_json::json!({"patch": {}, "action": {"type": "yield"}}).to_string(),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        std::sync::Arc::clone(&log),
        std::sync::Arc::clone(&model) as std::sync::Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = std::sync::Arc::new(PatchSkill);
    let mut active = session.activate(skill).await.unwrap();

    active
        .apply_user_patch(serde_json::json!({"counter": 7}))
        .await
        .unwrap();

    let rec = active
        .step(statie_core::Event::Tick, &mut statie_core::NullSink)
        .await
        .unwrap();
    let _ = log.append_record(rec).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1);
    let obs_idx = prompts[0]
        .find("## Observation")
        .expect("prompt should carry an Observation section");
    let heading = "## Observation\n";
    let observation_body = prompts[0][obs_idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading");
    assert!(
        observation_body.starts_with("operator patched the workspace: keys"),
        "got: {observation_body}"
    );
    assert!(
        observation_body.contains("counter"),
        "got: {observation_body}"
    );
    assert!(
        !observation_body.to_lowercase().contains("rejected"),
        "observation should read as a plain mention, not a rejection: {observation_body}"
    );
}

// ---------------------------------------------------------------------
// 3. A rule violation is rejected and leaves state (and version)
//    unchanged: the user still cannot break the skill's invariants.
// ---------------------------------------------------------------------

#[tokio::test]
async fn apply_user_patch_rejects_rule_violation_without_changing_state() {
    let root = tempfile::tempdir().unwrap();
    let (mut session, log) = make_session(&root);

    let skill = std::sync::Arc::new(PatchSkill);
    let mut active = session
        .activate(std::sync::Arc::clone(&skill))
        .await
        .unwrap();

    let err = active
        .apply_user_patch(serde_json::json!({"notes": "way too many characters"}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, SessionError::Patch(PatchError::RuleViolation(_))),
        "expected a RuleViolation, got {err:?}"
    );

    // State is completely unchanged: still version 0, still the initial
    // workspace.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));
    assert_eq!(state.agent.notes, "");
    assert_eq!(state.harness.step, 0);

    // Nothing was logged: no Patch entry, no user_patch system entry.
    let patches = active.session().patch_log().await.unwrap();
    assert!(patches.is_empty(), "{patches:?}");
    let entries = log.entries(None, 100);
    assert!(
        !entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("user_patch")),
        "no user_patch entry should be logged on a rejected patch"
    );
}
