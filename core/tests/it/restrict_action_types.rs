//! Integration test for `restrict_action_types` wired through
//! `Session::activate`/`Active::step`: a skill that forbids `yield` (and
//! offers no delegates) must produce a system prompt whose "Exact shapes"
//! section has no `yield` example line, and a schema whose
//! `action.type` enum has no `"yield"` entry -- both are what a
//! grammar-constrained backend (Ollama's `format: "schema"`) and the pasted
//! schema in the system prompt actually see, so a decoder can no longer be
//! steered into an action the skill's own rules will reject anyway.
//!
//! Core cannot depend on `statie-skills`/`statie-store`/`statie-models`, so
//! (in the same spirit as `core/tests/parse_validate.rs` and
//! `core/tests/delegate.rs`) this test builds its own minimal `StateStore`
//! and `Model` stubs.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, CommittedState, Event, HarnessState, LoggedPatch,
    MemoryLog, Model, ModelError, NullSink, Outcome, PatchError, PatchStrategy, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>`, copied in spirit from
// `core/tests/delegate.rs`'s `TestStore`.
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
// A `Model` that records the system prompt text and the schema it was
// handed for every call, then answers with a scripted turn.
// ---------------------------------------------------------------------

struct RecordingModel {
    turns: Mutex<Vec<String>>,
    system_prompts: Mutex<Vec<String>>,
    schemas: Mutex<Vec<Schema>>,
}

impl RecordingModel {
    fn new(turns: Vec<String>) -> Arc<RecordingModel> {
        Arc::new(RecordingModel {
            turns: Mutex::new(turns),
            system_prompts: Mutex::new(Vec::new()),
            schemas: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Model for RecordingModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: true,
            // Deliberately false: for_harness only pastes the schema text
            // into the system prompt when schema_constrained is false, and
            // this test wants to inspect that pasted text too.
            schema_constrained: false,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        self.system_prompts
            .lock()
            .unwrap()
            .push(prompt.system().to_string());
        self.schemas.lock().unwrap().push(schema.clone());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// A workspace/skill that forbids `yield` (like
// `core/tests/parse_validate.rs`'s `NoYieldSkill`), offers no delegates,
// and allows `ask_user`.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
}

struct ReplaceStrategy;

impl PatchStrategy<Workspace> for ReplaceStrategy {
    type Patch = Workspace;

    fn name() -> &'static str {
        "replace"
    }

    fn describe() -> &'static str {
        "The patch is the entire new workspace value."
    }

    fn apply(_cur: &Workspace, patch: &Workspace) -> Result<Workspace, PatchError> {
        Ok(patch.clone())
    }
}

struct NoYieldSkill;

impl Skill for NoYieldSkill {
    type Workspace = Workspace;
    type Patch = ReplaceStrategy;

    fn name(&self) -> &str {
        "no-yield"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("no-yield", "a skill that forbids yield"))
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

    fn allows_yield(&self) -> bool {
        false
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

/// The recorded system prompt's "Exact shapes" block has no `yield` example
/// line, and the schema the model was handed has no `"yield"` in its
/// `action.type` enum, when the active skill forbids `yield`.
#[tokio::test]
async fn no_yield_skill_gets_no_yield_example_or_enum_entry() {
    let root = tempfile::tempdir().unwrap();
    let ask_user_turn =
        serde_json::json!({"patch": {}, "action": {"type": "ask_user", "message": "which?"}})
            .to_string();
    let model = RecordingModel::new(vec![ask_user_turn]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(NoYieldSkill);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Parked(_)), "{outcome:?}");

    // The system prompt text: no "yield" example shape.
    let system_prompts = model.system_prompts.lock().unwrap().clone();
    assert_eq!(system_prompts.len(), 1);
    let system_text = &system_prompts[0];
    assert!(
        !system_text.contains(r#""type": "yield""#),
        "system prompt should not contain a yield example shape:\n{system_text}"
    );
    // ask_user and done and tool are still allowed and should still appear.
    assert!(
        system_text.contains(r#""type": "ask_user""#),
        "{system_text}"
    );
    assert!(system_text.contains(r#""type": "done""#), "{system_text}");
    assert!(system_text.contains(r#""type": "tool""#), "{system_text}");

    // The schema itself: no "yield" in the action.type enum.
    let schemas = model.schemas.lock().unwrap().clone();
    assert_eq!(schemas.len(), 1);
    let enum_values: Vec<String> = schemas[0]
        .as_value()
        .pointer("/properties/action/properties/type/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should have action.type.enum")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !enum_values.contains(&"yield".to_string()),
        "{enum_values:?}"
    );
    assert!(enum_values.contains(&"tool".to_string()), "{enum_values:?}");
    assert!(
        enum_values.contains(&"ask_user".to_string()),
        "{enum_values:?}"
    );
    assert!(enum_values.contains(&"done".to_string()), "{enum_values:?}");
    assert!(
        !enum_values.contains(&"delegate".to_string()),
        "{enum_values:?}"
    );
}
