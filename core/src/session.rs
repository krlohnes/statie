//! The session loop: `Session<W>`, activation typestate (`Active<'a, K>`),
//! and the `step` algorithm (ARCHITECTURE.md "Loop, session, activation").

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::SessionError;
use crate::log::{LogEntry, LogKind, SessionLog, StepRecord};
use crate::loop_types::{Event, Executable, Outcome, Park};
use crate::model::{Action, Model, RawOutput, Schema, StepOutput, StepSink, Usage};
use crate::patch::PatchStrategy;
use crate::prompt::{AgentState, Augmentation, HarnessView, PromptBuilder, Snippet, SystemPrompt};
use crate::promptable::{Observation, Promptable};
use crate::sandbox::SandboxRoot;
use crate::skill::{ParsedOutput, Skill, ValidatedStep, validate_user_patch};
use crate::store::{LoggedPatch, StateStore, apply_fn};
use crate::tokenizer::{Budgets, TokenBudget, Tokenizer};
use crate::tool::ToolSet;
use crate::version::{CommittedState, HarnessState, InboxItem, Version};

/// How many recent committed tool calls the loop guard remembers when
/// looking for repeated (name, args, output) triples, including
/// alternating patterns (e.g. A/B/A/B), not just consecutive repeats.
const TOOL_CALL_HISTORY: usize = 8;

/// A cloneable, shareable handle that can cancel the in-flight `step()`
/// call. A fresh `CancellationToken` is installed at the start of every
/// `step`, so a handle obtained before a given step still cancels it (the
/// handle always points at whichever token is "current").
#[derive(Clone)]
pub struct CancelHandle(Arc<Mutex<CancellationToken>>);

impl CancelHandle {
    fn new() -> Self {
        CancelHandle(Arc::new(Mutex::new(CancellationToken::new())))
    }

    /// Installs and returns a fresh token, used for the step about to run.
    fn fresh_token(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut guard = self.0.lock().expect("cancel handle mutex poisoned");
        *guard = token.clone();
        token
    }

    /// Cancels whichever token is currently installed.
    pub fn cancel(&self) {
        let guard = self.0.lock().expect("cancel handle mutex poisoned");
        guard.cancel();
    }
}

/// Session-wide configuration: budgets, retry cap, and sandbox root.
pub struct SessionConfig {
    pub budgets: Budgets,
    pub retries: u32,
    pub root: SandboxRoot,
}

/// A session: persistent workspace store, session log, model backend,
/// augmenters, tools, tokenizer, and config. Owns a sequence of skill
/// activations (`Active<K>`).
pub struct Session<W> {
    pub(crate) store: Box<dyn StateStore<W>>,
    pub(crate) log: Arc<dyn SessionLog>,
    pub(crate) model: Box<dyn Model>,
    pub(crate) augmenters: Vec<(Box<dyn crate::augmenter::Augmenter<W>>, TokenBudget)>,
    pub(crate) tools: ToolSet,
    pub(crate) tokenizer: Box<dyn Tokenizer>,
    pub(crate) config: SessionConfig,
    pending_observation: Option<Observation>,
    /// (tool name, args, output) of the last `TOOL_CALL_HISTORY` committed
    /// tool calls, most-recent-last; used to warn the model when a call
    /// recurs identically, whether consecutively or in an alternating
    /// pattern (e.g. A/B/A/B).
    recent_tool_calls: VecDeque<(String, Value, String)>,
    last_prompt_tokens: Option<usize>,
    last_usage: Option<Usage>,
    failures: u32,
    cancel: CancelHandle,
    last_activation: Option<(String, Vec<String>)>,
}

impl<W> Session<W> {
    pub fn new(
        store: Box<dyn StateStore<W>>,
        log: Arc<dyn SessionLog>,
        model: Box<dyn Model>,
        tools: ToolSet,
        tokenizer: Box<dyn Tokenizer>,
        config: SessionConfig,
    ) -> Self {
        Session {
            store,
            log,
            model,
            augmenters: Vec::new(),
            tools,
            tokenizer,
            config,
            pending_observation: None,
            recent_tool_calls: VecDeque::new(),
            last_prompt_tokens: None,
            last_usage: None,
            failures: 0,
            cancel: CancelHandle::new(),
            last_activation: None,
        }
    }

    pub fn with_augmenter(
        mut self,
        augmenter: Box<dyn crate::augmenter::Augmenter<W>>,
        budget: TokenBudget,
    ) -> Self {
        self.augmenters.push((augmenter, budget));
        self
    }

    /// Sole constructor of `Active`. Checks the activation-transition rule
    /// against the session's remembered last activation, then initializes
    /// the store if empty, or updates `activation`/`active_tools` if not.
    pub async fn activate<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        self.check_transition(skill.name())?;
        self.activate_inner(skill).await
    }

    /// Same as `activate`, but skips the transition-allowed check.
    pub async fn activate_forced<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        self.activate_inner(skill).await
    }

    fn check_transition(&self, to: &str) -> Result<(), SessionError> {
        if let Some((prev, allowed)) = &self.last_activation
            && prev != to
            && !allowed.iter().any(|a| a == to)
        {
            return Err(SessionError::TransitionNotAllowed {
                from: Some(prev.clone()),
                to: to.to_string(),
            });
        }
        Ok(())
    }

    async fn activate_inner<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let name = skill.name().to_string();

        match self.store.load().await? {
            None => {
                let initial = CommittedState {
                    harness: HarnessState {
                        budgets: self.config.budgets,
                        activation: name.clone(),
                        active_tools: skill.tools(),
                        inbox: Vec::new(),
                        step: 0,
                        last_overflow: None,
                    },
                    agent: skill.initial_workspace(),
                };
                self.store.init(initial).await?;
            }
            Some((cur, _version)) => {
                let mut harness = cur.harness;
                harness.activation = name.clone();
                harness.active_tools = skill.tools();
                // The config file, not the persisted snapshot, owns budgets.
                harness.budgets = self.config.budgets;
                self.store.set_harness(harness).await?;
            }
        }

        self.last_activation = Some((name.clone(), skill.allowed_transitions()));

        let entry = LogEntry {
            step: self.log.last_step(),
            kind: LogKind::System,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({"event": "activate", "skill": name}),
        };
        self.log.append(entry)?;

        let spec = skill.spec();
        let schema = Schema::of::<StepOutput<<K::Patch as PatchStrategy<W>>::Patch>>();

        Ok(Active {
            session: self,
            skill,
            spec,
            schema,
        })
    }

    pub async fn state(&self) -> Result<(CommittedState<W>, Version), SessionError> {
        self.store.load().await?.ok_or(SessionError::NoActivation)
    }

    /// Appends a user message to the inbox and logs it.
    pub async fn push_user(&mut self, text: String) -> Result<(), SessionError> {
        let (cur, _version) = self.store.load().await?.ok_or(SessionError::NoActivation)?;
        let mut harness = cur.harness;
        let next_id = harness.inbox.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        harness.inbox.push(InboxItem {
            id: next_id,
            text: text.clone(),
            acked: false,
        });
        self.store.set_harness(harness).await?;

        let entry = LogEntry {
            step: self.log.last_step(),
            kind: LogKind::User,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({"id": next_id, "text": text}),
        };
        self.log.append(entry)?;
        Ok(())
    }

    pub fn log(&self) -> Arc<dyn SessionLog> {
        Arc::clone(&self.log)
    }

    pub async fn patch_log(&self) -> Result<Vec<LoggedPatch>, SessionError> {
        Ok(self.store.patch_log().await?)
    }

    /// Token count of the most recently built prompt, if any.
    pub fn last_prompt_tokens(&self) -> Option<usize> {
        self.last_prompt_tokens
    }

    /// Server-reported usage from the most recent model call, if any.
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }
}

/// An active skill activation: the sole way to call `step`. Only
/// `Session::activate`/`activate_forced` construct one (A8).
pub struct Active<'a, K: Skill + ?Sized> {
    session: &'a mut Session<K::Workspace>,
    skill: Arc<K>,
    spec: Arc<crate::skill::SkillSpec>,
    schema: Schema,
}

impl<'a, K: Skill + ?Sized> Active<'a, K>
where
    K::Workspace: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    pub async fn step(
        &mut self,
        ev: Event,
        sink: &mut dyn StepSink,
    ) -> Result<StepRecord, SessionError> {
        let mut entries: Vec<LogEntry> = Vec::new();

        // 1. Handle the incoming event.
        match ev {
            Event::User(text) => {
                self.session.push_user(text).await?;
            }
            Event::Cancel => {
                self.session.cancel.cancel();
                let (cur, _version) = self.session.state().await?;
                let step = cur.harness.step;
                entries.push(system_entry(step, "cancelled", serde_json::json!({})));
                return Ok(StepRecord::new(entries, Outcome::Parked(Park::Cancelled)));
            }
            Event::Tick => {}
        }

        // Fresh cancellation token for this step; no prompt has been built yet,
        // so a step that ends before calling the model reports no token count.
        let token = self.session.cancel.fresh_token();
        self.session.last_prompt_tokens = None;
        self.session.last_usage = None;

        // 2. Load state + version; terminal check.
        let (cur, version) = self.session.state().await?;
        let step_at_start = cur.harness.step;

        if let Some(target) = self.skill.handoff(&cur.agent) {
            entries.push(system_entry(
                step_at_start,
                "handoff",
                serde_json::json!({"to": target}),
            ));
            return Ok(StepRecord::new(entries, Outcome::Handoff(target)));
        }
        if self.skill.is_terminal(&cur.agent) {
            entries.push(system_entry(step_at_start, "done", serde_json::json!({})));
            return Ok(StepRecord::new(entries, Outcome::Done));
        }

        // 3. Run augmenters.
        let obs_ref = self.session.pending_observation.as_ref();
        let mut snippets: Vec<Snippet> = Vec::new();
        for (augmenter, budget) in &self.session.augmenters {
            let budget = TokenBudget(budget.0);
            let mut got = augmenter
                .augment(&cur.agent, obs_ref, budget, self.session.tokenizer.as_ref())
                .await;
            snippets.append(&mut got);
        }
        // Augmentation is derived, never load-bearing: clamp it to the
        // section budget by dropping trailing snippets rather than failing
        // the step. Per-augmenter budgets bound each source; this bounds the sum.
        let aug_cap = cur.harness.budgets.augmentation;
        while !snippets.is_empty()
            && self
                .session
                .tokenizer
                .count(&Augmentation::from_snippets(snippets.clone()).render())
                > aug_cap
        {
            snippets.pop();
        }
        let augmentation = if snippets.is_empty() {
            None
        } else {
            Some(Augmentation::from_snippets(snippets))
        };

        // 4. Build the bounded prompt.
        let caps = self.session.model.capabilities();
        let system_prompt = SystemPrompt::for_harness(
            &caps,
            <K::Patch as PatchStrategy<K::Workspace>>::name(),
            <K::Patch as PatchStrategy<K::Workspace>>::describe(),
            Some(&self.schema),
        );
        let tool_list = self.session.tools.list_for(&cur.harness.active_tools);
        let agent_state = AgentState::new(&cur.agent);
        let harness_view = HarnessView::new(&cur.harness);

        let mut builder = PromptBuilder::new(
            self.session.tokenizer.as_ref(),
            cur.harness.budgets,
            version,
        )
        .system(&system_prompt)
        .spec(&self.spec)
        .tools(&tool_list)
        .agent(&agent_state)
        .harness(&harness_view);
        if let Some(aug) = &augmentation {
            builder = builder.augmentation(aug);
        }
        // Clamp defensively before handing the observation to the builder:
        // `truncate` already bounds fresh tool output, but harness-authored
        // messages (`rejection`, `tool_aborted`, ...) are never truncated at
        // construction time and a long one (e.g. a tool error carrying
        // stderr, or an unknown-tool name) could otherwise exceed the
        // section budget and fail the step. The builder's per-section check
        // stays strict as the real invariant; this is what keeps it true.
        let pending_obs = self.session.pending_observation.take().map(|obs| {
            obs.clamp(
                cur.harness.budgets.observation,
                self.session.tokenizer.as_ref(),
            )
        });
        if let Some(obs) = &pending_obs {
            builder = builder.observation(obs);
        }
        let prompt = builder.build()?;
        self.session.last_prompt_tokens = Some(prompt.tokens());

        // 5. Call the model.
        let raw: RawOutput = match self
            .session
            .model
            .step(prompt, &self.schema, sink, token.clone())
            .await
        {
            Ok(raw) => raw,
            Err(crate::error::ModelError::Cancelled) => {
                // The model never answered; keep the observation for the next attempt.
                self.session.pending_observation = pending_obs;
                entries.push(system_entry(
                    step_at_start,
                    "cancelled",
                    serde_json::json!({}),
                ));
                return Ok(StepRecord::new(entries, Outcome::Parked(Park::Cancelled)));
            }
            Err(e) => return Err(SessionError::Model(e)),
        };

        // Recorded immediately after the model call, before parsing, so a
        // rejected output still counts toward real cost per model call.
        self.session.last_usage = raw.usage;
        let finish_reason = raw.finish_reason.clone();

        if let Some(reasoning) = &raw.reasoning {
            entries.push(LogEntry {
                step: step_at_start,
                kind: LogKind::Reasoning,
                ts: chrono::Utc::now().to_rfc3339(),
                payload: serde_json::json!({"text": reasoning.as_ref()}),
            });
        }

        // 6. Parse. The raw content is kept for the log so a rejection can be
        // diagnosed from the session log alone (it never re-enters a prompt).
        let raw_content = raw.content.clone();
        let parsed: ParsedOutput<K::Workspace> =
            match ParsedOutput::parse_with_fixes::<K::Patch>(raw) {
                Ok((p, fixes)) => {
                    if !fixes.is_empty() {
                        entries.push(system_entry(
                            step_at_start,
                            "normalized",
                            serde_json::json!({"fixes": fixes}),
                        ));
                    }
                    p
                }
                Err(e) => {
                    return Ok(self.reject(
                        entries,
                        step_at_start,
                        e.to_string(),
                        &raw_content,
                        finish_reason.as_deref(),
                    ));
                }
            };

        // 7. Validate.
        let validated: ValidatedStep<K::Workspace> = match self.skill.validate(&cur, parsed) {
            Ok(v) => v,
            Err(e) => {
                return Ok(self.reject(entries, step_at_start, e.to_string(), &raw_content, None));
            }
        };

        // 8. Commit (B1 stale-version check first).
        let (patch, action, acks) = validated.into_parts();
        if patch.version() != version {
            return Ok(self.reject(
                entries,
                step_at_start,
                "stale version".to_string(),
                &raw_content,
                None,
            ));
        }

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        for item in &mut next_harness.inbox {
            if acks.contains(&item.id) {
                item.acked = true;
            }
        }
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();

        let patch_raw = patch.raw().clone();
        let new_version = self
            .session
            .store
            .commit(patch, next_harness.clone(), &action, version)
            .await?;
        self.session.failures = 0;

        let mut patch_payload = serde_json::json!({"version": new_version, "patch": patch_raw});
        if let Some(usage) = self.session.last_usage
            && let Value::Object(map) = &mut patch_payload
        {
            map.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or(Value::Null),
            );
        }
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: patch_payload,
        });
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Action,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::to_value(&action).unwrap_or(serde_json::Value::Null),
        });

        let executable = Executable::new(action);

        // 9. Execute.
        let outcome = self
            .execute(executable, next_harness.step, &mut entries, token)
            .await?;

        Ok(StepRecord::new(entries, outcome))
    }

    async fn execute(
        &mut self,
        executable: Executable<Action>,
        step: u64,
        entries: &mut Vec<LogEntry>,
        token: CancellationToken,
    ) -> Result<Outcome, SessionError> {
        match executable.into_inner() {
            Action::Tool { name, args } => {
                // A missing/null `args` means "no arguments".
                let args = if args.is_null() {
                    Value::Object(Default::default())
                } else {
                    args
                };
                let Some(tool) = self.session.tools.get(&name) else {
                    entries.push(system_entry(
                        step,
                        "tool_error",
                        serde_json::json!({"name": name, "reason": "unknown tool"}),
                    ));
                    self.session.pending_observation =
                        Some(Observation::rejection(&format!("unknown tool `{name}`")));
                    return Ok(Outcome::Continue);
                };

                let ctx = crate::tool::ToolCtx {
                    root: self.session.config.root.clone(),
                    log: Arc::clone(&self.session.log),
                    cancel: token.clone(),
                };

                let result = tokio::select! {
                    res = tool.call(args.clone(), &ctx) => Some(res),
                    _ = token.cancelled() => None,
                };

                match result {
                    None => {
                        entries.push(system_entry(
                            step,
                            "cancelled",
                            serde_json::json!({"tool": name}),
                        ));
                        self.session.pending_observation = Some(Observation::tool_aborted(&name));
                        Ok(Outcome::Parked(Park::Cancelled))
                    }
                    Some(Ok(full)) => {
                        entries.push(LogEntry {
                            step,
                            kind: LogKind::ToolOutput,
                            ts: chrono::Utc::now().to_rfc3339(),
                            payload: serde_json::json!({
                                "name": name,
                                "output": full.as_ref(),
                                "truncated": false,
                            }),
                        });
                        // Empty output is still information; say so explicitly.
                        let mut text = if full.as_ref().trim().is_empty() {
                            format!("(tool `{name}` returned no output)")
                        } else {
                            full.as_ref().to_string()
                        };
                        // Loop guard: identical (name, args, output) triple recurring within
                        // the recent-history window, whether consecutively or alternating
                        // (e.g. A/B/A/B).
                        let current = (name.clone(), args.clone(), full.as_ref().to_string());
                        let prior_occurrences = self
                            .session
                            .recent_tool_calls
                            .iter()
                            .filter(|entry| **entry == current)
                            .count();
                        if prior_occurrences >= 2 {
                            let occurrence = prior_occurrences + 1;
                            text.push_str(&format!(
                                "\n\n[harness] This is occurrence #{occurrence} of `{name}` with \
                                 identical arguments and identical output within the last \
                                 {TOOL_CALL_HISTORY} tool calls; nothing has changed. Record what \
                                 you learned in the workspace and take a different action (if the \
                                 work is finished, hand off with `next_skill` or set the terminal \
                                 field)."
                            ));
                        }
                        self.session.recent_tool_calls.push_back(current);
                        if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                            self.session.recent_tool_calls.pop_front();
                        }
                        let (obs, overflow) = Observation::truncate_with_report(
                            &crate::promptable::ToolOutputFull::new(text),
                            self.session.config.budgets.observation,
                            self.session.tokenizer.as_ref(),
                        );
                        self.session.pending_observation = Some(obs);

                        // Persist a note in the harness view about whether
                        // this tool's output overflowed the observation
                        // budget, so the model is told persistently (not
                        // just in the one prompt where the truncated
                        // observation is shown).
                        let note = overflow.map(|o| {
                            format!(
                                "step {step}: `{name}` output overflowed ({} chars, ~{} tokens > \
                                 {} budget); only the head and tail were shown. Narrow the next \
                                 call or use `filter_last_tool_call`.",
                                o.total_chars,
                                o.total_tokens,
                                self.session.config.budgets.observation
                            )
                        });
                        let (cur, _version) = self.session.state().await?;
                        let mut harness = cur.harness;
                        harness.last_overflow = note;
                        self.session.store.set_harness(harness).await?;

                        Ok(Outcome::Continue)
                    }
                    Some(Err(e)) => {
                        entries.push(system_entry(
                            step,
                            "tool_error",
                            serde_json::json!({"name": name, "reason": e.to_string()}),
                        ));
                        self.session.pending_observation = Some(Observation::rejection(&format!(
                            "tool `{name}` failed: {e}"
                        )));
                        Ok(Outcome::Continue)
                    }
                }
            }
            Action::AskUser { message } => Ok(Outcome::Parked(Park::AskUser(message))),
            Action::Yield => Ok(Outcome::Parked(Park::Yield)),
            Action::Done => {
                let (cur, _version) = self.session.state().await?;
                match self.skill.handoff(&cur.agent) {
                    Some(target) => Ok(Outcome::Handoff(target)),
                    None => Ok(Outcome::Done),
                }
            }
        }
    }

    fn reject(
        &mut self,
        mut entries: Vec<LogEntry>,
        step: u64,
        reason: String,
        raw_content: &str,
        finish_reason: Option<&str>,
    ) -> StepRecord {
        let reason = match finish_reason {
            Some("length") => {
                let completion_tokens = self
                    .session
                    .last_usage
                    .map(|u| u.completion_tokens)
                    .unwrap_or(0);
                format!(
                    "{reason}; the server cut the output off (done_reason=length after \
                     {completion_tokens} tokens). Write less per step: use edit_file for a \
                     small change instead of write_file for a whole file, or split the file \
                     across steps."
                )
            }
            Some(other) if other != "stop" => format!("{reason} (done_reason={other})"),
            _ => reason,
        };
        self.session.pending_observation = Some(Observation::rejection(&reason));
        self.session.failures += 1;
        let content: String = raw_content.chars().take(16000).collect();
        let mut rejected_payload = serde_json::json!({"reason": reason, "content": content});
        if let Some(usage) = self.session.last_usage
            && let Value::Object(map) = &mut rejected_payload
        {
            map.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or(Value::Null),
            );
        }
        entries.push(system_entry(step, "rejected", rejected_payload));

        if self.session.failures > self.session.config.retries {
            self.session.failures = 0;
            entries.push(system_entry(
                step,
                "failure",
                serde_json::json!({"reason": reason}),
            ));
            StepRecord::new(entries, Outcome::Parked(Park::Failure(reason)))
        } else {
            StepRecord::new(entries, Outcome::Continue)
        }
    }

    /// User-authored patch (`/edit`): applies the strategy and skill rules,
    /// but skips the inbox-ack check (the action is always `Yield`).
    pub async fn user_patch(&mut self, patch: Value) -> Result<StepRecord, SessionError> {
        let (cur, version) = self.session.state().await?;

        let checked = validate_user_patch::<K::Workspace, K::Patch>(
            self.skill.as_ref(),
            &cur,
            crate::patch::RawPatch(patch),
            version,
        )
        .map_err(SessionError::Patch)?;
        let step = ValidatedStep::user(checked);
        let (validated, action, _acks) = step.into_parts();

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();

        let patch_raw = validated.raw().clone();
        let new_version = self
            .session
            .store
            .commit(validated, next_harness.clone(), &action, version)
            .await?;
        self.session.failures = 0;

        let mut entries = Vec::new();
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "version": new_version,
                "patch": patch_raw,
                "source": "user",
            }),
        });
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Action,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::to_value(&action).unwrap_or(serde_json::Value::Null),
        });
        Ok(StepRecord::new(entries, Outcome::Parked(Park::Yield)))
    }

    /// Replays the patch log up to `to` using this skill's patch strategy.
    pub async fn replay(&self, to: Version) -> Result<CommittedState<K::Workspace>, SessionError> {
        let apply = apply_fn::<K::Workspace, K::Patch>();
        Ok(self.session.store.replay(to, &apply).await?)
    }

    pub fn session(&mut self) -> &mut Session<K::Workspace> {
        self.session
    }

    /// Token count of the most recently built prompt, if any.
    pub fn last_prompt_tokens(&self) -> Option<usize> {
        self.session.last_prompt_tokens
    }

    /// Server-reported usage from the most recent model call, if any.
    /// `None` when the step made no model call (e.g. terminal/handoff), or
    /// when the backend reported none.
    pub fn last_usage(&self) -> Option<Usage> {
        self.session.last_usage
    }

    pub fn skill_name(&self) -> &str {
        self.skill.name()
    }
}

fn system_entry(step: u64, event: &str, mut payload: Value) -> LogEntry {
    if let Value::Object(map) = &mut payload {
        map.insert("event".to_string(), Value::String(event.to_string()));
    }
    LogEntry {
        step,
        kind: LogKind::System,
        ts: chrono::Utc::now().to_rfc3339(),
        payload,
    }
}
