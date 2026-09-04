//! `ConfiguredChildRunner`: the `statie-skills` implementation of core's
//! [`statie_core::ChildRunner`] trait (milestone 9, ARCHITECTURE.md "The
//! `delegate` action"). Built from a map of [`ChildSkill`]s, it runs one
//! named child to completion over an `InMemoryStore<ReportWorkspace>` (a
//! deliberate deviation from the design's "second sqlite region" -- see the
//! module-level note below) and returns the resulting `Report`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use statie_core::{
    ChildRequest, ChildRunner, Event, Model, Outcome, Park, Report, ReportOutcome, ReportStats,
    ReportWorkspace, SandboxRoot, Session, SessionConfig, SessionError, SessionLog, Skill,
    StepSink, TaggedLog, Tokenizer, ToolSet,
};
use statie_store::InMemoryStore;

use crate::child::ChildSkill;

/// Errors constructing a [`ConfiguredChildRunner`].
#[derive(Debug, thiserror::Error)]
pub enum ChildRunnerError {
    /// A child's config names a tool the parent's own `ToolSet` doesn't
    /// have registered. Caught at construction time, not at delegation
    /// time, per ARCHITECTURE.md: "the child gets the subset named in its
    /// config; unknown names are an error at construction of the runner,
    /// not at run time".
    #[error("child `{child}` names unknown tool `{tool}`")]
    UnknownTool { child: String, tool: String },
}

/// Builds a `Box<dyn Tokenizer>` fresh each time it's called. `Session`
/// owns its tokenizer by value (`Box<dyn Tokenizer>`, not `Arc`), so a
/// child session needs its own instance rather than a shared one.
pub type TokenizerFactory = Box<dyn Fn() -> Box<dyn Tokenizer> + Send + Sync>;

/// Runs configured children (milestone 9's `delegate` action) to
/// completion. Built once per parent session from its `subagents` config
/// stanza, the shared model, the shared log, the parent's tool set
/// (restricted per child), a tokenizer factory, the parent's session
/// config (budgets/retries inherited unless the child overrides budgets),
/// and the shared sandbox root.
///
/// **Design deviation**: ARCHITECTURE.md's "The `delegate` action" step 3
/// says a child runs over "a child store (`memory` for `--memory`,
/// otherwise a second sqlite region keyed by agent path
/// `implement/ui-review#3`)". This milestone always uses an
/// `InMemoryStore<ReportWorkspace>` for the child regardless of what the
/// parent's own store is: `SqliteStore` gaining a `region` concept is
/// listed under "Changes to existing types (summary)" as a change to make,
/// not something already available to build against, and doing it as part
/// of this unit would mean designing and shipping a new `store` API
/// surface no test in this unit's plan exercises. The child's own steps
/// are still fully visible: every one of its `LogEntry`s is appended to
/// the *shared* session log (tagged via `TaggedLog` with its agent path),
/// which is what "children do not get a sqlite region in this milestone;
/// their steps are still in the shared session log, tagged" (task
/// instructions) describes. A child's workspace state itself (as opposed
/// to its log) does not survive a parent process restart -- only its
/// absorbed `Report`, once committed into the parent's own (persisted)
/// workspace, does.
pub struct ConfiguredChildRunner {
    children: BTreeMap<String, Arc<ChildSkill>>,
    model: Arc<dyn Model>,
    log: Arc<dyn SessionLog>,
    tools: ToolSet,
    tokenizer_factory: TokenizerFactory,
    parent_config: SessionConfig,
    sandbox_root: SandboxRoot,
}

impl ConfiguredChildRunner {
    /// Builds a runner. `tools` is the parent's *full* tool set (each
    /// child's own `ChildSkill::tools()` restricts what it actually gets at
    /// delegation time via `ToolSet::list_for`/`Session`'s tool lookup);
    /// every name in every child's config must already be registered in
    /// `tools`, checked here rather than at delegation time.
    pub fn new(
        children: BTreeMap<String, Arc<ChildSkill>>,
        model: Arc<dyn Model>,
        log: Arc<dyn SessionLog>,
        tools: ToolSet,
        tokenizer_factory: TokenizerFactory,
        parent_config: SessionConfig,
        sandbox_root: SandboxRoot,
    ) -> Result<ConfiguredChildRunner, ChildRunnerError> {
        let known_tools = tools.names();
        for (name, child) in &children {
            for tool in child.tools() {
                if !known_tools.contains(&tool) {
                    return Err(ChildRunnerError::UnknownTool {
                        child: name.clone(),
                        tool,
                    });
                }
            }
        }

        Ok(ConfiguredChildRunner {
            children,
            model,
            log,
            tools,
            tokenizer_factory,
            parent_config,
            sandbox_root,
        })
    }
}

/// Maps a committed `Outcome` to the short label `StepSink::child_step`
/// reports.
fn outcome_label(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Continue => "continue",
        Outcome::Parked(Park::AskUser(_)) => "ask_user",
        Outcome::Parked(Park::Yield) => "yield",
        Outcome::Parked(Park::Failure(_)) => "failure",
        Outcome::Parked(Park::Cancelled) => "cancelled",
        Outcome::Done => "done",
        Outcome::Handoff(_) => "handoff",
    }
}

#[async_trait]
impl ChildRunner for ConfiguredChildRunner {
    fn children(&self) -> Vec<String> {
        self.children.keys().cloned().collect()
    }

    async fn run(
        &self,
        req: ChildRequest,
        sink: &mut dyn StepSink,
    ) -> Result<Report, SessionError> {
        let Some(child_skill) = self.children.get(&req.child).cloned() else {
            // `children()` is what core validates against before calling
            // `run`, so this is only reachable if a caller bypasses that
            // check; treat it the same way core's own "unknown child" path
            // does, as a `Failed` report rather than an `Err` (infrastructure
            // failures are the only `Err` case per the `ChildRunner`
            // contract).
            return Ok(Report {
                agent: req.child.clone(),
                task: req.task.clone(),
                outcome: ReportOutcome::Failed,
                summary: format!("no configured child named `{}`", req.child),
                findings: Vec::new(),
                files: Vec::new(),
                data: serde_json::Map::new(),
                stats: ReportStats::default(),
            });
        };

        let budgets = child_skill
            .budgets()
            .map(|b| statie_core::Budgets {
                total: b.total,
                observation: b.observation,
                augmentation: b.augmentation,
                notes: statie_core::Budgets::default().notes,
                working_set: statie_core::Budgets::default().working_set,
            })
            .unwrap_or(self.parent_config.budgets);

        let child_config = SessionConfig {
            budgets,
            retries: self.parent_config.retries,
            root: self.sandbox_root.clone(),
        };

        let tagged_log: Arc<dyn SessionLog> = Arc::new(TaggedLog::new(
            Arc::clone(&self.log),
            req.agent_path.clone(),
        ));

        let mut session = Session::new(
            Box::new(InMemoryStore::<ReportWorkspace>::new()),
            tagged_log,
            Arc::clone(&self.model),
            self.tools.clone(),
            (self.tokenizer_factory)(),
            child_config,
        );

        let mut active = match session.activate_forced(Arc::clone(&child_skill)).await {
            Ok(a) => a,
            Err(e) => return Err(e),
        };

        let started = Instant::now();
        let max_steps = child_skill.max_steps() as u64;

        let mut steps: u64 = 0;
        let mut server_prompt_tokens: u64 = 0;
        let mut server_completion_tokens: u64 = 0;

        let mut ev = Event::User(req.task.clone());
        let mut final_outcome: Option<Outcome> = None;

        loop {
            if req.cancel.is_cancelled() {
                break;
            }

            let rec = active.step(ev, sink).await?;
            let outcome =
                statie_core::SessionLogExt::append_record(active.session().log().as_ref(), rec)
                    .map_err(SessionError::Log)?;

            let (state, _version) = active.session().state().await?;
            let committed_step = state.harness.step;
            if committed_step > steps {
                steps = committed_step;
                if let Some(usage) = active.last_usage() {
                    server_prompt_tokens += usage.prompt_tokens;
                    server_completion_tokens += usage.completion_tokens;
                }
            }
            sink.child_step(&req.agent_path, steps, outcome_label(&outcome));

            let stop = matches!(outcome, Outcome::Done)
                || matches!(outcome, Outcome::Parked(_))
                || steps >= max_steps;

            if stop {
                final_outcome = Some(outcome);
                break;
            }

            ev = Event::Tick;
        }

        let wall_ms = started.elapsed().as_millis() as u64;
        let stats = ReportStats {
            steps,
            server_prompt_tokens,
            server_completion_tokens,
            wall_ms,
        };

        let (final_state, _version) = active.session().state().await?;
        let mut report = Report::from_workspace(&req.child, &req.task, &final_state.agent, stats);

        let cancelled = req.cancel.is_cancelled();
        let capped_summary = || {
            format!(
                "step cap ({max_steps}) reached; last recent_summary: {}",
                non_empty_or(&final_state.agent.recent_summary, "(none)")
            )
        };

        // Priority order: cancel and the step cap both override whatever the
        // last committed step's `Outcome` was (a child that happened to
        // commit a normal `Continue` on its very last allowed step is still
        // capped, not silently treated as still running); short of those,
        // the child's own workspace (`report.outcome`, from `ws.outcome`)
        // stands for `Done`, and every other park/handoff -- none of which a
        // `ChildSkill` can legitimately produce -- becomes `Failed`.
        if cancelled {
            report.outcome = ReportOutcome::Capped;
            if report.summary.is_empty() {
                report.summary = format!(
                    "cancelled after {steps} step(s); last recent_summary: {}",
                    non_empty_or(&final_state.agent.recent_summary, "(none)")
                );
            }
        } else if steps >= max_steps && !matches!(final_outcome, Some(Outcome::Done)) {
            report.outcome = ReportOutcome::Capped;
            if report.summary.is_empty() {
                report.summary = capped_summary();
            }
        } else {
            match final_outcome {
                Some(Outcome::Done) => {
                    // `rules` already required `done: true` with a non-empty
                    // `summary` before this activation could reach `Done`;
                    // `report.outcome` stands as-is.
                }
                Some(Outcome::Parked(Park::Failure(reason))) => {
                    report.outcome = ReportOutcome::Failed;
                    if report.summary.is_empty() {
                        report.summary = format!("child parked with a failure: {reason}");
                    }
                }
                Some(Outcome::Parked(Park::AskUser(_) | Park::Yield)) => {
                    // Children reject `ask_user`/`yield` at validation time
                    // (`allows_ask_user`/`allows_yield` are both `false` for
                    // `ChildSkill`); unreachable in practice, kept as a
                    // defensive `Failed` rather than a panic.
                    report.outcome = ReportOutcome::Failed;
                    if report.summary.is_empty() {
                        report.summary =
                            "child parked (ask_user/yield), which is not allowed for a subagent"
                                .to_string();
                    }
                }
                Some(Outcome::Parked(Park::Cancelled)) => {
                    report.outcome = ReportOutcome::Capped;
                    if report.summary.is_empty() {
                        report.summary = format!(
                            "cancelled after {steps} step(s); last recent_summary: {}",
                            non_empty_or(&final_state.agent.recent_summary, "(none)")
                        );
                    }
                }
                Some(Outcome::Handoff(target)) => {
                    report.outcome = ReportOutcome::Failed;
                    if report.summary.is_empty() {
                        report.summary = format!(
                            "child attempted a handoff to `{target}`, which is not allowed for a subagent"
                        );
                    }
                }
                Some(Outcome::Continue) | None => {
                    // Reachable only if the loop exits without `stop` having
                    // been set (defensive fallback; the loop's own condition
                    // guarantees `stop` on every exit besides cancellation,
                    // handled above).
                    report.outcome = ReportOutcome::Capped;
                    if report.summary.is_empty() {
                        report.summary = capped_summary();
                    }
                }
            }
        }

        Ok(report)
    }
}

fn non_empty_or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}
