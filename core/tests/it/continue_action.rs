//! Integration test for the harness-internal `Action::Continue` (milestone:
//! "commit the patch, do not perform the action, tell the model in one line
//! what is open" -- see `core/src/model.rs`'s doc on the variant and
//! `core/src/session.rs`'s handling of it in `Active::execute`).
//!
//! Core cannot depend on `statie-skills`/`statie-store`/`statie-models`, so
//! this test builds its own minimal `StateStore`, `Model`, `Skill`, and
//! `Tool` stubs, in the same spirit as `core/tests/guards.rs` (the
//! `TestStore`/`ScriptedModel`/`Workspace`/`MergePatch` fixtures below are
//! copied from there, trimmed to what this file needs).

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
    Usage, ValidatedStep, Version, validate_default,
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
// A scripted `Model` that replays a fixed sequence of raw step outputs and
// records every prompt's user text (copied from `core/tests/guards.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
    /// Usage to attach to each turn's `RawOutput`, by index; a turn beyond
    /// this list (or an entry of `None`) reports no usage, matching
    /// `ScriptedModel::new`'s previous no-usage behaviour.
    usages: Mutex<Vec<Option<Usage>>>,
}

impl ScriptedModel {
    fn new(turns: Vec<Value>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
            usages: Mutex::new(Vec::new()),
        })
    }

    /// Like `new`, but each turn also reports the corresponding `Usage`
    /// (`None` for a turn with no server-reported usage).
    fn new_with_usage(turns: Vec<Value>, usages: Vec<Option<Usage>>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
            usages: Mutex::new(usages),
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
        let mut usages = self.usages.lock().unwrap();
        let usage = if usages.is_empty() {
            None
        } else {
            usages.remove(0)
        };
        let out = RawOutput::new(prompt.version(), None, content);
        Ok(match usage {
            Some(u) => out.with_usage(u),
            None => out,
        })
    }
}

// ---------------------------------------------------------------------
// Test workspace: `value: i64` (proves the patch landed), `next:
// Option<Value>` (a stand-in commitment-shaped field, used to prove `next`
// is untouched by a `Continue` step), `done: bool`. A real JSON-merge-patch
// strategy over top-level keys (reimplemented here, as `guards.rs` does,
// since core cannot depend on `statie_patch`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    next: Option<String>,
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

/// A skill whose `validate` override converts every incoming step's action
/// into the harness-internal `Action::Continue`, regardless of what the
/// model sent -- standing in for the coding skills' premature-`done`/
/// empty-`ask_user` conversions (`skills/src/coding/mod.rs`) without
/// depending on `statie-skills`. The note always names the workspace's
/// current `value`, so a test can tell which step's conversion produced the
/// observation it's reading.
struct ContinueSkill;

impl Skill for ContinueSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "continuer"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("continuer", "a skill that always continues"))
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

    fn validate(
        &self,
        cur: &CommittedState<Workspace>,
        mut out: ParsedOutput<Workspace>,
    ) -> Result<ValidatedStep<Workspace>, PatchError> {
        // Peek at the merged (post-patch) value the same way the coding
        // skills' `premature_done_note` does, so the note can name it.
        let merged_value = serde_json::from_value::<Value>(out.patch().0.clone())
            .ok()
            .and_then(|patch| MergePatch::apply(&cur.agent, &patch).ok())
            .map(|next| next.value)
            .unwrap_or(cur.agent.value);
        out.set_action(Action::Continue {
            note: format!("workspace value is now {merged_value}: keep going"),
        });
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

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn continued_note(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("continued")
        })
        .and_then(|e| e.payload.get("note").and_then(Value::as_str))
        .map(str::to_string)
}

/// The scripted skill's `validate` always returns `Action::Continue`, no
/// matter what the model sent (here: a `done` action, alongside a patch
/// that both bumps `value` and sets `next`). Asserts every piece of the
/// contract in the unit: the patch landed in the committed state (`value`
/// changed), no tool ran (no `tool_output`/`tool_error`/`skipped_redundant`
/// entry, and the tool-call counter -- there is none active here, so
/// absence of any such entry stands in for it), `next` was untouched by the
/// harness (the model's own patch set it; a `Continue` step must not clear
/// it the way a spent commitment does), the next prompt's observation
/// contains the note, and a `continued` System entry exists.
#[tokio::test]
async fn continue_commits_patch_runs_no_tool_and_surfaces_note_as_next_observation() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(
            serde_json::json!({"value": 7, "next": "edit a.rs"}),
            serde_json::json!({"type": "done"}),
        ),
        // A harmless second turn so the model's second prompt (built from
        // the `pending_observation` the first step set) can be captured.
        step_output(serde_json::json!({}), serde_json::json!({"type": "yield"})),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model.clone() as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ContinueSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();

    // Inspect everything `rec1` carries (entries) before it's consumed by
    // `append_record` below, since `StepRecord` is not `Clone` and its
    // `outcome` is only reachable through that call.

    // 2. No tool ran: no tool_output/tool_error/skipped_redundant entry.
    for event in ["tool_output", "tool_error", "skipped_redundant"] {
        assert!(
            !has_event(&rec1, event),
            "a Continue step must run no tool; found `{event}` in {:?}",
            rec1.entries()
        );
    }
    assert!(
        !rec1.entries().iter().any(|e| e.kind == LogKind::ToolOutput),
        "a Continue step must log no ToolOutput entry: {:?}",
        rec1.entries()
    );

    // 4. The step's action logged in the session log is `Action::Continue`
    // (round tripped through `serde_json::to_value`), and a `continued`
    // System entry exists with the note.
    let action_entry = rec1
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::Action)
        .expect("a Patch step logs an Action entry");
    assert_eq!(
        action_entry.payload.get("type").and_then(Value::as_str),
        Some("continue")
    );
    let logged_note = action_entry
        .payload
        .get("note")
        .and_then(Value::as_str)
        .expect("Action::Continue's note field");
    assert_eq!(logged_note, "workspace value is now 7: keep going");

    let note = continued_note(&rec1).expect("a `continued` System entry must be logged");
    assert_eq!(note, "workspace value is now 7: keep going");

    // The outcome for this step is `Outcome::Continue` (consumes `rec1`).
    let outcome1 = SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "got {outcome1:?}; a Continue step must yield Outcome::Continue, not a handoff/park"
    );

    // 1. The patch landed in the committed state.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(
        state.agent.value, 7,
        "the model's patch must have committed even though its `done` action was replaced"
    );

    // 3. `next` was untouched by the harness: the model's own patch set it
    // to "edit a.rs"; a spent-commitment clear (session.rs step 10) never
    // fires for a Continue step since that logic is gated on `Action::Tool`.
    assert_eq!(
        state.agent.next.as_deref(),
        Some("edit a.rs"),
        "next must be exactly what the model's patch set, untouched by the harness"
    );

    // 5. The next prompt's observation contains the note.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "two model calls should have happened");
    assert!(
        prompts[1].contains("workspace value is now 7: keep going"),
        "the second prompt's observation should contain the Continue note:\n{}",
        prompts[1]
    );
}

/// The maintainer runs after a `Continue` step exactly as it does after any
/// other `Outcome::Continue` step (session.rs step 11: gated on `outcome`,
/// not on which `Action` produced it) -- asserted here via the `notes`
/// field the (mock) maintainer would touch being harness-owned and thus
/// left alone by the model's own patch, proving the step reached that far
/// without erroring on a real (non-mock) maintainer being absent (the
/// default `Session` has none configured, so "the maintainer runs" here
/// means "the step completes normally past the gate", not a literal
/// maintainer call assertion -- see `core/tests/maintainer.rs` for that).
#[tokio::test]
async fn continue_step_completes_without_a_configured_maintainer() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({"value": 1}),
        serde_json::json!({"type": "done"}),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ContinueSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
}

// ---------------------------------------------------------------------
// Repeated-`Continue`-note guard (`CONTINUE_REPEAT_CAP`, `core/src/
// session.rs`): the same `Continue` note produced three steps running parks
// as `Park::AskUser` instead of continuing forever. `ContinueSkill`'s
// `validate` derives its note from the merged `value`, so sending the same
// `value` on consecutive steps reproduces the identical-note case, and a
// different `value` in between produces a different note and resets the
// streak.
// ---------------------------------------------------------------------

/// Runs `n` scripted `done` turns, each patching `value` to the
/// corresponding entry of `values` (so the note `ContinueSkill::validate`
/// derives is fully controlled by the caller), and returns every step's
/// outcome in order.
async fn run_continue_sequence(values: &[i64]) -> Vec<Outcome> {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let turns: Vec<Value> = values
        .iter()
        .map(|v| {
            step_output(
                serde_json::json!({"value": v}),
                serde_json::json!({"type": "done"}),
            )
        })
        .collect();
    let model = ScriptedModel::new(turns);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ContinueSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    for _ in values {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        outcomes.push(SessionLogExt::append_record(log.as_ref(), rec).unwrap());
    }
    outcomes
}

/// Steps 1 and 2 with the identical note (`value` unchanged) both yield
/// `Outcome::Continue`; step 3 -- the note recurring a third time in a row
/// -- parks as `Outcome::Parked(Park::AskUser(msg))`, with `msg` containing
/// the note text and "3 times in a row".
#[tokio::test]
async fn third_consecutive_identical_continue_note_parks_as_ask_user() {
    let outcomes = run_continue_sequence(&[1, 1, 1]).await;
    assert_eq!(outcomes.len(), 3);

    assert!(
        matches!(outcomes[0], Outcome::Continue),
        "step 1: got {:?}",
        outcomes[0]
    );
    assert!(
        matches!(outcomes[1], Outcome::Continue),
        "step 2: got {:?}",
        outcomes[1]
    );

    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("workspace value is now 1: keep going"),
                "park message should contain the repeated note: {msg}"
            );
            assert!(
                msg.contains("3 times in a row"),
                "park message should say it repeated 3 times in a row: {msg}"
            );
        }
        other => panic!("step 3: expected Parked(AskUser(_)), got {other:?}"),
    }
}

/// A different note in between resets the count: A, A, B, A, A -> every step
/// is `Outcome::Continue` (the cap is never reached because no note recurs
/// three times running).
#[tokio::test]
async fn a_different_note_in_between_resets_the_repeat_count() {
    // value 1 => note "A"-shaped (repeated), value 2 => note "B"-shaped.
    let outcomes = run_continue_sequence(&[1, 1, 2, 1, 1]).await;
    assert_eq!(outcomes.len(), 5);
    for (i, outcome) in outcomes.iter().enumerate() {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {}: got {outcome:?}; a different note in between must reset the streak so the \
             cap is never reached",
            i + 1
        );
    }
}

/// Four consecutive identical notes: the third step parks (as in the
/// dedicated test above), and after answering the park with a user event the
/// streak has been reset (`core/src/session.rs`'s `Action::Continue` arm
/// clears it right before parking) -- so a fourth step whose note repeats
/// yet again does NOT immediately re-park; it must build back up to three
/// before parking again. This guards against an off-by-one that would park
/// on every subsequent step once the cap is first hit.
#[tokio::test]
async fn streak_resets_after_parking_so_a_single_further_repeat_does_not_repark() {
    let outcomes = run_continue_sequence(&[1, 1, 1, 1]).await;
    assert_eq!(outcomes.len(), 4);
    assert!(matches!(outcomes[0], Outcome::Continue));
    assert!(matches!(outcomes[1], Outcome::Continue));
    assert!(matches!(outcomes[2], Outcome::Parked(Park::AskUser(_))));
    assert!(
        matches!(outcomes[3], Outcome::Continue),
        "step 4: got {:?}; the streak must have reset when step 3 parked, so a single further \
         repeat should not immediately re-park",
        outcomes[3]
    );
}

// ---------------------------------------------------------------------
// A plain skill (default `Skill::validate`, no override) to exercise the
// no-`action` recovery (`core/src/skill.rs`'s `ParsedOutput::
// parse_with_fixes`) end to end through the session, as opposed to
// `ContinueSkill` above, whose `validate` override always converts to
// `Continue` regardless of what the model sent.
// ---------------------------------------------------------------------

struct PlainSkill;

impl Skill for PlainSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "plain"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("plain", "a skill with no validate override"))
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
}

/// The session-level repeat cap: a model output with a well-formed `patch`
/// and no `action` at all -- the exact live shape that hit the retry cap
/// four times in a row before this recovery existed -- parses as
/// `Action::Continue` (see `core/tests/parse_validate.rs` for the parser-
/// level assertions) and, sent identically three times running through a
/// plain skill with no `validate` override, hits `CONTINUE_REPEAT_CAP` the
/// same as any other repeated `Continue` note: steps 1-2 are
/// `Outcome::Continue`, step 3 parks as `Park::AskUser` with the exact
/// no-`action` note and "3 times in a row" in the message.
#[tokio::test]
async fn three_identical_no_action_outputs_in_a_row_park_via_repeat_cap() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    // No `action` key at all -- `ParsedOutput::parse_with_fixes` recovers
    // this as `Action::Continue` rather than rejecting it.
    let no_action_output = || serde_json::json!({"patch": {"value": 1}});
    let model = ScriptedModel::new(vec![
        no_action_output(),
        no_action_output(),
        no_action_output(),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(PlainSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        outcomes.push(SessionLogExt::append_record(log.as_ref(), rec).unwrap());
    }

    assert!(
        matches!(outcomes[0], Outcome::Continue),
        "step 1: got {:?}",
        outcomes[0]
    );
    assert!(
        matches!(outcomes[1], Outcome::Continue),
        "step 2: got {:?}",
        outcomes[1]
    );
    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains(
                    "your output had no `action`; the patch was kept. Every output needs an \
                     action: a tool call, `done`, or `ask_user`."
                ),
                "park message should contain the no-action note: {msg}"
            );
            assert!(
                msg.contains("3 times in a row"),
                "park message should say it repeated 3 times in a row: {msg}"
            );
        }
        other => panic!("step 3: expected Parked(AskUser(_)), got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// The `patch` log entry's `usage` payload (core/src/session.rs, `Active::
// commit`/`execute`): built from `serde_json::to_value(self.session.
// last_usage)`, so `Usage`'s own `#[serde(skip_serializing_if =
// "Option::is_none")]` on `prompt_eval_secs`/`eval_secs` governs whether the
// keys appear at all (unit 35: "every step records how much of its prompt
// was evaluated").
// ---------------------------------------------------------------------

/// A step whose model call reports `Usage` with both timing fields set logs
/// them, by exact key and value, in the `patch` entry's `usage` object.
#[tokio::test]
async fn patch_entry_usage_includes_prompt_eval_secs_when_reported() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new_with_usage(
        vec![step_output(
            serde_json::json!({"value": 1}),
            serde_json::json!({"type": "done"}),
        )],
        vec![Some(Usage {
            prompt_tokens: 6136,
            completion_tokens: 80,
            prompt_eval_secs: Some(2.4),
            eval_secs: Some(2.0),
        })],
    );

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ContinueSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let patch_entry = rec
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::Patch)
        .expect("a committed step logs a Patch entry")
        .clone();
    let _ = SessionLogExt::append_record(log.as_ref(), rec).unwrap();

    let usage = patch_entry
        .payload
        .get("usage")
        .expect("Patch entry must carry a usage object when the model call reported one");
    assert_eq!(
        usage,
        &serde_json::json!({
            "prompt_tokens": 6136,
            "completion_tokens": 80,
            "prompt_eval_secs": 2.4,
            "eval_secs": 2.0
        }),
        "patch entry usage payload: {usage}"
    );
}

/// A step whose model call reports `Usage` with no timing (both `None`)
/// logs `prompt_tokens`/`completion_tokens` but omits `prompt_eval_secs`
/// and `eval_secs` entirely from the `patch` entry's `usage` object --
/// `skip_serializing_if` on `Usage`, not a `null` value.
#[tokio::test]
async fn patch_entry_usage_omits_timing_keys_when_not_reported() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new_with_usage(
        vec![step_output(
            serde_json::json!({"value": 1}),
            serde_json::json!({"type": "done"}),
        )],
        vec![Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 10,
            prompt_eval_secs: None,
            eval_secs: None,
        })],
    );

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        statie_core::ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ContinueSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let patch_entry = rec
        .entries()
        .iter()
        .find(|e| e.kind == LogKind::Patch)
        .expect("a committed step logs a Patch entry")
        .clone();
    let _ = SessionLogExt::append_record(log.as_ref(), rec).unwrap();

    let usage = patch_entry
        .payload
        .get("usage")
        .expect("Patch entry must carry a usage object when the model call reported one");
    assert_eq!(
        usage,
        &serde_json::json!({"prompt_tokens": 100, "completion_tokens": 10}),
        "patch entry usage payload must omit prompt_eval_secs/eval_secs keys entirely: {usage}"
    );
    assert!(
        usage.get("prompt_eval_secs").is_none(),
        "prompt_eval_secs key must be absent, not null: {usage}"
    );
    assert!(
        usage.get("eval_secs").is_none(),
        "eval_secs key must be absent, not null: {usage}"
    );
}

// ---------------------------------------------------------------------
// `HarnessState::consecutive_continues` (unit 37, `core/src/version.rs`):
// the streak of consecutive committed steps whose FINAL action (after
// `Skill::validate`) was the harness-internal `Action::Continue`. Set at
// the same commit site as `consecutive_non_question_asks`
// (`core/src/session.rs`'s `step`), but keyed on the post-`validate`
// action rather than the model's raw one.
// ---------------------------------------------------------------------

/// A no-op `edit_file` tool, so a scripted `Action::Tool` step can actually
/// commit (rather than erroring for want of a registered tool).
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

/// A skill whose `validate` converts a `done` action into the
/// harness-internal `Action::Continue` (the same shape `ContinueSkill`
/// always produces above), but leaves any OTHER action -- in particular a
/// real `Action::Tool` call -- untouched, so a scripted sequence of
/// `done`/tool-call turns lands whatever mix of final actions a test wants
/// to drive `consecutive_continues` with.
struct ToggleSkill;

impl Skill for ToggleSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "toggle"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("toggle", "done converts to continue"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["edit_file".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn validate(
        &self,
        cur: &CommittedState<Workspace>,
        mut out: ParsedOutput<Workspace>,
    ) -> Result<ValidatedStep<Workspace>, PatchError> {
        if matches!(out.action(), Action::Done) {
            out.set_action(Action::Continue {
                note: "still open, keep going".to_string(),
            });
        }
        validate_default::<Workspace, MergePatch>(self, cur, out)
    }
}

async fn harness_state(active: &mut statie_core::Active<'_, ToggleSkill>) -> HarnessState {
    let (state, _version) = active.session().state().await.unwrap();
    state.harness
}

/// A scripted session whose committed actions are `Continue, Continue,
/// tool, Continue` (`done`, `done`, `edit_file`, `done`): asserts
/// `consecutive_continues` reads 1, 2, 0, 1 after each commit, exactly.
#[tokio::test]
async fn consecutive_continues_tracks_the_streak_of_committed_continue_actions() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), serde_json::json!({"type": "done"})),
        step_output(serde_json::json!({}), serde_json::json!({"type": "done"})),
        step_output(
            serde_json::json!({}),
            serde_json::json!({"type": "tool", "name": "edit_file", "args": {}}),
        ),
        step_output(serde_json::json!({}), serde_json::json!({"type": "done"})),
    ]);

    let mut tools = ToolSet::new();
    tools.register(Arc::new(NoopEditFile));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ToggleSkill);
    let mut active = session.activate(skill).await.unwrap();

    assert_eq!(
        harness_state(&mut active).await.consecutive_continues,
        0,
        "a fresh session must read 0"
    );

    let expected = [1u32, 2, 0, 1];
    for want in expected {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        let state = harness_state(&mut active).await;
        assert_eq!(
            state.consecutive_continues, want,
            "consecutive_continues after this commit"
        );
    }
}
