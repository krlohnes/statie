//! Bounded prompt construction (A5, B1) and the promptable section types
//! that aren't defined elsewhere (`SystemPrompt`, `ToolList`, `AgentState`,
//! `HarnessView`).

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::model::Capabilities;
use crate::model::Schema;
use crate::promptable::{Notes, Observation, Promptable, sealed};
use crate::skill::{Commitment, SkillSpec};
use crate::tokenizer::{Budgets, Tokenizer};
use crate::tool::ToolList;
use crate::version::{HarnessState, Version};
use crate::working_set::WorkingSet;

/// One of the nine fixed prompt sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Section {
    System,
    Spec,
    Tools,
    AgentState,
    Notes,
    HarnessView,
    WorkingSet,
    Augmentation,
    Observation,
}

impl Section {
    fn heading(self) -> &'static str {
        match self {
            Section::System => "System",
            Section::Spec => "Spec",
            Section::Tools => "Tools",
            Section::AgentState => "Agent State",
            Section::Notes => "notes (kept by the harness from your previous reasoning)",
            Section::HarnessView => "Harness View",
            Section::WorkingSet => {
                "working set (what you read recently; derived from the log, not state):"
            }
            Section::Augmentation => "Augmentation",
            Section::Observation => "Observation",
        }
    }
}

/// A section over budget. `system()`/`user()`/`build()` never silently
/// truncate; they return this instead.
#[derive(Debug, thiserror::Error)]
#[error("prompt section {section:?} over budget: {tokens} > {limit}")]
pub struct OverBudget {
    pub section: Section,
    pub tokens: usize,
    pub limit: usize,
}

/// Builds a `BoundedPrompt`. Sections may be supplied in any call order;
/// they are always emitted in the fixed order System, Spec, Tools,
/// AgentState, Notes, HarnessView, Augmentation, Observation. A section
/// that was never supplied is simply omitted.
pub struct PromptBuilder<'t> {
    tokenizer: &'t dyn Tokenizer,
    budgets: Budgets,
    version: Version,
    rendered: [Option<String>; 9],
}

const IDX_SYSTEM: usize = 0;
const IDX_SPEC: usize = 1;
const IDX_TOOLS: usize = 2;
const IDX_AGENT: usize = 3;
const IDX_NOTES: usize = 4;
const IDX_HARNESS: usize = 5;
const IDX_WORKING_SET: usize = 6;
const IDX_AUGMENTATION: usize = 7;
const IDX_OBSERVATION: usize = 8;

const SECTION_ORDER: [Section; 9] = [
    Section::System,
    Section::Spec,
    Section::Tools,
    Section::AgentState,
    Section::Notes,
    Section::HarnessView,
    Section::WorkingSet,
    Section::Augmentation,
    Section::Observation,
];

impl<'t> PromptBuilder<'t> {
    pub fn new(tokenizer: &'t dyn Tokenizer, budgets: Budgets, version: Version) -> Self {
        PromptBuilder {
            tokenizer,
            budgets,
            version,
            rendered: Default::default(),
        }
    }

    fn set(mut self, idx: usize, rendered: String) -> Self {
        self.rendered[idx] = Some(rendered);
        self
    }

    pub fn system(self, s: &SystemPrompt) -> Self {
        let text = s.render();
        self.set(IDX_SYSTEM, text)
    }

    pub fn spec(self, s: &Arc<SkillSpec>) -> Self {
        let text = s.render();
        self.set(IDX_SPEC, text)
    }

    pub fn tools(self, t: &ToolList) -> Self {
        let text = t.render();
        self.set(IDX_TOOLS, text)
    }

    pub fn agent<S: Serialize>(self, a: &AgentState<'_, S>) -> Self {
        let text = a.render();
        self.set(IDX_AGENT, text)
    }

    /// Renders the harness-maintained notes section. `None` (no maintainer
    /// configured, or nothing maintained yet) or empty notes render
    /// nothing — the same as never calling this method.
    pub fn notes(self, n: Option<&Notes>) -> Self {
        match n {
            Some(n) if !n.as_str().is_empty() => {
                let text = n.render();
                self.set(IDX_NOTES, text)
            }
            _ => self,
        }
    }

    pub fn harness(self, h: &HarnessView<'_>) -> Self {
        let text = h.render();
        self.set(IDX_HARNESS, text)
    }

    /// Renders the working-set section (statie-spec.md "10. Working set").
    /// `None` (disabled, or nothing to show) renders nothing -- the same as
    /// never calling this method.
    pub fn working_set(self, w: Option<&WorkingSet>) -> Self {
        match w {
            Some(w) => {
                let text = w.render();
                self.set(IDX_WORKING_SET, text)
            }
            None => self,
        }
    }

    pub fn augmentation(self, a: &Augmentation) -> Self {
        let text = a.render();
        self.set(IDX_AUGMENTATION, text)
    }

    pub fn observation(self, o: &Observation) -> Self {
        let text = o.render();
        self.set(IDX_OBSERVATION, text)
    }

    pub fn build(self) -> Result<BoundedPrompt, OverBudget> {
        // Per-section budget checks (only augmentation and observation have
        // section-specific caps; the rest are covered by the total check).
        if let Some(text) = &self.rendered[IDX_AUGMENTATION] {
            let tokens = self.tokenizer.count(text);
            if tokens > self.budgets.augmentation {
                return Err(OverBudget {
                    section: Section::Augmentation,
                    tokens,
                    limit: self.budgets.augmentation,
                });
            }
        }
        if let Some(text) = &self.rendered[IDX_OBSERVATION] {
            let tokens = self.tokenizer.count(text);
            if tokens > self.budgets.observation {
                return Err(OverBudget {
                    section: Section::Observation,
                    tokens,
                    limit: self.budgets.observation,
                });
            }
        }
        if let Some(text) = &self.rendered[IDX_NOTES] {
            let tokens = self.tokenizer.count(text);
            if tokens > self.budgets.notes {
                return Err(OverBudget {
                    section: Section::Notes,
                    tokens,
                    limit: self.budgets.notes,
                });
            }
        }
        if let Some(text) = &self.rendered[IDX_WORKING_SET] {
            let tokens = self.tokenizer.count(text);
            if tokens > self.budgets.working_set {
                return Err(OverBudget {
                    section: Section::WorkingSet,
                    tokens,
                    limit: self.budgets.working_set,
                });
            }
        }

        // Headed text per slot, indexed the same way as `self.rendered`.
        let mut headed: [Option<String>; 9] = Default::default();
        for (idx, section) in SECTION_ORDER.iter().enumerate() {
            if let Some(text) = &self.rendered[idx] {
                headed[idx] = Some(format!("## {}\n{}", section.heading(), text));
            }
        }

        let full: String = headed
            .iter()
            .flatten()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let total_tokens = self.tokenizer.count(&full);
        if total_tokens > self.budgets.total {
            // Attribute the overage to the largest single section for a
            // more useful error, falling back to System if there is a tie
            // or nothing rendered.
            let worst = SECTION_ORDER
                .iter()
                .enumerate()
                .filter_map(|(idx, section)| headed[idx].as_ref().map(|text| (*section, text)))
                .max_by_key(|(_, text)| self.tokenizer.count(text))
                .map(|(sec, _)| sec)
                .unwrap_or(Section::System);
            return Err(OverBudget {
                section: worst,
                tokens: total_tokens,
                limit: self.budgets.total,
            });
        }

        let system: String = [IDX_SYSTEM, IDX_SPEC, IDX_TOOLS]
            .iter()
            .filter_map(|&i| headed[i].as_ref())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        let user: String = [
            IDX_AGENT,
            IDX_NOTES,
            IDX_HARNESS,
            IDX_WORKING_SET,
            IDX_AUGMENTATION,
            IDX_OBSERVATION,
        ]
        .iter()
        .filter_map(|&i| headed[i].as_ref())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");

        Ok(BoundedPrompt {
            system,
            user,
            version: self.version,
            tokens: total_tokens,
        })
    }
}

/// A prompt that has been checked against every budget. `system()` is
/// sections System+Spec+Tools; `user()` is AgentState+HarnessView+
/// Augmentation+Observation.
#[derive(Debug)]
pub struct BoundedPrompt {
    system: String,
    user: String,
    version: Version,
    tokens: usize,
}

impl BoundedPrompt {
    pub fn system(&self) -> &str {
        &self.system
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    /// Builds a `BoundedPrompt` directly from a system prompt and a single
    /// user-message string, bypassing `PromptBuilder`'s eight fixed
    /// sections. Used by the maintainer call (`crate::maintainer`), whose
    /// prompt shape (one system block, one user block with its own
    /// labelled sub-blocks) doesn't fit the worker's section set without
    /// laundering it through `AgentState`/`HarnessView`/etc.
    pub(crate) fn two_part(
        system: &SystemPrompt,
        user: String,
        tokens: usize,
        version: Version,
    ) -> Self {
        BoundedPrompt {
            system: system.render(),
            user,
            version,
            tokens,
        }
    }
}

/// The harness system prompt: output contract, patch strategy description,
/// action semantics, tool call convention, recall/step explanation, inbox
/// ack rule, and (conditionally) the `<thoughts>` instruction and schema
/// text.
pub struct SystemPrompt(String);

impl SystemPrompt {
    /// Render the harness system prompt for a given backend's capabilities
    /// and patch strategy.
    pub fn for_harness(
        caps: &Capabilities,
        strategy_name: &str,
        strategy_desc: &str,
        schema: Option<&Schema>,
        delegates: &[String],
    ) -> Self {
        let mut s = String::new();
        s.push_str(
            "You are operating inside the statie harness. Each step, you must respond \
             with exactly one JSON object of the form {\"patch\": ..., \"action\": ..., \
             \"ack_inbox\": [...]} and nothing else — no prose outside the object.\n\n",
        );
        s.push_str(&format!(
            "State updates use the `{strategy_name}` patch strategy: {strategy_desc}\n\n"
        ));
        if delegates.is_empty() {
            s.push_str(
                "The `action` field is one of: `tool` (call a named tool with `args`), \
                 `ask_user` (park and ask the user a question), `yield` (park and wait for \
                 the user), or `done` (end the activation). To call a tool, set \
                 action.type to \"tool\" with the tool's `name` and its `args` matching that \
                 tool's schema from the Tools section.\n\n",
            );
        } else {
            s.push_str(&format!(
                "The `action` field is one of: `tool` (call a named tool with `args`), \
                 `ask_user` (park and ask the user a question), `yield` (park and wait for \
                 the user), `done` (end the activation), or `delegate` (hand a self-contained \
                 job to a subagent and get back a `Report`). To call a tool, set \
                 action.type to \"tool\" with the tool's `name` and its `args` matching that \
                 tool's schema from the Tools section. To delegate, set action.type to \
                 \"delegate\" with `skill` naming one of {delegates:?} and `task` describing \
                 the job.\n\n"
            ));
        }
        // Which example lines to print: when `schema` is given, read its
        // action.type enum so a skill that forbids `yield`/`ask_user`/
        // `delegate` (restrict_action_types having narrowed the enum
        // accordingly, see session.rs) never sees an example shape for an
        // action its own rules will reject. `schema == None` keeps today's
        // behaviour (all applicable lines, gated only by `delegates`).
        let enum_types: Option<Vec<&str>> = schema.and_then(|s| {
            s.as_value()
                .pointer("/properties/action/properties/type/enum")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        });
        let show = |ty: &str| enum_types.as_ref().is_none_or(|types| types.contains(&ty));

        s.push_str(
            "Exact shapes (copy these; `action` always carries `type`, `name`, and `args`; non-tool actions use \"\" and {}):\n",
        );
        let has_delegates = !delegates.is_empty();
        if has_delegates {
            s.push_str(
                "{\"patch\": {...}, \"action\": {\"type\": \"tool\", \"name\": \"list_files\", \"args\": {\"path\": \".\"}, \"message\": \"\", \"skill\": \"\", \"task\": \"\"}, \"ack_inbox\": [1]}\n",
            );
        } else {
            s.push_str(
                "{\"patch\": {...}, \"action\": {\"type\": \"tool\", \"name\": \"list_files\", \"args\": {\"path\": \".\"}, \"message\": \"\"}, \"ack_inbox\": [1]}\n",
            );
        }
        if show("ask_user") {
            if has_delegates {
                s.push_str(
                    "{\"patch\": {...}, \"action\": {\"type\": \"ask_user\", \"name\": \"\", \"args\": {}, \"message\": \"Which file?\", \"skill\": \"\", \"task\": \"\"}, \"ack_inbox\": []}\n",
                );
            } else {
                s.push_str(
                    "{\"patch\": {...}, \"action\": {\"type\": \"ask_user\", \"name\": \"\", \"args\": {}, \"message\": \"Which file?\"}, \"ack_inbox\": []}\n",
                );
            }
        }
        if show("yield") {
            if has_delegates {
                s.push_str(
                    "{\"patch\": {...}, \"action\": {\"type\": \"yield\", \"name\": \"\", \"args\": {}, \"message\": \"\", \"skill\": \"\", \"task\": \"\"}, \"ack_inbox\": []}\n",
                );
            } else {
                s.push_str(
                    "{\"patch\": {...}, \"action\": {\"type\": \"yield\", \"name\": \"\", \"args\": {}, \"message\": \"\"}, \"ack_inbox\": []}\n",
                );
            }
        }
        if has_delegates {
            s.push_str(
                "{\"patch\": {...}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": {}, \"message\": \"\", \"skill\": \"\", \"task\": \"\"}, \"ack_inbox\": []}\n",
            );
        } else {
            s.push_str(
                "{\"patch\": {...}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": {}, \"message\": \"\"}, \"ack_inbox\": []}\n",
            );
        }
        if has_delegates && show("delegate") {
            let example_delegate = delegates.first().map(String::as_str).unwrap_or("ui-review");
            s.push_str(&format!(
                "{{\"patch\": {{...}}, \"action\": {{\"type\": \"delegate\", \"name\": \"\", \"args\": {{}}, \"message\": \"\", \"skill\": \"{example_delegate}\", \"task\": \"...\"}}, \"ack_inbox\": []}}\n"
            ));
        }
        s.push_str("`done` means this activation is complete; to keep working, call a tool.\n\n");
        s.push_str(
            "The session history is searchable with the `recall` tool; `step` in the \
             harness view counts committed steps, not model turns.\n\n",
        );
        s.push_str(
            "The harness view lists unacknowledged inbox items. Set `ack_inbox` to the \
             ids of every inbox item you have read and responded to. You may not `yield`, \
             `ask_user`, or `done` while any inbox item remains unacked.\n\n",
        );
        if !caps.native_thinking {
            s.push_str(
                "Before the JSON object, open with a <thoughts>...</thoughts> block \
                 containing your reasoning, then emit the JSON object on its own.\n\n",
            );
        }
        if !caps.schema_constrained
            && let Some(schema) = schema
        {
            s.push_str("Your output must conform to this JSON Schema:\n");
            s.push_str(
                &serde_json::to_string_pretty(schema.as_value())
                    .unwrap_or_else(|_| "{}".to_string()),
            );
            s.push('\n');
        }
        SystemPrompt(s)
    }

    /// The fixed system prompt for a context-maintenance call
    /// (statie-spec.md "9. Context maintenance"): the maintainer's role,
    /// its output contract (including that `next.tool` must be one of the
    /// active tools listed in the input, and that `shell` must never be
    /// proposed just to read a file), the `notes` token cap, and the six
    /// priorities from spec section 9, verbatim and in order.
    pub fn for_maintainer(budget_tokens: usize) -> Self {
        let mut s = String::new();
        s.push_str(
            "You maintain the notes of a coding agent that forgets its reasoning between \
             steps. You will see its reasoning for the step just taken, the patch and action \
             it sent, what that action returned, its current state, its open commitment, and \
             the previous notes.\n\n",
        );
        s.push_str(
            "Respond with exactly one JSON object {\"notes\": \"<text>\", \"next\": <commitment \
             object or null>} and nothing else. `next` has the shape {\"tool\", \"path\", \
             \"what\"} and is only for a decision to make a specific mutating call; `tool` is \
             one of edit_file, write_file, shell, or test. `next.tool` must be one of the \
             active tools listed in the input; never propose a tool that is not active, and \
             never propose `shell` just to read a file (reading is not a commitment). `next` is \
             only ever the mutating call the reasoning decided on (edit_file, write_file, \
             shell, or test) with a non-empty one-line `what`; a read is never a \
             commitment.\n\n",
        );
        s.push_str(&format!(
            "notes must fit in {budget_tokens} tokens; cut from the end.\n\n"
        ));
        s.push_str(
            "Priorities, in order:\n\
             1. Quote only from the inputs. Never invent a line number, a path, an anchor, or \
             a decision that is not in the reasoning or the observation.\n\
             2. Keep verbatim whatever the next mutating call needs: line numbers, anchor \
             text, exact error text, the `old`/`new` of an edit that failed on an argument \
             slip.\n\
             3. Keep decisions with their reasons, and the fact that a decision was reversed \
             if it was.\n\
             4. Drop narration, restatements of the task, and anything already in the plan or \
             the observation.\n\
             5. If the reasoning contains a decision to make a specific mutating call and \
             `commitment` is null, emit it as `next` in the commitment shape.\n\
             6. Order by what is needed next; cut from the end to fit the budget.\n",
        );
        SystemPrompt(s)
    }
}

impl sealed::Sealed for SystemPrompt {}
impl Promptable for SystemPrompt {
    fn render(&self) -> String {
        self.0.clone()
    }
}

/// Renders `Committed.agent` as pretty JSON.
pub struct AgentState<'a, S: Serialize>(&'a S);

impl<'a, S: Serialize> AgentState<'a, S> {
    pub fn new(state: &'a S) -> Self {
        AgentState(state)
    }
}

impl<'a, S: Serialize> sealed::Sealed for AgentState<'a, S> {}
impl<'a, S: Serialize> Promptable for AgentState<'a, S> {
    fn render(&self) -> String {
        serde_json::to_string_pretty(self.0).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Renders the unacked inbox, step counter, activation name, budgets, and
/// (when set) the agent workspace's commitment.
pub struct HarnessView<'a> {
    state: &'a HarnessState,
    /// The skill's current commitment, if any, paired with how many reads
    /// on its path remain before the harness expects the call. Filled in by
    /// the session from `skill.commitment(&cur.agent)` and
    /// `commitment_reads` -- the commitment itself lives in the agent
    /// workspace, not `HarnessState`, so it can't be read off `state` alone.
    commitment: Option<(Commitment, u32)>,
}

impl<'a> HarnessView<'a> {
    pub fn new(state: &'a HarnessState) -> Self {
        HarnessView {
            state,
            commitment: None,
        }
    }

    /// Builder: attaches the skill's current commitment (tool/path/what)
    /// plus reads remaining on its path, so `render` can show the
    /// `next: ...` line. `None` renders exactly as `new` alone.
    pub fn with_commitment(mut self, commitment: Option<(Commitment, u32)>) -> Self {
        self.commitment = commitment;
        self
    }
}

impl<'a> sealed::Sealed for HarnessView<'a> {}
impl<'a> Promptable for HarnessView<'a> {
    fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("activation: {}\n", self.state.activation));
        s.push_str(&format!("step: {}\n", self.state.step));
        s.push_str(&format!(
            "budgets: total={} observation={} augmentation={}\n",
            self.state.budgets.total,
            self.state.budgets.observation,
            self.state.budgets.augmentation
        ));
        if let Some(note) = &self.state.last_overflow {
            s.push_str(&format!("last_observation: {note}\n"));
        }
        if let Some(pinned) = &self.state.pinned {
            let remaining = pinned.expires_at_step.saturating_sub(self.state.step);
            s.push_str(&format!(
                "pinned: {} ({remaining} steps left)\n",
                pinned.name
            ));
        }
        if let Some(green_step) = self.state.green_step {
            if self.state.green_filtered {
                s.push_str(&format!(
                    "verification: passed at step {green_step} (filtered; run the unfiltered \
                     target before approving)\n"
                ));
            } else {
                s.push_str(&format!("verification: passed at step {green_step}\n"));
            }
        }
        if let Some((c, reads_left)) = &self.commitment {
            let path = c.path.as_deref().unwrap_or("(no path)");
            s.push_str(&format!(
                "next: {} {} — {} ({reads_left} reads left before you must act)\n",
                c.tool, path, c.what
            ));
        }
        s.push_str("inbox:\n");
        let unacked: Vec<_> = self.state.inbox.iter().filter(|i| !i.acked).collect();
        if unacked.is_empty() {
            s.push_str("  (empty)\n");
        } else {
            for item in unacked {
                s.push_str(&format!("  [{}] {}\n", item.id, item.text));
            }
        }
        s
    }
}

/// A single retrieved snippet from an `Augmenter`.
#[derive(Clone, Debug, Serialize)]
pub struct Snippet {
    pub source: String,
    pub text: String,
}

/// Retrieved snippets from all augmenters, combined into one prompt section.
#[derive(Clone, Debug, Default)]
pub struct Augmentation(Vec<Snippet>);

impl Augmentation {
    /// Combines augmenter output into one section. Called by the loop
    /// unit after running all `Augmenter`s for a step.
    #[allow(dead_code)]
    pub(crate) fn from_snippets(snippets: Vec<Snippet>) -> Self {
        Augmentation(snippets)
    }

    pub fn snippets(&self) -> &[Snippet] {
        &self.0
    }
}

impl sealed::Sealed for Augmentation {}
impl Promptable for Augmentation {
    fn render(&self) -> String {
        if self.0.is_empty() {
            return "(none)".to_string();
        }
        self.0
            .iter()
            .map(|s| format!("[{}]\n{}", s.source, s.text))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Capabilities;
    use crate::promptable::ToolOutputFull;
    use crate::tokenizer::HeuristicTokenizer;
    use crate::version::HarnessState;

    fn caps() -> Capabilities {
        Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        }
    }

    fn budgets(observation: usize) -> Budgets {
        Budgets {
            total: 1_000_000,
            observation,
            augmentation: 1_000_000,
            notes: 1_000_000,
            working_set: 1_000_000,
        }
    }

    #[test]
    fn observation_over_budget_reports_observation_section() {
        let tok = HeuristicTokenizer;
        let sys = SystemPrompt::for_harness(&caps(), "replace", "replace semantics", None, &[]);
        let spec = Arc::new(SkillSpec::new("toy", "instructions"));
        let harness_state = HarnessState::default();
        let harness_view = HarnessView::new(&harness_state);
        let agent = serde_json::json!({});
        let agent_state = AgentState::new(&agent);

        // A tool output far larger than the observation budget, truncated
        // only against a much larger cap so it still exceeds the prompt's
        // observation budget of 20 tokens.
        let full = ToolOutputFull::new("x".repeat(10_000));
        let obs = Observation::truncate(&full, 5_000, &tok);

        let result = PromptBuilder::new(&tok, budgets(20), Version(0))
            .system(&sys)
            .spec(&spec)
            .agent(&agent_state)
            .harness(&harness_view)
            .observation(&obs)
            .build();

        let err = result.expect_err("expected OverBudget");
        assert_eq!(err.section, Section::Observation);
    }

    /// Regression for the live "prompt section Observation over budget"
    /// failure: a huge tool output, truncated at exactly the configured
    /// observation budget via `Observation::truncate`, must build
    /// successfully through the real `PromptBuilder` -- for several budget
    /// sizes, with the real `TiktokenTokenizer` (the tokenizer that
    /// surfaced the bug; `HeuristicTokenizer`'s char/4 estimate can't
    /// exhibit the BPE-merge-across-boundary overshoot).
    #[test]
    fn huge_observation_truncated_at_budget_builds_successfully() {
        use crate::tokenizer::TiktokenTokenizer;

        let tok = TiktokenTokenizer;
        for budget in [100usize, 500, 2000] {
            let sys = SystemPrompt::for_harness(&caps(), "replace", "replace semantics", None, &[]);
            let spec = Arc::new(SkillSpec::new("toy", "instructions"));
            let harness_state = HarnessState::default();
            let harness_view = HarnessView::new(&harness_state);
            let agent = serde_json::json!({});
            let agent_state = AgentState::new(&agent);

            let full = ToolOutputFull::new("x".repeat(100_000));
            let obs = Observation::truncate(&full, budget, &tok);

            let prompt = PromptBuilder::new(&tok, budgets(budget), Version(0))
                .system(&sys)
                .spec(&spec)
                .agent(&agent_state)
                .harness(&harness_view)
                .observation(&obs)
                .build();

            assert!(
                prompt.is_ok(),
                "budget={budget} expected build() to succeed, got {:?}",
                prompt.err()
            );
        }
    }
}
