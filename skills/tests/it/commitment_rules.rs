//! Tests for the commitment-slot validation added to
//! `skills/src/coding/mod.rs` (`strip_malformed_next`, shared by
//! `plan`/`implement`/`review` via their `validate()` overrides): a `next`
//! (the mutating call the model has committed to making next) must actually
//! name a mutating tool, must say what it will do, and must name a path
//! when the tool touches one file. A `next` that fails any of these is never
//! rejected -- the step commits, the malformed `next` is silently dropped
//! from the raw patch (so the committed `next` is left exactly as it was),
//! and a one-line note explaining why is delivered to the model.
//!
//! `next.tool` naming a non-mutating tool (e.g. `search`) was the first case
//! handled this way, closing the deadlock where a model committed to
//! `next.tool = "read_file"`: nothing rejected that, so the harness then
//! capped reads on the committed path while its own message demanded the
//! (impossible, read-only) committed call. Empty `next.what` and a missing
//! `next.path` used to reject the whole step instead (a rejection loop, not
//! a dropped slot -- a live run parked as a failure when a local model sent
//! `next = {"tool":"test","path":"..."}`, no `what`, four times running,
//! rejected each time until the retry cap); they now use the same
//! strip-and-note mechanism as the non-mutating case.
//!
//! Mirrors the commitment-slot tests in `recovery.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools (the same
//! shape as `common::make_file_tools_session`), a scripted `MockModel`, and
//! rejection reasons read from the `System` log's `rejected` entries.
//!
//! Deliberately not `mod common;` here: `skills/tests/common/mod.rs` also
//! exports `EchoTool`/`tool_set`/`make_session`, which build a
//! `Session<ToyWorkspace>` this file has no use for (only
//! `config`/`make_file_tools_session` apply to `CodingWorkspace`), and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused. Rather than editing `common/mod.rs` (out of scope for this
//! change) or faking a use of the `ToyWorkspace` helpers, this file inlines
//! the small `config`/`make_file_tools_session` pieces it actually needs, in
//! the same shape as `common/mod.rs`'s versions (see `child.rs` for the same
//! pattern, for the same reason).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, PlanSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `common::config`: a `SessionConfig` sandboxed to `root`
/// with the default retry cap (3).
fn config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

/// Same shape as `common::ArcModel`: wraps an `Arc<MockModel>` as a `Model`
/// so tests can keep an `Arc` handle to introspect `prompts()`/`calls()`
/// after the model has been moved (boxed) into a `Session`.
struct ArcModel(Arc<MockModel>);

#[async_trait]
impl statie_core::Model for ArcModel {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.0.step(prompt, schema, sink, cancel).await
    }
}

/// Same shape as `common::make_file_tools_session`: a `Session<CodingWorkspace>`
/// wired with the real `statie-tools` file tools (`read_file`, `edit_file`,
/// `write_file`, `shell`, `test`, ...), sandboxed via `cfg.root`, so these
/// tests exercise the actual `next.tool` value against real tool names.
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tools = default_tools(&ToolConfig::default());
    let session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// The rejection reason of the most recent `rejected` System log entry, or
/// `""` if there is none.
fn last_rejection_reason(log: &Arc<dyn SessionLog>) -> String {
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    sys_entries
        .iter()
        .rev()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .and_then(|e| e.payload.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// A single-step turn: patches `next` to the given raw JSON value and takes
/// action `ask_user` (never rejected by the yield/tool gates, so the only
/// thing that can reject the step is the rule under test).
fn set_next_turn(next: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"next": next},
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )
}

/// Same as `set_next_turn`, but also performs a `read_file` on `path` in the
/// same step -- used for the accepted-commitment case, which mirrors how a
/// model realistically commits right as it starts looking at the file (see
/// `recovery.rs`'s `set_commitment_and_read_turn`).
fn set_next_and_read_turn(next: Value, path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"next": next},
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": 1, "limit": 5},
            },
        })
        .to_string(),
    )
}

/// A single-step turn: patches `plan` to the given raw JSON array and takes
/// a `list_files` call (never rejected by the yield/tool gates, so the only
/// thing that can reject the step is the rule under test). Used by the
/// `plan_rules` tests (`skills/src/coding/mod.rs`) to first commit a plan,
/// then attempt a regression against it in a following turn.
fn set_plan_turn(plan: Value) -> MockTurn {
    // A `list_files` call, not `ask_user`: since item 30, an `ask_user`
    // sent alongside a non-empty plan is the hand-off to `implement`, and
    // these tests need to stay in `plan` for their second turn.
    MockTurn::new(
        json!({
            "patch": {"plan": plan},
            "action": {"type": "tool", "name": "list_files", "args": {"path": "."}},
        })
        .to_string(),
    )
}

/// Number of `rejected` System log entries recorded so far.
fn rejection_count(log: &Arc<dyn SessionLog>) -> usize {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .count()
}

/// Runs `turns` in order against a fresh `ImplementSkill` session (real file
/// tools, sandboxed to a fresh tempdir), one `step()` call per turn. Used by
/// the `plan_rules` tests, which need a first turn to commit a plan and a
/// second turn to attempt a regression against it. Returns whether the
/// *last* turn was rejected, plus the most recent rejection reason (which is
/// only meaningful when that flag is true).
async fn run_turns_on_implement(turns: Vec<MockTurn>) -> (bool, String) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(turns.clone()));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let mut rejected = false;
    for _ in &turns {
        let before = rejection_count(&log);
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "got {outcome:?}"
        );
        let after = rejection_count(&log);
        rejected = after > before;
    }

    (rejected, last_rejection_reason(&log))
}

/// Same as `run_turns_on_implement`, but against a fresh `PlanSkill` session
/// -- used by the test that checks `plan_rules` also holds in `plan`, not
/// just `implement`/`review`.
async fn run_turns_on_plan(turns: Vec<MockTurn>) -> (bool, String) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(turns.clone()));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let mut rejected = false;
    for _ in &turns {
        let before = rejection_count(&log);
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "got {outcome:?}"
        );
        let after = rejection_count(&log);
        rejected = after > before;
    }

    (rejected, last_rejection_reason(&log))
}

// `next.tool` naming a non-mutating tool is no longer a rejection
// (statie-spec.md "10. After it works: the rule audit", run 21's "nine
// rejections for `next.tool = search`"): the step commits, the offending
// `next` is silently dropped (the committed `next` is left exactly as it
// was), and a one-line note explaining why is delivered to the model: as a
// suffix on this step's own observation when the action is a tool call (see
// `next_tool_search_is_not_rejected_and_next_is_left_null`), or as a
// `System` log entry (`event: "note"`) when it is not (`ask_user`/`yield`
// never produce an observation of their own -- see
// `next_tool_search_with_ask_user_and_no_open_commitment_logs_a_note_entry`).

/// A single-step turn: patches `next` to the given raw JSON value and runs
/// `search` for `pattern` (optionally scoped to `path`) -- used for the
/// dropped-`next` cases, which need a real tool call (so `execute` produces
/// a `pending_observation` the note can be appended to) rather than
/// `set_next_turn`'s `ask_user`.
fn set_next_and_search_turn(next: Value, pattern: &str, path: Option<&str>) -> MockTurn {
    let mut args = json!({"pattern": pattern});
    if let (Some(path), Some(obj)) = (path, args.as_object_mut()) {
        obj.insert("path".to_string(), json!(path));
    }
    MockTurn::new(
        json!({
            "patch": {"next": next},
            "action": {
                "type": "tool",
                "name": "search",
                "args": args,
            },
        })
        .to_string(),
    )
}

#[tokio::test]
async fn next_tool_search_is_not_rejected_and_next_is_left_null() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        set_next_and_search_turn(
            json!({
                "tool": "search",
                "what": "find where the trait is defined",
            }),
            "fn old",
            None,
        ),
        // A no-op second turn, just so the first step's pending observation
        // (carrying the dropped-`next` note) gets rendered into a prompt
        // the test can inspect.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "a non-mutating `next` must not reject the step");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "the committed `next` must be left as it was (null) when the patch's only `next` \
         names a non-mutating tool"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    let prompt_after_first_step = prompts
        .get(1)
        .expect("expected a second prompt (built from the first step's observation)");
    assert!(
        prompt_after_first_step
            .user
            .contains("`next` = search ignored")
            && prompt_after_first_step
                .user
                .contains("a commitment is a change, not a look"),
        "expected the dropped-`next` note in the next prompt, got: {}",
        prompt_after_first_step.user
    );
}

/// When the step's action is not a tool call (e.g. `ask_user`), `execute`
/// never produces a `pending_observation` to append a validation note to
/// (there is no tool output to attach it in front of), so the note is
/// logged as a `System` entry (`event: "note"`) instead, so it is not lost.
#[tokio::test]
async fn next_tool_search_with_ask_user_and_no_open_commitment_logs_a_note_entry() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![set_next_turn(json!({
        "tool": "search",
        "what": "find where the trait is defined",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    // `set_next_turn` uses action `ask_user`, which parks the activation (it
    // is not rejected): a rejected step would instead have returned
    // `Outcome::Continue` with an unchanged committed state, so
    // `Parked(AskUser(_))` here is itself evidence the step committed.
    assert!(
        matches!(outcome, Outcome::Parked(Park::AskUser(_))),
        "got {outcome:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "a non-mutating `next` must not reject the step");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "the committed `next` must be left as it was (null) when the patch's only `next` \
         names a non-mutating tool"
    );

    let note_entry = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .expect("expected a `note` System entry when there is no pending observation");
    let text = note_entry
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("`next` = search ignored")
            && text.contains("a commitment is a change, not a look"),
        "unexpected note text: {text}"
    );
}

#[tokio::test]
async fn next_tool_search_with_an_open_commitment_leaves_the_open_commitment_and_notes_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        set_next_and_read_turn(
            json!({
                "tool": "edit_file",
                "path": "src/a.rs",
                "what": "replace fn old with fn new",
            }),
            "src/a.rs",
        ),
        set_next_and_search_turn(
            json!({
                "tool": "search",
                "what": "find another usage first",
            }),
            "fn old",
            Some("src/a.rs"),
        ),
        // A no-op third turn, just so the second step's pending observation
        // (carrying the dropped-`next` note) gets rendered into a prompt
        // the test can inspect.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next.as_ref().map(|c| c.tool.as_str()),
        Some("edit_file"),
        "the first step's commitment should have been accepted"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !rejected,
        "a non-mutating `next` must not reject the step even with an open commitment"
    );

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("the open edit_file commitment must survive the dropped search `next`");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("src/a.rs"));

    // One more step so the model sees the prompt built from the second
    // step's pending observation, which should carry the note naming the
    // still-open commitment.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    let prompt_after_second_step = prompts
        .get(2)
        .expect("expected a third prompt (built from the second step's observation)");
    assert!(
        prompt_after_second_step
            .user
            .contains("`next` = search ignored")
            && prompt_after_second_step
                .user
                .contains("your open commitment is still `edit_file src/a.rs`"),
        "expected the dropped-`next` note naming the open commitment in the next prompt, got: {}",
        prompt_after_second_step.user
    );
}

// --- Empty `next.what` / missing `next.path` are no longer rejections
// either (statie-spec.md "10. After it works: the rule audit"): a live run
// parked as a failure when a local model sent `next =
// {"tool":"test","path":"..."}` (no `what`) four times running, rejected
// each time until the retry cap -- a rejection loop over a missing one-liner
// is a worse outcome than a dropped slot. Both cases now use the same
// strip-and-note mechanism as `next.tool` naming a non-mutating tool: the
// step commits, the malformed `next` is dropped from the raw patch (the
// committed `next` is left exactly as it was), and a one-line note is
// delivered the same way (a suffix on the next observation when the action
// is a tool call, or a `System` `note` entry otherwise).

#[tokio::test]
async fn next_edit_file_with_empty_what_is_not_rejected_and_next_is_left_null() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        set_next_turn(json!({
            "tool": "edit_file",
            "path": "a.rs",
            "what": "   ",
        })),
        // A no-op second turn, just so the first step's `System` `note`
        // entry has already been logged by the time we inspect it (the
        // action above is `ask_user`, which produces no pending observation
        // to append a note to -- see the `next_tool_search_with_ask_user...`
        // test above for the same shape).
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::AskUser(_))),
        "got {outcome:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "empty `next.what` must not reject the step");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "the committed `next` must be left as it was (null) when `next.what` is empty"
    );

    let note_entry = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .expect("expected a `note` System entry when there is no pending observation");
    let text = note_entry
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("`next` ignored")
            && text.contains("`next.what` must say in one line what the call will do")
            && text.contains("Reads need no `next`; set it when you know the change you will make"),
        "unexpected note text: {text}"
    );
}

#[tokio::test]
async fn next_edit_file_with_empty_what_over_an_open_commitment_leaves_it_unchanged() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        set_next_and_read_turn(
            json!({
                "tool": "edit_file",
                "path": "src/a.rs",
                "what": "replace fn old with fn new",
            }),
            "src/a.rs",
        ),
        set_next_and_read_turn(
            json!({
                "tool": "edit_file",
                "path": "src/a.rs",
                "what": "   ",
            }),
            "src/a.rs",
        ),
        // A no-op third turn, just so the second step's pending observation
        // (carrying the dropped-`next` note) gets rendered into a prompt
        // the test can inspect.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next.as_ref().map(|c| c.tool.as_str()),
        Some("edit_file"),
        "the first step's commitment should have been accepted"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec2).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !rejected,
        "empty `next.what` must not reject the step even with an open commitment"
    );

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("the open edit_file commitment must survive the empty-`what` `next`");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("src/a.rs"));
    assert_eq!(next.what, "replace fn old with fn new");

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    let prompt_after_second_step = prompts
        .get(2)
        .expect("expected a third prompt (built from the second step's observation)");
    assert!(
        prompt_after_second_step.user.contains("`next` ignored")
            && prompt_after_second_step
                .user
                .contains("`next.what` must say in one line what the call will do")
            && prompt_after_second_step
                .user
                .contains("Your open commitment is unchanged"),
        "expected the dropped-`next` note in the next prompt, got: {}",
        prompt_after_second_step.user
    );
}

#[tokio::test]
async fn next_edit_file_with_no_path_is_not_rejected_and_next_is_left_null() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![set_next_turn(json!({
        "tool": "edit_file",
        "what": "replace fn old with fn new",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    // `set_next_turn` uses action `ask_user`, which parks the activation (it
    // is not rejected): a rejected step would instead have returned
    // `Outcome::Continue` with an unchanged committed state, so
    // `Parked(AskUser(_))` here is itself evidence the step committed.
    assert!(
        matches!(outcome, Outcome::Parked(Park::AskUser(_))),
        "got {outcome:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "missing `next.path` must not reject the step");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "the committed `next` must be left as it was (null) when `next.path` is missing"
    );

    let note_entry = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .expect("expected a `note` System entry when there is no pending observation");
    let text = note_entry
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("`next` ignored")
            && text.contains("`next.path` is required when `next.tool` is `edit_file`")
            && text.contains("Reads need no `next`; set it when you know the change you will make"),
        "unexpected note text: {text}"
    );
}

#[tokio::test]
async fn next_edit_file_with_path_and_what_and_a_matching_read_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![set_next_and_read_turn(
        json!({
            "tool": "edit_file",
            "path": "src/a.rs",
            "what": "add a line",
        }),
        "src/a.rs",
    )];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "step should not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("expected `next` to be set in the committed workspace");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("src/a.rs"));
    assert_eq!(next.what, "add a line");
}

#[tokio::test]
async fn next_shell_with_no_path_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![set_next_turn(json!({
        "tool": "shell",
        "what": "run cargo fmt",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    // `ask_user` parks the activation (it is not rejected); a rejected step
    // would instead have returned `Outcome::Continue` with an unchanged
    // committed state, so `Parked(AskUser(_))` here is itself evidence the
    // commitment passed validation and was committed.
    assert!(
        matches!(outcome, Outcome::Parked(Park::AskUser(_))),
        "got {outcome:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected, "step should not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    let next = state
        .agent
        .next
        .expect("expected `next` to be set in the committed workspace");
    assert_eq!(next.tool, "shell");
    assert_eq!(next.path, None);
    assert_eq!(next.what, "run cargo fmt");
}

// --- `plan_rules` (skills/src/coding/mod.rs): a step committed `done` must
// stay `done` (or move to `skipped`) -- never regress back to
// `pending`/`in_progress`. Observed three times in live runs with a local
// model: after a resume, the model moves a finished step back to
// `pending`/`in_progress`, then redoes finished work or loops on step 1.

#[tokio::test]
async fn done_step_moved_to_in_progress_is_rejected_and_points_at_next_open_step() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "in_progress", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];

    let (rejected, reason) = run_turns_on_implement(turns).await;
    assert!(rejected, "second turn should have been rejected");
    assert!(
        reason.contains("plan step 1 is done and stays done (first)"),
        "rejection reason should name the offending step: {reason}"
    );
    assert!(
        reason.contains("step 2 (second)"),
        "rejection reason should redirect to the next open step: {reason}"
    );
}

#[tokio::test]
async fn done_step_moved_to_pending_with_no_open_step_left_says_all_steps_are_done() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "only step"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "pending", "text": "only step"},
        ])),
    ];

    let (rejected, reason) = run_turns_on_implement(turns).await;
    assert!(rejected, "second turn should have been rejected");
    assert!(
        reason.contains("plan step 1 is done and stays done (only step)"),
        "rejection reason should name the offending step: {reason}"
    );
    assert!(
        reason.contains("All steps are done"),
        "rejection reason should say all steps are done when there is no open step: {reason}"
    );
}

#[tokio::test]
async fn done_step_moved_to_skipped_is_accepted() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "skipped", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];

    let (rejected, reason) = run_turns_on_implement(turns).await;
    assert!(
        !rejected,
        "done -> skipped should not be rejected: {reason}"
    );
}

#[tokio::test]
async fn pending_to_in_progress_to_done_across_turns_is_accepted() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "pending", "text": "first"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "in_progress", "text": "first"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
        ])),
    ];

    let (rejected, reason) = run_turns_on_implement(turns).await;
    assert!(
        !rejected,
        "pending -> in_progress -> done should not be rejected: {reason}"
    );
}

#[tokio::test]
async fn plan_skill_also_rejects_a_done_step_moved_back_to_pending() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "pending", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];

    let (rejected, reason) = run_turns_on_plan(turns).await;
    assert!(
        rejected,
        "second turn should have been rejected in plan too"
    );
    assert!(
        reason.contains("plan step 1 is done and stays done (first)"),
        "rejection reason should name the offending step: {reason}"
    );
    assert!(
        reason.contains("step 2 (second)"),
        "rejection reason should redirect to the next open step: {reason}"
    );
}

// --- `notes` is harness-owned on `CodingWorkspace` (Unit 3): a model patch
// touching it is rejected by `validate_default`'s owned-field check (same
// mechanism as `reports`, now that the old `reports`-shape heuristic in
// `common_rules` has been replaced by that check); an operator patch
// (`Active::apply_user_patch`) is a trusted write, and the next worker
// prompt renders it under the "notes (kept by the harness from your
// previous reasoning)" header (`core/src/prompt.rs`'s `Section::Notes`,
// wired to `Skill::notes` in `session.rs`).

/// A single-step turn: patches `notes` to the given string and takes action
/// `ask_user` (never rejected by the yield/tool gates, so the only thing
/// that could reject the step is the owned-field check under test -- which
/// no longer does; see `model_patch_setting_notes_is_stripped_and_noted`
/// below).
fn set_notes_turn(notes: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"notes": notes},
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )
}

/// A model patch touching `notes` (harness-owned) used to have the whole
/// step rejected by `validate_default`'s owned-field check. It now goes
/// through `strip_harness_owned` (`skills/src/coding/mod.rs`, called at the
/// top of `plan`/`implement`/`review`'s `validate` overrides, in the same
/// place `strip_malformed_next` runs) before `validate_default` ever sees
/// it: the `notes` key is dropped from the raw patch, the step commits
/// (here, the `ask_user` action still parks the session), and a note
/// explaining the drop is recorded -- logged as a `System` `event: "note"`
/// entry since `ask_user` leaves no pending observation to append to (see
/// `core/src/session.rs`'s "10b. Validation notes").
#[tokio::test]
async fn model_patch_setting_notes_is_stripped_and_noted() {
    let root = tempfile::tempdir().unwrap();
    let turns = vec![set_notes_turn("the model wrote this")];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(statie_core::Park::AskUser(_))),
        "the ask_user action should still park the session; got {outcome:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejection = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        rejection.is_none(),
        "the notes-patch step must not be rejected: {sys_entries:?}"
    );

    let note_entry = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"));
    let note_text = note_entry
        .and_then(|e| e.payload.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        note_text,
        "`notes` is kept by the harness; that part of your patch was dropped and the rest was kept.",
        "expected the strip-and-note text, got: {note_text}"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.notes, "",
        "the dropped patch must not have overwritten the harness-owned `notes`"
    );
}

#[tokio::test]
async fn operator_patch_setting_notes_is_accepted_and_next_prompt_carries_the_notes_header() {
    let root = tempfile::tempdir().unwrap();
    // One turn queued for the `step()` call made *after* the operator
    // patch, to capture the prompt it renders.
    let turns = vec![MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    active
        .apply_user_patch(json!({"notes": "kept from last reasoning"}))
        .await
        .expect("an operator patch touching a harness-owned field should be accepted");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.notes, "kept from last reasoning");

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1);
    let user = &prompts[0].user;
    assert!(
        user.contains("notes (kept by the harness from your previous reasoning)"),
        "prompt should carry the notes section header:\n{user}"
    );
    assert!(
        user.contains("kept from last reasoning"),
        "prompt should carry the notes text:\n{user}"
    );
}

#[tokio::test]
async fn empty_notes_omits_the_notes_header_from_the_prompt() {
    let root = tempfile::tempdir().unwrap();
    let turns = vec![MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.notes, "", "notes should start empty");

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1);
    let user = &prompts[0].user;
    assert!(
        !user.contains("notes (kept by the harness from your previous reasoning)"),
        "prompt should not carry the notes section header when notes is empty:\n{user}"
    );
}
