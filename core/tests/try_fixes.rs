//! Integration tests for the `try_fixes` tool (BACKLOG.md item 4,
//! "Hypotheses and verify"): `Session::with_fixer`/`with_candidate_runner`,
//! the session's interception of `Action::Tool { name: "try_fixes", .. }`
//! before dispatch, and the harness-owned `HarnessState::
//! last_try_fixes_step`/`try_fixes_available` fields.
//!
//! Fixture style copied from `core/tests/think.rs` (core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models` as a normal dependency;
//! this test builds its own minimal `StateStore`, `Model`, and `Skill`
//! stubs) -- but the `test` tool is the REAL `statie_tools::TestTool`
//! (available as a dev-dependency, see `core/Cargo.toml`), run against a
//! real temporary git sandbox (fixture style from `tools/tests/
//! worktree.rs`), so the prior `test` turn's failure and the winning
//! candidate's effect on `answer.txt` are genuine, not scripted. Candidate
//! verification itself goes through `statie_core::MockCandidateRunner`
//! (core cannot depend on `statie-tools`'s `WorktreeRunner` as a normal
//! dependency either -- see `core/src/fixer.rs`'s module doc for why the
//! verification primitive is a trait, `CandidateRunner`, with the real git-
//! worktree implementation living in `tools::worktree`).

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, CandidateEdit, CandidateRunResult, Commitment, CommittedState, Event,
    FixOutput, Fixer, HarnessState, LogKind, LoggedPatch, MockCandidateRunner, MockFixer,
    MockThinker, Model, NullSink, Outcome, PatchError, PatchStrategy, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore,
    StepSink, StoreError, ThinkOutput, ToolSet, Version,
};
use statie_tools::TestTool;

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/think.rs`).
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
// A scripted `Model` driving a fixed sequence of turns.
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<String>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn capabilities(&self) -> statie_core::Capabilities {
        statie_core::Capabilities {
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
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.prompts.lock().unwrap().push(prompt.user().to_string());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(statie_core::ModelError::Backend(
                "script exhausted".to_string(),
            ));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace and skill: `try_fixes` active alongside `test`.
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
                cur_map.insert(k.clone(), v.clone());
            }
        } else {
            return Err(PatchError::Malformed("patch must be an object".to_string()));
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

struct TryFixesSkill;

impl Skill for TryFixesSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "try_fixes_skill"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "try_fixes_skill",
            "a skill for try_fixes tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["test".to_string(), "try_fixes".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn session_config(root: &std::path::Path) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 40_000,
            observation: 8000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root).unwrap(),
    }
}

fn test_turn(cmd: &str) -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        "ack_inbox": []
    })
    .to_string()
}

fn try_fixes_turn() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "tool", "name": "try_fixes", "args": {}},
        "ack_inbox": []
    })
    .to_string()
}

fn yield_turn() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "yield"},
        "ack_inbox": []
    })
    .to_string()
}

fn test_tool_set() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(TestTool {
        default_timeout_secs: 30,
        max_output_bytes: 200_000,
    }));
    tools
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return;
        }
    };
}

async fn harness_state<K: Skill>(active: &mut statie_core::Active<'_, K>) -> HarnessState
where
    K::Workspace: Clone + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let (state, _version) = active.session().state().await.unwrap();
    state.harness
}

fn worktree_dirs_remaining(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let worktrees_dir = root.join(".statie").join("worktrees");
    if !worktrees_dir.exists() {
        return Vec::new();
    }
    std::fs::read_dir(&worktrees_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default()
}

// =======================================================================
// (a) a winning candidate: candidate 2 passes, is applied to the real
// sandbox; candidate 1 is a loser with a reason; candidate 3 is "not
// tried"; no worktree directory remains; the `try_fixes` log event names
// winner index 2; HEAD/index are unchanged.
// =======================================================================

#[tokio::test]
async fn try_fixes_applies_the_first_passing_candidate() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    std::fs::write(root.join("answer.txt"), "WRONG\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
    let head_before = String::from_utf8(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let index_before = std::fs::read(root.join(".git").join("index")).ok();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        test_turn("sh -c 'grep -q RIGHT answer.txt'"),
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "WRONG\n".to_string(),
                new: "STILL_WRONG\n".to_string(),
                rationale: "guess 1".to_string(),
            },
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "WRONG\n".to_string(),
                new: "RIGHT\n".to_string(),
                rationale: "guess 2".to_string(),
            },
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "NOPE".to_string(),
                new: "x".to_string(),
                rationale: "guess 3".to_string(),
            },
        ],
        usage: None,
    })]));

    let mut files = HashMap::new();
    files.insert("answer.txt".to_string(), "WRONG\n".to_string());
    let runner = Arc::new(MockCandidateRunner::new(
        files,
        vec![
            Ok(CandidateRunResult {
                exit: Some(1),
                stdout: String::new(),
                stderr: "assertion failed: grep did not match".to_string(),
                timed_out: false,
            }),
            Ok(CandidateRunResult {
                exit: Some(0),
                stdout: "ok".to_string(),
                stderr: String::new(),
                timed_out: false,
            }),
        ],
    ));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        test_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3)
    .with_candidate_runner(Arc::clone(&runner) as Arc<dyn statie_core::CandidateRunner>);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: a real failing `test` call, so the failure is on record.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let h1 = harness_state(&mut active).await;
    assert_eq!(
        h1.last_test_cmd.as_deref(),
        Some("sh -c 'grep -q RIGHT answer.txt'")
    );

    // Step 2: `try_fixes`.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step2 = rec2.entries().iter().map(|e| e.step).max().unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    // The sandbox's `answer.txt` is now RIGHT.
    let contents = std::fs::read_to_string(root.join("answer.txt")).unwrap();
    assert_eq!(contents, "RIGHT\n");

    // The observation names candidate 2 as the winner, candidate 1 as a
    // loser with a reason, candidate 3 as not tried.
    let entries = log.entries(Some(LogKind::ToolOutput), 100);
    let output_entry = entries
        .iter()
        .find(|e| {
            e.step == step2 && e.payload.get("name").and_then(Value::as_str) == Some("try_fixes")
        })
        .expect("expected a try_fixes ToolOutput entry");
    let output_text = output_entry
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap();
    assert!(
        output_text.contains("candidate 2 passed"),
        "expected candidate 2 to be named the winner:\n{output_text}"
    );
    assert!(
        output_text.contains("1. answer.txt — guess 1 -> "),
        "expected candidate 1 to be a loser with a reason:\n{output_text}"
    );
    assert!(
        output_text.contains("3. answer.txt — guess 3 -> not tried"),
        "expected candidate 3 to be not tried:\n{output_text}"
    );

    // No worktree directory remains.
    assert!(
        worktree_dirs_remaining(root).is_empty(),
        "expected no worktree directories left under .statie/worktrees"
    );
    assert_eq!(runner.removed_count(), 2, "both candidates must be removed");

    // The `try_fixes` log event exists with winner index 2.
    let system_entries = log.entries(Some(LogKind::System), 100);
    let try_fixes_event = system_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("try_fixes"))
        .expect("expected a try_fixes system entry");
    assert_eq!(
        try_fixes_event
            .payload
            .get("winner")
            .and_then(Value::as_u64),
        Some(2)
    );

    // The sandbox's HEAD and index are unchanged.
    let head_after = String::from_utf8(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(head_before, head_after, "HEAD must be unchanged");
    let index_after = std::fs::read(root.join(".git").join("index")).ok();
    assert_eq!(index_before, index_after, "the index must be unchanged");

    // `last_try_fixes_step`/`last_think_step` set to this step.
    let h2 = harness_state(&mut active).await;
    assert_eq!(h2.last_try_fixes_step, Some(step2));
    assert_eq!(h2.last_think_step, Some(step2));
}

// =======================================================================
// (b) no candidate passes, a thinker is attached: it receives one input
// whose question contains both loser lines; the observation carries
// "think:"; the sandbox file is unchanged.
// =======================================================================

#[tokio::test]
async fn try_fixes_falls_back_to_think_when_none_pass() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    std::fs::write(root.join("answer.txt"), "WRONG\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        test_turn("sh -c 'grep -q RIGHT answer.txt'"),
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "WRONG\n".to_string(),
                new: "STILL_WRONG\n".to_string(),
                rationale: "guess 1".to_string(),
            },
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "WRONG\n".to_string(),
                new: "ALSO_WRONG\n".to_string(),
                rationale: "guess 2".to_string(),
            },
        ],
        usage: None,
    })]));

    let mut files = HashMap::new();
    files.insert("answer.txt".to_string(), "WRONG\n".to_string());
    let runner = Arc::new(MockCandidateRunner::new(
        files,
        vec![
            Ok(CandidateRunResult {
                exit: Some(1),
                stdout: String::new(),
                stderr: "error: first guess wrong".to_string(),
                timed_out: false,
            }),
            Ok(CandidateRunResult {
                exit: Some(1),
                stdout: String::new(),
                stderr: "error: second guess wrong".to_string(),
                timed_out: false,
            }),
        ],
    ));

    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer(
        "the real bug is in a different file",
    ))]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        test_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3)
    .with_candidate_runner(Arc::clone(&runner) as Arc<dyn statie_core::CandidateRunner>)
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step2 = rec2.entries().iter().map(|e| e.step).max().unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    // The thinker received exactly one call, whose question contains both
    // loser lines.
    let calls = thinker.calls();
    assert_eq!(calls.len(), 1, "expected exactly one thinker call");
    assert!(
        calls[0].question.contains("error: first guess wrong")
            && calls[0].question.contains("error: second guess wrong"),
        "expected both loser lines in the think question:\n{}",
        calls[0].question
    );

    // The observation carries "think:".
    let entries = log.entries(Some(LogKind::ToolOutput), 100);
    let output_entry = entries
        .iter()
        .find(|e| {
            e.step == step2 && e.payload.get("name").and_then(Value::as_str) == Some("try_fixes")
        })
        .expect("expected a try_fixes ToolOutput entry");
    let output_text = output_entry
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap();
    assert!(
        output_text.contains("think:\nthe real bug is in a different file"),
        "expected the think fallback's answer in the observation:\n{output_text}"
    );
    assert!(output_text.contains("none passed"));

    // The sandbox file is unchanged.
    let contents = std::fs::read_to_string(root.join("answer.txt")).unwrap();
    assert_eq!(contents, "WRONG\n");
}

// =======================================================================
// (c) no failing test on record: the error observation, fixer never
// called.
// =======================================================================

#[tokio::test]
async fn try_fixes_with_no_failing_test_reports_error_without_calling_fixer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![try_fixes_turn(), yield_turn()]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![],
        usage: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    // No ToolOutput entry for this shape (mirrors `think`'s own invalid-
    // args/empty-question path): a `tool_error` System entry, and the
    // observation text reaches the model via the NEXT prompt.
    let system_entries = log.entries(Some(LogKind::System), 100);
    let tool_error = system_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .expect("expected a tool_error system entry");
    assert_eq!(
        tool_error.payload.get("name").and_then(Value::as_str),
        Some("try_fixes")
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    let second_prompt = &prompts[1];
    assert!(
        second_prompt.contains("try_fixes: no failing test to fix; run test first"),
        "expected the no-failing-test message in the next prompt:\n{second_prompt}"
    );

    assert_eq!(
        fixer.inputs().len(),
        0,
        "the fixer must never be called when there is no failing test on record"
    );
}

// =======================================================================
// (d) no fixer attached: same shape as `think` without a thinker.
// =======================================================================

#[tokio::test]
async fn try_fixes_with_no_fixer_reports_not_configured() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![try_fixes_turn(), yield_turn()]);

    // No `.with_fixer(...)` call.
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    );

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step_number = rec.entries().iter().map(|e| e.step).max().unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let entries = log.entries(Some(LogKind::System), 100);
    let try_fixes_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("try_fixes"))
        .expect("expected a try_fixes system entry");
    assert_eq!(
        try_fixes_event
            .payload
            .get("configured")
            .and_then(Value::as_bool),
        Some(false)
    );

    let h = harness_state(&mut active).await;
    assert_eq!(h.last_try_fixes_step, Some(step_number));
    assert!(!h.try_fixes_available);

    let tool_output_entries = log.entries(Some(LogKind::ToolOutput), 100);
    let output_entry = tool_output_entries
        .iter()
        .find(|e| e.payload.get("name").and_then(Value::as_str) == Some("try_fixes"))
        .expect("expected a try_fixes ToolOutput entry");
    let output_text = output_entry
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap();
    assert!(
        output_text.contains("`try_fixes` is not configured"),
        "got: {output_text}"
    );
}

// =======================================================================
// (e) an anchor-ambiguous candidate is a loser with "ambiguous" in its
// reason, and the next candidate still runs.
// =======================================================================

#[tokio::test]
async fn try_fixes_ambiguous_anchor_is_a_loser_and_the_next_candidate_still_runs() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    // "A" occurs twice (a deliberately ambiguous anchor for the first
    // candidate below); "WRONG\n" occurs exactly once, so the second
    // candidate's anchor is unambiguous.
    std::fs::write(root.join("answer.txt"), "AA\nWRONG\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        test_turn("sh -c 'grep -q RIGHT answer.txt'"),
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![
            CandidateEdit {
                path: "answer.txt".to_string(),
                // "A" occurs twice in "AA\nWRONG\n" -- ambiguous.
                old: "A".to_string(),
                new: "X".to_string(),
                rationale: "ambiguous guess".to_string(),
            },
            CandidateEdit {
                path: "answer.txt".to_string(),
                old: "WRONG\n".to_string(),
                new: "RIGHT\n".to_string(),
                rationale: "good guess".to_string(),
            },
        ],
        usage: None,
    })]));

    let mut files = HashMap::new();
    files.insert("answer.txt".to_string(), "AA\nWRONG\n".to_string());
    let runner = Arc::new(MockCandidateRunner::new(
        files,
        vec![Ok(CandidateRunResult {
            exit: Some(0),
            stdout: "ok".to_string(),
            stderr: String::new(),
            timed_out: false,
        })],
    ));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        test_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3)
    .with_candidate_runner(Arc::clone(&runner) as Arc<dyn statie_core::CandidateRunner>);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step2 = rec2.entries().iter().map(|e| e.step).max().unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let entries = log.entries(Some(LogKind::ToolOutput), 100);
    let output_entry = entries
        .iter()
        .find(|e| {
            e.step == step2 && e.payload.get("name").and_then(Value::as_str) == Some("try_fixes")
        })
        .expect("expected a try_fixes ToolOutput entry");
    let output_text = output_entry
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap();
    assert!(
        output_text.contains("ambiguous"),
        "expected the ambiguous-anchor loser's reason to say so:\n{output_text}"
    );
    assert!(
        output_text.contains("candidate 2 passed"),
        "the second candidate must still be tried and win:\n{output_text}"
    );

    let contents = std::fs::read_to_string(root.join("answer.txt")).unwrap();
    assert_eq!(contents, "AA\nRIGHT\n");
}
