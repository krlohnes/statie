//! Bounded prompt construction (A5, B1) and the promptable section types
//! that aren't defined elsewhere (`SystemPrompt`, `ToolList`, `AgentState`,
//! `HarnessView`).

use std::sync::Arc;

use serde::Serialize;

use crate::model::Capabilities;
use crate::promptable::{Observation, Promptable, sealed};
use crate::skill::SkillSpec;
use crate::tokenizer::{Budgets, Tokenizer};
use crate::tool::ToolList;
use crate::version::{HarnessState, Version};
use crate::model::Schema;

/// One of the seven fixed prompt sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Section {
    System,
    Spec,
    Tools,
    AgentState,
    HarnessView,
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
            Section::HarnessView => "Harness View",
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
/// AgentState, HarnessView, Augmentation, Observation. A section that was
/// never supplied is simply omitted.
pub struct PromptBuilder<'t> {
    tokenizer: &'t dyn Tokenizer,
    budgets: Budgets,
    version: Version,
    rendered: [Option<String>; 7],
}

const IDX_SYSTEM: usize = 0;
const IDX_SPEC: usize = 1;
const IDX_TOOLS: usize = 2;
const IDX_AGENT: usize = 3;
const IDX_HARNESS: usize = 4;
const IDX_AUGMENTATION: usize = 5;
const IDX_OBSERVATION: usize = 6;

const SECTION_ORDER: [Section; 7] = [
    Section::System,
    Section::Spec,
    Section::Tools,
    Section::AgentState,
    Section::HarnessView,
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

    pub fn harness(self, h: &HarnessView<'_>) -> Self {
        let text = h.render();
        self.set(IDX_HARNESS, text)
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

        // Headed text per slot, indexed the same way as `self.rendered`.
        let mut headed: [Option<String>; 7] = Default::default();
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
        let user: String = [IDX_AGENT, IDX_HARNESS, IDX_AUGMENTATION, IDX_OBSERVATION]
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
        s.push_str(
            "The `action` field is one of: `tool` (call a named tool with `args`), \
             `ask_user` (park and ask the user a question), `yield` (park and wait for \
             the user), or `done` (end the activation). To call a tool, set \
             action.type to \"tool\" with the tool's `name` and its `args` matching that \
             tool's schema from the Tools section.\n\n",
        );
        s.push_str(
            "Exact shapes (copy these; `action` is always an object with a `type` key):\n\
             {\"patch\": {...}, \"action\": {\"type\": \"tool\", \"name\": \"list_files\", \"args\": {\"path\": \".\"}}, \"ack_inbox\": [1]}\n\
             {\"patch\": {...}, \"action\": {\"type\": \"ask_user\", \"message\": \"Which file?\"}, \"ack_inbox\": []}\n\
             {\"patch\": {...}, \"action\": {\"type\": \"yield\"}, \"ack_inbox\": []}\n\
             {\"patch\": {...}, \"action\": {\"type\": \"done\"}, \"ack_inbox\": []}\n\
             `done` means this activation is complete; to keep working, call a tool.\n\n",
        );
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

/// Renders the unacked inbox, step counter, activation name, and budgets.
pub struct HarnessView<'a>(&'a HarnessState);

impl<'a> HarnessView<'a> {
    pub fn new(state: &'a HarnessState) -> Self {
        HarnessView(state)
    }
}

impl<'a> sealed::Sealed for HarnessView<'a> {}
impl<'a> Promptable for HarnessView<'a> {
    fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("activation: {}\n", self.0.activation));
        s.push_str(&format!("step: {}\n", self.0.step));
        s.push_str(&format!(
            "budgets: total={} observation={} augmentation={}\n",
            self.0.budgets.total, self.0.budgets.observation, self.0.budgets.augmentation
        ));
        s.push_str("inbox:\n");
        let unacked: Vec<_> = self.0.inbox.iter().filter(|i| !i.acked).collect();
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
        }
    }

    #[test]
    fn observation_over_budget_reports_observation_section() {
        let tok = HeuristicTokenizer;
        let sys = SystemPrompt::for_harness(&caps(), "replace", "replace semantics", None);
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
}
