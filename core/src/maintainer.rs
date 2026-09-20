//! Context maintenance (statie-spec.md "9. Context maintenance"): a
//! separate model call that distils a committed step's reasoning into a
//! bounded, harness-owned `notes` field, so the worker's dropped reasoning
//! trace doesn't take a decision with it.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema as SchemarsSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Action, Schema};
use crate::prompt::{BoundedPrompt, OverBudget, SystemPrompt};
use crate::promptable::Promptable;
use crate::skill::Commitment;
use crate::tokenizer::{TokenBudget, Tokenizer};
use crate::version::Version;

/// Everything the maintainer sees about one committed step: the reasoning
/// text (if any), the raw patch the worker sent, the action taken, the
/// observation text the *next* step will see, the committed agent state,
/// the current commitment (if any), the previous notes, and the token
/// budget `notes` must fit.
pub struct MaintainInput<'a> {
    pub reasoning: Option<&'a str>,
    pub patch: &'a Value,
    pub action: &'a Action,
    pub observation: &'a str,
    pub agent: &'a Value,
    pub commitment: Option<&'a Commitment>,
    pub notes: &'a str,
    pub budget: TokenBudget,
    /// The harness's active tool names for the current activation
    /// (`cur.harness.active_tools`). `next.tool` must be one of these --
    /// see `SystemPrompt::for_maintainer` and `run_maintainer`'s
    /// inactive-tool check.
    pub active_tools: &'a [String],
}

/// The maintainer's output: the new notes text, and optionally a
/// commitment the worker decided on but did not write to `next`.
#[derive(Clone, Debug)]
pub struct MaintainOutput {
    pub notes: String,
    pub next: Option<Commitment>,
}

/// An owned copy of one `MaintainInput`, for a call spawned off the
/// critical path (unit 43: the maintainer runs asynchronously, so its input
/// must outlive the borrows `Active::step` holds -- `tokio::spawn` requires
/// `'static`). Built once, right after the step that produced it commits
/// (`Active::spawn_maintainer`), exactly the same values `MaintainInput`
/// would have borrowed at that point -- spec item 5: no change to what the
/// maintainer is given. Never mutated after construction; `as_input`
/// borrows it back into a `MaintainInput` for the actual call.
pub struct MaintainInputOwned {
    pub reasoning: Option<String>,
    pub patch: Value,
    pub action: Action,
    pub observation: String,
    pub agent: Value,
    pub commitment: Option<Commitment>,
    pub notes: String,
    /// `TokenBudget`'s inner value: the type itself carries no `Clone`/`Copy`
    /// (see `tokenizer.rs`), so this holds the same `usize` and re-wraps it
    /// in `as_input`.
    pub budget: usize,
    pub active_tools: Vec<String>,
}

impl MaintainInputOwned {
    pub fn as_input(&self) -> MaintainInput<'_> {
        MaintainInput {
            reasoning: self.reasoning.as_deref(),
            patch: &self.patch,
            action: &self.action,
            observation: &self.observation,
            agent: &self.agent,
            commitment: self.commitment.as_ref(),
            notes: &self.notes,
            budget: TokenBudget(self.budget),
            active_tools: &self.active_tools,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MaintainError {
    #[error("maintainer backend error: {0}")]
    Backend(String),
    #[error("maintainer returned unparseable output: {0}")]
    Parse(String),
    #[error("maintainer notes over budget: {tokens} > {budget}")]
    OverBudget { tokens: usize, budget: usize },
    #[error("maintainer call timed out")]
    Timeout,
}

/// Drives the context-maintenance call. Mirrors `Model::step`'s shape so
/// `Arc<dyn Maintainer>` works the same way `Arc<dyn Model>` does.
#[async_trait]
pub trait Maintainer: Send + Sync {
    async fn maintain(&self, input: MaintainInput<'_>) -> Result<MaintainOutput, MaintainError>;
}

/// An owned snapshot of one `MaintainInput`, recorded by `MockMaintainer`
/// for test assertions.
#[derive(Clone, Debug)]
pub struct MaintainCall {
    pub reasoning: Option<String>,
    pub patch: Value,
    pub action: Action,
    pub observation: String,
    pub agent: Value,
    pub commitment: Option<Commitment>,
    pub notes: String,
    pub budget: usize,
    pub active_tools: Vec<String>,
}

/// A scripted `Maintainer` for tests: consumes a `Vec<Result<...>>` in
/// order, recording every input it received. Once the script is exhausted,
/// returns `Err(MaintainError::Backend("script exhausted"))`.
pub struct MockMaintainer {
    script: Mutex<VecDeque<Result<MaintainOutput, MaintainError>>>,
    calls: Mutex<Vec<MaintainCall>>,
}

impl MockMaintainer {
    pub fn new(script: Vec<Result<MaintainOutput, MaintainError>>) -> Self {
        MockMaintainer {
            script: Mutex::new(script.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every `MaintainInput` this maintainer has received, in call order.
    pub fn calls(&self) -> Vec<MaintainCall> {
        self.calls
            .lock()
            .expect("MockMaintainer calls poisoned")
            .clone()
    }
}

#[async_trait]
impl Maintainer for MockMaintainer {
    async fn maintain(&self, input: MaintainInput<'_>) -> Result<MaintainOutput, MaintainError> {
        let call = MaintainCall {
            reasoning: input.reasoning.map(str::to_string),
            patch: input.patch.clone(),
            action: input.action.clone(),
            observation: input.observation.to_string(),
            agent: input.agent.clone(),
            commitment: input.commitment.cloned(),
            notes: input.notes.to_string(),
            budget: input.budget.0,
            active_tools: input.active_tools.to_vec(),
        };
        self.calls
            .lock()
            .expect("MockMaintainer calls poisoned")
            .push(call);

        let mut script = self.script.lock().expect("MockMaintainer script poisoned");
        match script.pop_front() {
            Some(result) => result,
            None => Err(MaintainError::Backend("script exhausted".to_string())),
        }
    }
}

/// The maintainer's schema-constrained output shape: `notes` plus an
/// optional `next` commitment. Deserialization target for
/// `ModelMaintainer` (in `statie-models`); the schema this type would
/// derive via `#[derive(JsonSchema)]` is not what's advertised to the
/// model -- see `maintainer_output_schema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintainerReply {
    pub notes: String,
    pub next: Option<Commitment>,
}

/// A hand-built, flat `JsonSchema` for `MaintainerReply`.
///
/// `#[derive(JsonSchema)]` on `Option<Commitment>` renders `next` as
/// `anyOf: [<Commitment schema>, {"type": "null"}]` under schemars 1.x --
/// the same union-keyword shape `Action`'s manual impl in `core/src/
/// model.rs` avoids for grammar-constrained decoders. This impl instead
/// gives `next` a single flat object schema with `type: ["object",
/// "null"]` and `tool`/`path`/`what` inlined as properties, so
/// `maintainer_output_schema` (below) has no `oneOf`/`anyOf`/`allOf`
/// anywhere in the tree.
impl JsonSchema for MaintainerReply {
    fn schema_name() -> Cow<'static, str> {
        "MaintainerReply".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemarsSchema {
        let value = serde_json::json!({
            "type": "object",
            "properties": {
                "notes": {
                    "type": "string",
                    "description": "The distilled notes text, quoting only from the inputs, cut from the end to fit the budget."
                },
                "next": {
                    "type": ["object", "null"],
                    "description": "A commitment the reasoning decided on but did not write to `next`, or null.",
                    "properties": {
                        "tool": {
                            "type": "string",
                            "enum": ["edit_file", "write_file", "shell", "test"],
                            "description": "The mutating tool that will be called next: edit_file, write_file, shell, or test."
                        },
                        "path": {
                            "type": "string",
                            "description": "The file that call will touch, when it touches one."
                        },
                        "what": {
                            "type": "string",
                            "minLength": 1,
                            "description": "One line: exactly what the call will do."
                        }
                    },
                    "required": ["tool", "what"]
                }
            },
            "required": ["notes"]
        });
        SchemarsSchema::try_from(value).expect("literal is a JSON object")
    }
}

/// The JSON Schema advertised to the model for a maintainer call: `Schema::
/// of::<MaintainerReply>()`, guaranteed flat (no `oneOf`/`anyOf`/`allOf`)
/// by `MaintainerReply`'s manual `JsonSchema` impl above.
pub fn maintainer_output_schema() -> Schema {
    Schema::of::<MaintainerReply>()
}

/// Truncates `text` to at most `budget_tokens` tokens via head+tail
/// truncation, replacing the cut middle with a `[... N chars omitted
/// ...]` marker, where `N` is the number of *characters* removed. Returns
/// `text` unchanged (as `Cow::Borrowed`) when it already fits. Unlike
/// `Observation::truncate`, this is a simple, marker-inclusive best-effort
/// cut used only to make an oversized `reasoning`/`observation` block fit
/// `total_budget` in the maintainer's own prompt -- it is not required to
/// never overshoot by a token or two, since the caller checks the whole
/// prompt against `total_budget` afterward via `Tokenizer::count`.
fn truncate_block<'a>(text: &'a str, budget_tokens: usize, tok: &dyn Tokenizer) -> Cow<'a, str> {
    if tok.count(text) <= budget_tokens {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || budget_tokens == 0 {
        return Cow::Borrowed("");
    }

    // Reserve room for the marker itself; try progressively shorter
    // head/tail splits until the whole thing fits `budget_tokens`.
    let head_frac = 0.6;
    let mut keep = chars.len();
    loop {
        let head_len = ((keep as f64) * head_frac).floor() as usize;
        let tail_len = keep - head_len;
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        let omitted = chars.len() - head_len - tail_len;
        let marker = format!("[... {omitted} chars omitted ...]");
        let candidate = format!("{head}{marker}{tail}");
        if tok.count(&candidate) <= budget_tokens || keep == 0 {
            return Cow::Owned(candidate);
        }
        // Shrink by 10% of the original length each round (at least one
        // char), so this always terminates.
        let step = (chars.len() / 10).max(1);
        keep = keep.saturating_sub(step);
    }
}

/// Builds the two-part prompt for a context-maintenance call
/// (statie-spec.md "9. Context maintenance"): a fixed system prompt
/// (`SystemPrompt::for_maintainer`) and a single user message with
/// labelled blocks, in order: `previous notes:`, `plan/state (agent):`,
/// `open commitment:`, `active tools:`, `patch sent:`, `action:`,
/// `reasoning:`, `observation:`, and (when given) a trailing `retry_note`.
///
/// `reasoning` and `observation` are truncated (head+tail, via
/// `truncate_block`) as needed so the whole prompt fits `total_budget`;
/// `notes` and `agent` are never truncated.
pub fn build_maintainer_prompt(
    tokenizer: &dyn Tokenizer,
    input: &MaintainInput<'_>,
    total_budget: usize,
    retry_note: Option<&str>,
) -> Result<BoundedPrompt, OverBudget> {
    let system = SystemPrompt::for_maintainer(input.budget.0);

    let agent_json = serde_json::to_string_pretty(input.agent).unwrap_or_else(|_| "{}".to_string());
    let commitment_json = match input.commitment {
        Some(c) => serde_json::to_string(c).unwrap_or_else(|_| "null".to_string()),
        None => "none".to_string(),
    };
    let patch_json = serde_json::to_string(input.patch).unwrap_or_else(|_| "{}".to_string());
    let action_json = serde_json::to_string(input.action).unwrap_or_else(|_| "{}".to_string());
    let reasoning_text = input.reasoning.unwrap_or("(none)");
    let active_tools_text = input.active_tools.join(", ");

    // Everything except `reasoning`/`observation` is fixed size for this
    // call; measure it once so we know how much of `total_budget` is left
    // for the two truncatable blocks, then split what's left between them
    // in proportion to their own sizes (never truncating below 0).
    let fixed_len_estimate = |reasoning: &str, observation: &str| -> String {
        let mut s = String::new();
        s.push_str("previous notes:\n");
        s.push_str(input.notes);
        s.push_str("\n\nplan/state (agent):\n");
        s.push_str(&agent_json);
        s.push_str("\n\nopen commitment:\n");
        s.push_str(&commitment_json);
        s.push_str("\n\nactive tools: ");
        s.push_str(&active_tools_text);
        s.push_str("\n\npatch sent:\n");
        s.push_str(&patch_json);
        s.push_str("\n\naction:\n");
        s.push_str(&action_json);
        s.push_str("\n\nreasoning:\n");
        s.push_str(reasoning);
        s.push_str("\n\nobservation:\n");
        s.push_str(observation);
        if let Some(note) = retry_note {
            s.push_str("\n\n");
            s.push_str(note);
        }
        s
    };

    // Fast path: everything fits untruncated.
    let untruncated = fixed_len_estimate(reasoning_text, input.observation);
    let system_tokens = tokenizer.count(&system.render());
    let user_tokens = tokenizer.count(&untruncated);
    if system_tokens + user_tokens <= total_budget {
        return Ok(BoundedPrompt::two_part(
            &system,
            untruncated,
            system_tokens + user_tokens,
            Version(0),
        ));
    }

    // Over budget: truncate `reasoning` and `observation` down, in
    // lockstep, splitting the available budget between them in proportion
    // to their own token counts, until the whole prompt fits.
    let available = total_budget.saturating_sub(system_tokens);
    let reasoning_tokens = tokenizer.count(reasoning_text);
    let observation_tokens = tokenizer.count(input.observation);
    let total_variable = (reasoning_tokens + observation_tokens).max(1);

    let mut reasoning_budget =
        (available * reasoning_tokens / total_variable).min(reasoning_tokens);
    let mut observation_budget =
        (available * observation_tokens / total_variable).min(observation_tokens);

    loop {
        let truncated_reasoning = truncate_block(reasoning_text, reasoning_budget, tokenizer);
        let truncated_observation =
            truncate_block(input.observation, observation_budget, tokenizer);
        let candidate = fixed_len_estimate(&truncated_reasoning, &truncated_observation);
        let candidate_tokens = tokenizer.count(&candidate);
        let total = system_tokens + candidate_tokens;
        if total <= total_budget {
            return Ok(BoundedPrompt::two_part(
                &system,
                candidate,
                total,
                Version(0),
            ));
        }
        if reasoning_budget == 0 && observation_budget == 0 {
            // Nothing left to cut; report the overage against the whole
            // prompt (attributed to the harness System-equivalent section,
            // since the maintainer prompt has no `Section` of its own).
            return Err(OverBudget {
                section: crate::prompt::Section::System,
                tokens: total,
                limit: total_budget,
            });
        }
        let step_r = (reasoning_budget / 10).max(1);
        let step_o = (observation_budget / 10).max(1);
        reasoning_budget = reasoning_budget.saturating_sub(step_r);
        observation_budget = observation_budget.saturating_sub(step_o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Action;
    use crate::tokenizer::HeuristicTokenizer;

    #[allow(clippy::too_many_arguments)]
    fn input<'a>(
        reasoning: Option<&'a str>,
        patch: &'a Value,
        action: &'a Action,
        observation: &'a str,
        agent: &'a Value,
        commitment: Option<&'a Commitment>,
        notes: &'a str,
        budget: usize,
    ) -> MaintainInput<'a> {
        input_with_tools(
            reasoning,
            patch,
            action,
            observation,
            agent,
            commitment,
            notes,
            budget,
            &[],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn input_with_tools<'a>(
        reasoning: Option<&'a str>,
        patch: &'a Value,
        action: &'a Action,
        observation: &'a str,
        agent: &'a Value,
        commitment: Option<&'a Commitment>,
        notes: &'a str,
        budget: usize,
        active_tools: &'a [String],
    ) -> MaintainInput<'a> {
        MaintainInput {
            reasoning,
            patch,
            action,
            observation,
            agent,
            commitment,
            notes,
            budget: TokenBudget(budget),
            active_tools,
        }
    }

    fn tool_action() -> Action {
        Action::Tool {
            name: "edit_file".to_string(),
            args: serde_json::json!({"path": "a.rs"}),
        }
    }

    #[test]
    fn block_order_matches_spec() {
        let tok = HeuristicTokenizer;
        let patch = serde_json::json!({"notes": "n"});
        let action = tool_action();
        let agent = serde_json::json!({"state": "editing"});
        let commitment = Commitment {
            tool: "edit_file".to_string(),
            path: Some("a.rs".to_string()),
            what: "fix the bug".to_string(),
        };
        let active_tools = vec!["edit_file".to_string(), "write_file".to_string()];
        let inp = input_with_tools(
            Some("my reasoning"),
            &patch,
            &action,
            "the observation text",
            &agent,
            Some(&commitment),
            "previous notes text",
            400,
            &active_tools,
        );

        let prompt = build_maintainer_prompt(&tok, &inp, 100_000, None).expect("should build");
        let user = prompt.user();

        let idx_notes = user.find("previous notes:").expect("previous notes label");
        let idx_agent = user.find("plan/state (agent):").expect("plan/state label");
        let idx_commitment = user.find("open commitment:").expect("commitment label");
        let idx_active_tools = user.find("active tools:").expect("active tools label");
        let idx_patch = user.find("patch sent:").expect("patch label");
        let idx_action = user.find("action:").expect("action label");
        let idx_reasoning = user.find("reasoning:").expect("reasoning label");
        let idx_observation = user.find("observation:").expect("observation label");

        assert!(idx_notes < idx_agent);
        assert!(idx_agent < idx_commitment);
        assert!(idx_commitment < idx_active_tools);
        assert!(idx_active_tools < idx_patch);
        assert!(idx_patch < idx_action);
        assert!(idx_action < idx_reasoning);
        assert!(idx_reasoning < idx_observation);

        assert!(user.contains("previous notes text"));
        assert!(user.contains("my reasoning"));
        assert!(user.contains("the observation text"));
        assert!(user.contains("\"fix the bug\""));
        assert!(user.contains("active tools: edit_file, write_file"));
    }

    #[test]
    fn no_commitment_renders_none() {
        let tok = HeuristicTokenizer;
        let patch = serde_json::json!({});
        let action = tool_action();
        let agent = serde_json::json!({});
        let inp = input(None, &patch, &action, "obs", &agent, None, "notes", 400);

        let prompt = build_maintainer_prompt(&tok, &inp, 100_000, None).expect("should build");
        assert!(prompt.user().contains("open commitment:\nnone"));
        assert!(prompt.user().contains("reasoning:\n(none)"));
    }

    #[test]
    fn truncation_marker_present_only_when_needed() {
        let tok = HeuristicTokenizer;
        let patch = serde_json::json!({});
        let action = tool_action();
        let agent = serde_json::json!({});

        // Short blocks: total budget is generous, no marker anywhere.
        let short_inp = input(
            Some("short reasoning"),
            &patch,
            &action,
            "short observation",
            &agent,
            None,
            "notes",
            400,
        );
        let short_prompt =
            build_maintainer_prompt(&tok, &short_inp, 100_000, None).expect("should build");
        assert!(!short_prompt.user().contains("chars omitted"));

        // Huge reasoning/observation against a tight total budget: the
        // marker must appear.
        let long_reasoning = "R".repeat(20_000);
        let long_observation = "O".repeat(20_000);
        let long_inp = input(
            Some(&long_reasoning),
            &patch,
            &action,
            &long_observation,
            &agent,
            None,
            "notes",
            400,
        );
        let long_prompt =
            build_maintainer_prompt(&tok, &long_inp, 600, None).expect("should build");
        assert!(long_prompt.user().contains("chars omitted"));
        assert!(tok.count(long_prompt.user()) + tok.count(long_prompt.system()) <= 600);
    }

    #[test]
    fn retry_note_appended_last() {
        let tok = HeuristicTokenizer;
        let patch = serde_json::json!({});
        let action = tool_action();
        let agent = serde_json::json!({});
        let inp = input(
            Some("reasoning"),
            &patch,
            &action,
            "observation",
            &agent,
            None,
            "notes",
            400,
        );

        let without = build_maintainer_prompt(&tok, &inp, 100_000, None).expect("should build");
        assert!(!without.user().contains("cap is"));

        let note = "your previous notes were 500 tokens; the cap is 400. Cut from the end.";
        let with = build_maintainer_prompt(&tok, &inp, 100_000, Some(note)).expect("should build");
        assert!(with.user().ends_with(note));
        assert!(with.user().find(note).unwrap() > with.user().find("observation:").unwrap());
    }

    #[test]
    fn maintainer_output_schema_has_no_union_keywords() {
        fn walk(value: &Value, path: &str) {
            match value {
                Value::Object(map) => {
                    for bad in ["oneOf", "anyOf", "allOf"] {
                        assert!(!map.contains_key(bad), "found `{bad}` at {path}: {value}");
                    }
                    for (k, v) in map {
                        walk(v, &format!("{path}.{k}"));
                    }
                }
                Value::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        walk(v, &format!("{path}[{i}]"));
                    }
                }
                _ => {}
            }
        }

        let schema = maintainer_output_schema();
        walk(schema.as_value(), "$");
    }

    #[test]
    fn maintainer_output_schema_shape() {
        let schema = maintainer_output_schema();
        let value = schema.as_value();

        let props = value
            .get("properties")
            .expect("schema should have properties")
            .as_object()
            .expect("properties should be an object");
        assert!(props.contains_key("notes"));
        assert!(props.contains_key("next"));

        let next = &props["next"];
        let ty = next.get("type").expect("next should have a type");
        let ty_values: Vec<&str> = ty
            .as_array()
            .expect("next.type should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ty_values, vec!["object", "null"]);

        let next_props = next
            .get("properties")
            .and_then(Value::as_object)
            .expect("next should have properties");
        assert!(next_props.contains_key("tool"));
        assert!(next_props.contains_key("path"));
        assert!(next_props.contains_key("what"));

        let tool_enum: Vec<&str> = next_props["tool"]
            .get("enum")
            .and_then(Value::as_array)
            .expect("next.tool should have an enum")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            tool_enum,
            vec!["edit_file", "write_file", "shell", "test"],
            "next.tool's enum must match MUTATING_TOOLS"
        );

        assert_eq!(
            next_props["what"].get("minLength").and_then(Value::as_u64),
            Some(1),
            "next.what must require at least one character"
        );

        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("schema should have required");
        assert_eq!(required, &vec![Value::String("notes".to_string())]);
    }

    #[test]
    fn maintainer_output_schema_validates_examples() {
        let schema = maintainer_output_schema();
        let validator = jsonschema::validator_for(schema.as_value())
            .expect("generated schema should compile as a JSON Schema");

        let with_next = serde_json::json!({
            "notes": "kept the anchor text",
            "next": {"tool": "edit_file", "path": "a.rs", "what": "delete lines 10-12"}
        });
        assert!(validator.is_valid(&with_next), "{with_next}");

        let null_next = serde_json::json!({"notes": "kept the anchor text", "next": null});
        assert!(validator.is_valid(&null_next), "{null_next}");

        let no_notes = serde_json::json!({"next": null});
        assert!(!validator.is_valid(&no_notes), "{no_notes}");
    }
}
