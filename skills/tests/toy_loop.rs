//! Integration tests for the session loop against `ToySkill` (milestone 1's
//! "toy skill runs 50 steps with flat prompt size" plus validation/
//! recovery, cancellation, activation typestate, and `/edit`).

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use statie_core::{
    Action, Event, LogKind, NullSink, Outcome, Park, SessionError, SessionLogExt, Skill, SkillSpec,
    Usage, Version,
};
use statie_models::{MockModel, MockTurn};
use statie_patch::JsonMergePatch;
use statie_skills::{ToySkill, ToyWorkspace};

use common::{config, config_with_retries, make_session};

#[tokio::test]
async fn flat_prompt_over_fifty_steps() {
    let root = tempfile::tempdir().unwrap();

    let mut turns: Vec<MockTurn> = (1..=50u64)
        .map(|i| {
            let content = json!({
                "patch": {"counter": i, "recent_summary": format!("did step {i}")},
                "action": {"type": "tool", "name": "echo", "args": {"text": format!("hello {i}")}},
            })
            .to_string();
            MockTurn::new(content).with_reasoning(format!("thinking about step {i}"))
        })
        .collect();
    turns.push(
        MockTurn::new(json!({"patch": {"counter": 51}, "action": {"type": "done"}}).to_string())
            .with_reasoning("thinking about step 51".to_string()),
    );

    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(51));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let mut ev = Event::Tick;
    let mut iterations = 0;
    loop {
        let rec = active.step(ev, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        iterations += 1;
        match outcome {
            Outcome::Continue => ev = Event::Tick,
            Outcome::Done => break,
            other => panic!("unexpected outcome: {other:?}"),
        }
        if iterations > 200 {
            panic!("loop did not terminate");
        }
    }

    assert_eq!(model.calls(), 51, "expected exactly 51 model calls");

    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(51));
    assert_eq!(state.agent.counter, 51);
    assert_eq!(state.harness.step, 51);

    let replayed = active.replay(Version(25)).await.unwrap();
    assert_eq!(replayed.agent.counter, 25);

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 51);

    let token_series: Vec<usize> = prompts[1..51].iter().map(|p| p.tokens).collect();
    eprintln!("token series (prompts 2..=51): {token_series:?}");
    let max = *token_series.iter().max().unwrap();
    let min = *token_series.iter().min().unwrap();
    assert!(
        max - min <= 8,
        "prompt size grew across steps: max={max} min={min} series={token_series:?}"
    );

    let last_prompt = &prompts[50];
    assert!(
        last_prompt.user.contains("echo: hello 50"),
        "last prompt should contain the latest observation:\n{}",
        last_prompt.user
    );
    assert!(
        !last_prompt.user.contains("echo: hello 49"),
        "last prompt should not contain the previous observation:\n{}",
        last_prompt.user
    );

    for p in &prompts {
        assert!(
            !p.system.contains("thinking about step"),
            "system prompt leaked reasoning: {}",
            p.system
        );
        assert!(
            !p.user.contains("thinking about step"),
            "user prompt leaked reasoning: {}",
            p.user
        );
    }

    assert_eq!(log.entries(Some(LogKind::Reasoning), 10_000).len(), 51);
    assert_eq!(log.entries(Some(LogKind::Patch), 10_000).len(), 51);
    assert_eq!(log.entries(Some(LogKind::Action), 10_000).len(), 51);
    assert_eq!(log.entries(Some(LogKind::ToolOutput), 10_000).len(), 50);
}

#[tokio::test]
async fn inbox_and_ack() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
        MockTurn::new(
            json!({
                "patch": {"notes": ["saw it"]},
                "action": {"type": "yield"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(100));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active
        .step(Event::User("please note this".to_string()), &mut NullSink)
        .await
        .unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].user.contains("[1] please note this"),
        "prompt should render the unacked inbox item:\n{}",
        prompts[0].user
    );

    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "expected a rejected system entry, got {sys_entries:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].user.contains("output rejected"),
        "second prompt should show the rejection observation:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[1].user.to_lowercase().contains("inbox"),
        "rejection reason should mention the inbox:\n{}",
        prompts[1].user
    );

    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    let item = state.harness.inbox.iter().find(|i| i.id == 1).unwrap();
    assert!(item.acked);
    assert_eq!(state.agent.notes, vec!["saw it".to_string()]);
}

#[tokio::test]
async fn retry_cap_parks_with_failure() {
    let root = tempfile::tempdir().unwrap();

    let model = Arc::new(
        MockModel::new(vec![
            MockTurn::new("not json".to_string()),
            MockTurn::new("not json".to_string()),
            MockTurn::new("not json".to_string()),
        ])
        .repeat_last(true),
    );
    let (mut session, log) = make_session(Arc::clone(&model), config_with_retries(&root, 2));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(100));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Parked(Park::Failure(_))),
        "got {outcome3:?}"
    );

    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts[2].user.contains("output rejected"),
        "third prompt should contain the rejection:\n{}",
        prompts[2].user
    );
}

#[tokio::test]
async fn cancel_during_model_call() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string())
            .with_delay(Duration::from_secs(10)),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(100));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let handle = active.session().cancel_handle();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        active.step(Event::Tick, &mut NullSink),
    )
    .await
    .expect("step should return within 2s")
    .unwrap();
    let outcome = log.append_record(result).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Cancelled)),
        "got {outcome:?}"
    );
    cancel_task.await.unwrap();

    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));

    // A fresh token is installed per step, so a subsequent step with a
    // valid (non-delayed) turn should succeed normally.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );
}

#[tokio::test]
async fn activation_typestate_and_transitions() {
    let root = tempfile::tempdir().unwrap();

    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string(),
    )]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    // Object safety: `Skill` trait object via `Arc<dyn Skill<...>>`.
    let skill: Arc<dyn Skill<Workspace = ToyWorkspace, Patch = JsonMergePatch<ToyWorkspace>>> =
        Arc::new(ToySkill::new(1));
    let mut active = session.activate(skill).await.unwrap();
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();
    drop(active);

    // OtherSkill has no allowed transitions from "toy", and "toy" doesn't
    // allow transitioning to "other" either, so a plain `activate` errors.
    let other: Arc<OtherSkill> = Arc::new(OtherSkill);
    match session.activate(Arc::clone(&other)).await {
        Err(err @ SessionError::TransitionNotAllowed { .. }) => {
            let _ = err;
        }
        Err(other) => panic!("expected TransitionNotAllowed, got {other:?}"),
        Ok(_) => panic!("expected activate to reject the disallowed transition"),
    }

    let mut forced = session.activate_forced(Arc::clone(&other)).await.unwrap();
    let (state, _) = forced.session().state().await.unwrap();
    assert_eq!(state.harness.activation, "other");
}

struct OtherSkill;

impl Skill for OtherSkill {
    type Workspace = ToyWorkspace;
    type Patch = JsonMergePatch<ToyWorkspace>;

    fn name(&self) -> &str {
        "other"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("other", "another skill"))
    }

    fn initial_workspace(&self) -> Self::Workspace {
        ToyWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }
}

#[tokio::test]
async fn user_patch_edit() {
    let root = tempfile::tempdir().unwrap();

    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string(),
    )]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(100));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Yield)),
        "got {outcome:?}"
    );

    let (_, version_before) = active.session().state().await.unwrap();

    let rec2 = active
        .user_patch(json!({"notes": ["manual"]}))
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let (state, version_after) = active.session().state().await.unwrap();
    assert_eq!(version_after, Version(version_before.0 + 1));
    assert_eq!(state.agent.notes, vec!["manual".to_string()]);

    let patch_entries = log.entries(Some(LogKind::Patch), 10_000);
    let last = patch_entries.last().unwrap();
    assert_eq!(
        last.payload.get("source").and_then(Value::as_str),
        Some("user")
    );

    // A user patch that violates `rules` (recent_summary > 500 chars).
    let long_summary = "x".repeat(600);
    let bad = active
        .user_patch(json!({"recent_summary": long_summary}))
        .await;
    assert!(bad.is_err(), "rule-violating user patch should error");

    let (_, version_final) = active.session().state().await.unwrap();
    assert_eq!(
        version_final, version_after,
        "version must be unchanged on rejected user patch"
    );
}

// Keep `Action` import exercised for readability of expected shapes above,
// even though tests construct actions via JSON.
#[allow(dead_code)]
fn _keep_action_import(a: Action) -> Action {
    a
}

/// Augmentation is derived; when augmenters return more than the section
/// budget allows, the loop trims it instead of failing the step.
#[tokio::test]
async fn oversized_augmentation_is_trimmed_not_fatal() {
    use statie_core::{Augmenter, Snippet, TokenBudget, Tokenizer};
    struct Huge;
    #[async_trait::async_trait]
    impl Augmenter<ToyWorkspace> for Huge {
        fn name(&self) -> &str {
            "huge"
        }
        async fn augment(
            &self,
            _: &ToyWorkspace,
            _: Option<&statie_core::Observation>,
            _: TokenBudget,
            _: &dyn Tokenizer,
        ) -> Vec<Snippet> {
            vec![
                Snippet {
                    source: "small".into(),
                    text: "tiny".into(),
                },
                Snippet {
                    source: "big".into(),
                    text: "x".repeat(4000),
                },
            ]
        }
    }
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        json!({"patch": {"counter": 1}, "action": {"type": "yield"}, "ack_inbox": [1]}).to_string(),
    )]));
    let mut cfg = common::config(&root);
    cfg.budgets.augmentation = 40;
    let (session, _log) = common::make_session(model.clone(), cfg);
    let mut session = session.with_augmenter(Box::new(Huge), TokenBudget(10_000));
    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(5));
    let mut active = session.activate(skill).await.unwrap();
    let rec = active
        .step(Event::User("go".into()), &mut NullSink)
        .await
        .expect("step must not fail");
    let outcome = active.session().log().append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Yield)),
        "{outcome:?}"
    );
    let user = model.prompts()[0].user.clone();
    assert!(user.contains("tiny"), "small snippet kept");
    assert!(
        !user.contains(&"x".repeat(4000)),
        "oversized snippet dropped"
    );
}

/// A `MockTurn` with usage attached surfaces on `Active::last_usage()` after
/// the step that calls the model; a step with no model call (terminal skill)
/// reports `None`.
#[tokio::test]
async fn last_usage_reflects_the_most_recent_model_call() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![
        MockTurn::new(
            json!({"patch": {"counter": 1}, "action": {"type": "yield"}, "ack_inbox": [1]})
                .to_string(),
        )
        .with_usage(200, 20),
    ]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(5));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    assert!(
        active.last_usage().is_none(),
        "no model call has happened yet"
    );

    let rec = active
        .step(Event::User("go".into()), &mut NullSink)
        .await
        .unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Yield)),
        "{outcome:?}"
    );

    assert_eq!(
        active.last_usage(),
        Some(Usage {
            prompt_tokens: 200,
            completion_tokens: 20,
        })
    );
}

/// A step that never calls the model (the skill is already terminal)
/// reports `last_usage() == None`, even after a prior step reported usage.
#[tokio::test]
async fn last_usage_is_none_after_a_step_with_no_model_call() {
    let root = tempfile::tempdir().unwrap();
    // target 0: `is_terminal` is true immediately, so `step` returns
    // `Outcome::Done` without ever calling the model.
    let model = Arc::new(MockModel::new(vec![]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(0));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Done), "{outcome:?}");
    assert!(active.last_usage().is_none());
}

/// A rejected output (parse failure) with usage attached still records
/// `last_usage`, and the `rejected` System log payload carries
/// `usage.prompt_tokens`.
#[tokio::test]
async fn rejected_output_with_usage_records_last_usage_and_log_payload() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![
        MockTurn::new("not json".to_string()).with_usage(50, 3),
    ]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(5));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let sys_entries: Vec<_> = rec
        .entries()
        .iter()
        .filter(|e| e.kind == LogKind::System)
        .cloned()
        .collect();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "{outcome:?}");

    assert_eq!(
        active.last_usage(),
        Some(Usage {
            prompt_tokens: 50,
            completion_tokens: 3,
        })
    );

    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    assert_eq!(
        rejected
            .payload
            .get("usage")
            .and_then(|u| u.get("prompt_tokens")),
        Some(&json!(50))
    );
}
