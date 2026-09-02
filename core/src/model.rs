//! Model output types, JSON schema, and the `Model` backend trait.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::prompt::BoundedPrompt;
use crate::promptable::Reasoning;
use crate::version::Version;

/// The action the model requests this step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Tool {
        name: String,
        #[serde(default)]
        args: Value,
    },
    AskUser {
        message: String,
    },
    Yield,
    Done,
}

/// The exact type the parser deserializes into, and the schema is derived
/// from. `P` is a `PatchStrategy::Patch`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepOutput<P> {
    pub patch: P,
    pub action: Action,
    /// Inbox ids the model has read and responded to. Yield/AskUser/Done
    /// are rejected while any inbox item is unacked.
    #[serde(default)]
    pub ack_inbox: Vec<u64>,
}

/// A JSON Schema value. The only constructor derives it from a `JsonSchema`
/// type via `schemars`, so the decoder schema and the parser target can
/// never drift apart (A10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema(Value);

impl Schema {
    pub fn of<T: JsonSchema>() -> Schema {
        // Inline every subschema: grammar-constrained local decoders handle
        // a flat schema far more reliably than `$ref`/`$defs`.
        let settings = schemars::generate::SchemaSettings::draft2020_12()
            .with(|s| s.inline_subschemas = true);
        let root = settings.into_generator().into_root_schema_for::<T>();
        let mut value = root.to_value();
        if let Value::Object(ref mut map) = value {
            map.remove("$schema");
        }
        Schema(value)
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

/// Server-reported token usage for one model call, when the backend
/// provides it. Unlike the harness's local `TiktokenTokenizer` estimate
/// (which approximates prompt size with the cl100k encoding), these counts
/// come from the backend's own tokenizer and are authoritative for that
/// model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Raw model output for a step: optional reasoning, raw content string, the
/// `Version` the prompt it answers was built from, and optional
/// server-reported `Usage`.
#[derive(Debug, Clone)]
pub struct RawOutput {
    pub reasoning: Option<Reasoning>,
    pub content: String,
    pub usage: Option<Usage>,
    version: Version,
}

impl RawOutput {
    pub fn new(prompt_version: Version, reasoning: Option<Reasoning>, content: String) -> Self {
        RawOutput {
            reasoning,
            content,
            usage: None,
            version: prompt_version,
        }
    }

    /// Attaches server-reported usage.
    pub fn with_usage(mut self, u: Usage) -> Self {
        self.usage = Some(u);
        self
    }

    pub fn version(&self) -> Version {
        self.version
    }
}

/// Backend capability flags the harness adapts its system prompt to.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Reasoning arrives in its own channel (no `<thoughts>` tag needed).
    pub native_thinking: bool,
    /// The decoder enforces the JSON schema (no need to paste it).
    pub schema_constrained: bool,
    /// Stable-first prompt ordering pays off (prefix cache).
    pub prefix_cache: bool,
}

/// Streaming sink for reasoning and content deltas. The TUI implements this.
pub trait StepSink: Send {
    fn reasoning_delta(&mut self, s: &str);
    fn content_delta(&mut self, s: &str);
}

/// A `StepSink` that discards everything. Useful for tests and headless use.
pub struct NullSink;

impl StepSink for NullSink {
    fn reasoning_delta(&mut self, _s: &str) {}
    fn content_delta(&mut self, _s: &str) {}
}

#[async_trait]
pub trait Model: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn step(
        &self,
        prompt: BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, crate::error::ModelError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_of_step_output_has_patch_and_action_and_no_dollar_schema() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let value = schema.as_value();
        assert!(value.get("$schema").is_none());
        let props = value
            .get("properties")
            .expect("schema should have properties")
            .as_object()
            .expect("properties should be an object");
        assert!(props.contains_key("patch"));
        assert!(props.contains_key("action"));
        assert!(
            !serde_json::to_string(value).unwrap().contains("$ref"),
            "schema must be fully inlined"
        );
    }
}
