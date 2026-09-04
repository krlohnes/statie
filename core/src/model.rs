//! Model output types, JSON schema, and the `Model` backend trait.

use std::borrow::Cow;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema as SchemarsSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::prompt::BoundedPrompt;
use crate::promptable::Reasoning;
use crate::version::Version;

/// The action the model requests this step.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Delegate {
        skill: String,
        task: String,
    },
    /// Harness-internal: never offered to the model (absent from the flat
    /// `JsonSchema` enum and from `restrict_action_types`'s inputs) and
    /// never accepted as a model output (`ParsedOutput::parse_with_fixes`
    /// rejects a raw `"type": "continue"` before this variant's own `serde`
    /// tag could ever match it). Produced only by a `Skill::validate`
    /// override that wants to commit a step's patch, run no tool, and tell
    /// the model in `note` what is still open -- see `core/src/session.rs`'s
    /// handling of `Action::Continue` and
    /// `skills/src/coding/mod.rs`'s premature-`done`/empty-`ask_user`
    /// conversions. The `serde` tag (`"continue"`, from the same
    /// `rename_all = "snake_case"` derive as every other variant) exists so
    /// the harness's own `Action::Continue` values serialize/deserialize
    /// symmetrically with the rest of `Action` (e.g. through the session
    /// log), not so a model can produce one.
    Continue {
        note: String,
    },
}

/// A manual, flat `JsonSchema` for `Action`.
///
/// `#[derive(JsonSchema)]` on an internally-tagged enum produces a `oneOf`
/// over one subschema per variant. Under grammar-constrained decoding, a
/// small local model faced with that `oneOf` collapsed to the shortest
/// branches (`{"type":"yield"}` / `{"type":"done"}`) even when its own
/// reasoning said to call a tool. This impl instead emits one flat object
/// with every field from every variant, `type` constrained to an `enum`,
/// and no `oneOf`/`anyOf`/`allOf` anywhere -- the shape a grammar-constrained
/// decoder can follow without a "shortest branch" bias. `serde`'s derive
/// (internally tagged, `snake_case`) is unchanged above, so deserialization
/// -- and therefore the parser's strictness (unknown `type` values, missing
/// required fields per variant) -- is exactly as before; this only changes
/// what schema is *advertised* to the decoder/prompt.
impl JsonSchema for Action {
    fn schema_name() -> Cow<'static, str> {
        "Action".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemarsSchema {
        let value = serde_json::json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["tool", "ask_user", "delegate", "done", "yield"],
                    "description": "Which action to take. `name`, `args`, \
                        `message`, `skill`, and `task` are always required: \
                        for \"ask_user\", \"delegate\", \"done\", and \
                        \"yield\", set `name` to \"\" and `args` to {}; set \
                        `message` to \"\" except for \"ask_user\"; set \
                        `skill` and `task` to \"\" except for \"delegate\". \
                        \"tool\" uses `name` and `args` for the call. \
                        \"ask_user\" additionally requires `message`. \
                        \"delegate\" additionally requires `skill` and \
                        `task`."
                },
                "name": {
                    "type": "string",
                    "description": "tool name (type = tool); \"\" when type is not tool"
                },
                "args": {
                    "type": "object",
                    "description": "tool arguments object; `{}` when type is not tool"
                },
                "message": {
                    "type": "string",
                    "description": "the question for the user (type = ask_user); \"\" otherwise"
                },
                "skill": {
                    "type": "string",
                    "description": "skill to delegate to (type = delegate); \"\" otherwise"
                },
                "task": {
                    "type": "string",
                    "description": "the delegate's task text only -- what \
                        the delegate should do, in imperative form. Never a \
                        rationale for choosing delegate. \"\" otherwise."
                }
            },
            "required": ["type", "name", "args", "message", "skill", "task"]
        });
        SchemarsSchema::try_from(value).expect("literal is a JSON object")
    }
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
        let settings =
            schemars::generate::SchemaSettings::draft2020_12().with(|s| s.inline_subschemas = true);
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

/// Rewrites `properties.action.properties.type.enum` on `schema` to keep
/// only the types listed in `allowed`, in `allowed`'s order, and updates the
/// `type` field's description to document only the kept types' field
/// requirements (the leading sentence about `name`/`args` placeholders is
/// kept verbatim).
///
/// Grammar-constrained decoding (Ollama's `format: "schema"`) samples
/// exactly the schema it's given: a decoder offered `"yield"` in the enum
/// will occasionally emit it even when the active skill's rules will reject
/// it outright. Narrowing the enum to what the skill actually allows means
/// the grammar itself cannot produce a doomed-to-be-rejected action.
///
/// A no-op if `properties.action.properties.type` isn't present at that
/// path (e.g. a workspace schema that never went through the manual flat
/// `Action` impl). When `"delegate"` is dropped from `allowed`, the
/// `skill` and `task` properties are also removed from
/// `properties.action.properties` (they exist only to carry a `delegate`
/// action's fields). When `"ask_user"` is kept, `message` is left in place
/// unconditionally -- only `delegate`'s fields are conditionally pruned,
/// per the task.
pub fn restrict_action_types(schema: &mut Schema, allowed: &[&str]) {
    let Some(action) = schema.0.pointer_mut("/properties/action") else {
        return;
    };
    let Value::Object(action_obj) = action else {
        return;
    };
    let has_type_prop = action_obj
        .get("properties")
        .and_then(|p| p.get("type"))
        .is_some();
    if !has_type_prop {
        return;
    }

    // Drop delegate's fields from the action schema when delegate itself is
    // dropped.
    if !allowed.contains(&"delegate") {
        if let Some(Value::Object(props)) = action_obj.get_mut("properties") {
            props.remove("skill");
            props.remove("task");
        }
        if let Some(Value::Array(required)) = action_obj.get_mut("required") {
            required.retain(|v| v.as_str() != Some("skill") && v.as_str() != Some("task"));
        }
    }

    let Some(Value::Object(props)) = action_obj.get_mut("properties") else {
        return;
    };
    let Some(ty) = props.get_mut("type") else {
        return;
    };
    let Value::Object(ty_obj) = ty else {
        return;
    };

    let kept: Vec<Value> = allowed
        .iter()
        .map(|s| Value::String((*s).to_string()))
        .collect();
    ty_obj.insert("enum".to_string(), Value::Array(kept));

    let non_tool_placeholders: Vec<String> = allowed
        .iter()
        .filter(|a| **a != "tool")
        .map(|a| format!("\"{a}\""))
        .collect();
    let non_ask_user_placeholders: Vec<String> = allowed
        .iter()
        .filter(|a| **a != "ask_user")
        .map(|a| format!("\"{a}\""))
        .collect();
    let non_delegate_placeholders: Vec<String> = allowed
        .iter()
        .filter(|a| **a != "delegate")
        .map(|a| format!("\"{a}\""))
        .collect();
    let mut desc = String::from(
        "Which action to take. `name`, `args`, `message`, `skill`, and `task` are always required: ",
    );
    if !non_tool_placeholders.is_empty() {
        desc.push_str(&format!(
            "for {}, set `name` to \"\" and `args` to {{}}. ",
            join_with_and(&non_tool_placeholders)
        ));
    }
    if !non_ask_user_placeholders.is_empty() {
        desc.push_str(&format!(
            "for {}, set `message` to \"\". ",
            join_with_and(&non_ask_user_placeholders)
        ));
    }
    if !non_delegate_placeholders.is_empty() {
        desc.push_str(&format!(
            "for {}, set `skill` and `task` to \"\". ",
            join_with_and(&non_delegate_placeholders)
        ));
    }
    for a in allowed {
        match *a {
            "tool" => {
                desc.push_str("\"tool\" uses `name` and `args` for the call. ");
            }
            "ask_user" => {
                desc.push_str("\"ask_user\" additionally requires `message`. ");
            }
            "delegate" => {
                desc.push_str("\"delegate\" additionally requires `skill` and `task`. ");
            }
            "done" | "yield" => {}
            _ => {}
        }
    }
    let desc = desc.trim_end().to_string();
    ty_obj.insert("description".to_string(), Value::String(desc));
}

/// Joins `items` as `"a"`, `"a and b"`, or `"a, b, and c"` (Oxford comma) --
/// the style the original hand-written `Action` schema description used for
/// its `"ask_user", "delegate", "done", and "yield"` list.
fn join_with_and(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let (last, rest) = items.split_last().expect("len > 2");
            format!("{}, and {}", rest.join(", "), last)
        }
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
    /// Why the backend stopped generating, in the backend's own vocabulary
    /// (Ollama: `done_reason`, e.g. `"stop"`/`"length"`; OpenAI-compatible:
    /// `choices[0].finish_reason`, e.g. `"stop"`/`"length"`). `None` when
    /// the backend didn't report one.
    pub finish_reason: Option<String>,
    version: Version,
}

impl RawOutput {
    pub fn new(prompt_version: Version, reasoning: Option<Reasoning>, content: String) -> Self {
        RawOutput {
            reasoning,
            content,
            usage: None,
            finish_reason: None,
            version: prompt_version,
        }
    }

    /// Attaches server-reported usage.
    pub fn with_usage(mut self, u: Usage) -> Self {
        self.usage = Some(u);
        self
    }

    /// Attaches the backend-reported reason generation stopped.
    pub fn with_finish_reason(mut self, reason: impl Into<String>) -> Self {
        self.finish_reason = Some(reason.into());
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
///
/// The three `child_*` methods (milestone 9) let a `ChildRunner` report a
/// subagent's progress through the same sink the parent's own step uses, so
/// the TUI can render children without a second channel. All three are
/// provided no-ops: a sink that doesn't care about subagents (tests, the
/// headless summary sink) needs no changes.
pub trait StepSink: Send {
    fn reasoning_delta(&mut self, s: &str);
    fn content_delta(&mut self, s: &str);

    /// A child activation is about to start.
    fn child_started(&mut self, _agent: &str, _task: &str) {}
    /// The child committed a step. `outcome` is a short label (e.g.
    /// `"continue"`, `"done"`, `"parked"`).
    fn child_step(&mut self, _agent: &str, _step: u64, _outcome: &str) {}
    /// The child finished (successfully or not) and produced `report`.
    fn child_finished(&mut self, _agent: &str, _report: &crate::report::Report) {}

    /// A context-maintenance call (statie-spec.md "9. Context
    /// maintenance") completed successfully and its patch was applied.
    /// `notes_tokens` is the token count of the new `notes` text;
    /// `next_set` is whether a maintainer-proposed commitment was applied.
    fn maintainer_done(&mut self, _notes_tokens: usize, _next_set: bool) {}
    /// A context-maintenance call failed (backend error, over budget after
    /// retry, or its patch was rejected). The previous `notes` are kept and
    /// the step outcome is unaffected.
    fn maintainer_failed(&mut self, _reason: &str) {}
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

    /// Recursively asserts `value` contains none of `oneOf`/`anyOf`/`allOf`
    /// as an object key anywhere in its tree.
    fn walk_no_union_keywords(value: &Value, path: &str) {
        match value {
            Value::Object(map) => {
                for bad in ["oneOf", "anyOf", "allOf"] {
                    assert!(!map.contains_key(bad), "found `{bad}` at {path}: {value}");
                }
                for (k, v) in map {
                    walk_no_union_keywords(v, &format!("{path}.{k}"));
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk_no_union_keywords(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn action_schema_is_flat_no_union_keywords() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let value = schema.as_value();
        let action = value
            .pointer("/properties/action")
            .expect("action property should exist");
        walk_no_union_keywords(action, "properties.action");
        // Sanity: also check the whole document, in case the flat action
        // schema somehow ended up referenced from elsewhere.
        walk_no_union_keywords(value, "$");
    }

    #[test]
    fn action_schema_type_enum_and_required() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let value = schema.as_value();
        let action = value
            .pointer("/properties/action")
            .expect("action property should exist");

        let ty = action
            .pointer("/properties/type")
            .expect("action.properties.type should exist");
        let enum_values: Vec<&str> = ty
            .get("enum")
            .and_then(Value::as_array)
            .expect("type should have an enum")
            .iter()
            .map(|v| v.as_str().expect("enum values should be strings"))
            .collect();
        assert_eq!(
            enum_values,
            vec!["tool", "ask_user", "delegate", "done", "yield"]
        );

        let description = ty
            .get("description")
            .and_then(Value::as_str)
            .expect("type should have a description");
        assert!(description.contains("name"), "description: {description}");
        assert!(description.contains("args"), "description: {description}");

        let required = action
            .get("required")
            .and_then(Value::as_array)
            .expect("action should have required");
        assert_eq!(
            required,
            &vec![
                Value::String("type".to_string()),
                Value::String("name".to_string()),
                Value::String("args".to_string()),
                Value::String("message".to_string()),
                Value::String("skill".to_string()),
                Value::String("task".to_string()),
            ]
        );

        assert!(
            action.get("additionalProperties").is_none(),
            "additionalProperties must not be restricted"
        );
    }

    /// Every valid `Action` JSON shape, plus one invalid `type`, validates
    /// (or fails to) against the flat schema the way the parser expects.
    #[test]
    fn action_schema_validates_all_variants_and_rejects_unknown_type() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let validator = jsonschema::validator_for(schema.as_value())
            .expect("generated schema should compile as a JSON Schema");

        let tool_call = serde_json::json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": "a.rs"}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&tool_call),
            "tool call should validate: {tool_call}"
        );

        let ask_user = serde_json::json!({
            "patch": {},
            "action": {"type": "ask_user", "name": "", "args": {}, "message": "Which file?", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&ask_user),
            "ask_user should validate: {ask_user}"
        );

        let delegate = serde_json::json!({
            "patch": {},
            "action": {"type": "delegate", "name": "", "args": {}, "message": "", "skill": "ui-review", "task": "check the button"},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&delegate),
            "delegate should validate: {delegate}"
        );

        let done = serde_json::json!({
            "patch": {},
            "action": {"type": "done", "name": "", "args": {}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(validator.is_valid(&done), "done should validate: {done}");

        let yield_ = serde_json::json!({
            "patch": {},
            "action": {"type": "yield", "name": "", "args": {}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&yield_),
            "yield should validate: {yield_}"
        );

        let nonsense = serde_json::json!({
            "patch": {},
            "action": {"type": "nonsense", "name": "", "args": {}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            !validator.is_valid(&nonsense),
            "unknown type should not validate: {nonsense}"
        );
    }

    /// The exact live failure this schema change fixes: a grammar-decoded
    /// `ask_user` with no `message` at all must NOT validate now that
    /// `message` is required. With `"message": ""` present it validates
    /// against the schema (an empty placeholder is a syntactically valid
    /// string) -- `validate_default` is responsible for rejecting the empty
    /// value semantically; see `skill.rs`'s
    /// `ask_user_with_empty_message_is_rejected`.
    #[test]
    fn action_schema_rejects_ask_user_without_message_field_accepts_with_empty_string() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let validator = jsonschema::validator_for(schema.as_value())
            .expect("generated schema should compile as a JSON Schema");

        let no_message = serde_json::json!({
            "patch": {},
            "action": {"type": "ask_user", "name": "", "args": {}},
            "ack_inbox": []
        });
        assert!(
            !validator.is_valid(&no_message),
            "ask_user with no message field should NOT validate: {no_message}"
        );

        let empty_message = serde_json::json!({
            "patch": {},
            "action": {"type": "ask_user", "name": "", "args": {}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&empty_message),
            "ask_user with message=\"\" should validate against the schema: {empty_message}"
        );
    }

    /// A tool call missing `args` (the exact grammar-decoding failure this
    /// schema change fixes: the model used the optional `task` field for a
    /// rationale and omitted `args` entirely) must NOT validate now that
    /// `args` is required. A `done` action with the required-but-unused
    /// `name`/`args` filled in as the schema instructs must validate.
    #[test]
    fn action_schema_rejects_tool_without_args_and_accepts_done_with_placeholders() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let validator = jsonschema::validator_for(schema.as_value())
            .expect("generated schema should compile as a JSON Schema");

        let tool_missing_args = serde_json::json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "task": "why"},
            "ack_inbox": []
        });
        assert!(
            !validator.is_valid(&tool_missing_args),
            "tool call without args should NOT validate: {tool_missing_args}"
        );

        let done = serde_json::json!({
            "patch": {},
            "action": {"type": "done", "name": "", "args": {}, "message": "", "skill": "", "task": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&done),
            "done with name=\"\" and args={{}} should validate: {done}"
        );
    }

    /// Item 2: the internally-tagged `serde` derive on `Action` already
    /// ignores fields that don't belong to the matched variant (verified
    /// independently against a minimal reproduction), so no enum
    /// restructuring was needed to accept the extra `name`/`args` the new
    /// schema now requires on every shape, including non-tool actions.
    #[test]
    fn action_parses_non_tool_variants_with_required_name_and_args_present() {
        let done: Action =
            serde_json::from_value(serde_json::json!({"type": "done", "name": "", "args": {}}))
                .unwrap();
        assert!(matches!(done, Action::Done));

        let yield_: Action =
            serde_json::from_value(serde_json::json!({"type": "yield", "name": "", "args": {}}))
                .unwrap();
        assert!(matches!(yield_, Action::Yield));

        let ask_user: Action = serde_json::from_value(serde_json::json!({
            "type": "ask_user",
            "name": "",
            "args": {},
            "message": "Which file?"
        }))
        .unwrap();
        assert!(matches!(ask_user, Action::AskUser { message } if message == "Which file?"));

        let delegate: Action = serde_json::from_value(serde_json::json!({
            "type": "delegate",
            "name": "",
            "args": {},
            "skill": "ui-review",
            "task": "check the button"
        }))
        .unwrap();
        assert!(
            matches!(delegate, Action::Delegate { skill, task } if skill == "ui-review" && task == "check the button")
        );
    }

    /// `serde_json::from_value::<Action>` still parses all five variants:
    /// the manual `JsonSchema` impl must not have changed deserialization,
    /// since the serde derive on `Action` is unchanged.
    #[test]
    fn action_deserializes_all_five_variants() {
        let tool: Action = serde_json::from_value(
            serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": "a.rs"}}),
        )
        .unwrap();
        assert!(matches!(tool, Action::Tool { name, .. } if name == "read_file"));

        let ask_user: Action =
            serde_json::from_value(serde_json::json!({"type": "ask_user", "message": "hi?"}))
                .unwrap();
        assert!(matches!(ask_user, Action::AskUser { message } if message == "hi?"));

        let delegate: Action = serde_json::from_value(
            serde_json::json!({"type": "delegate", "skill": "ui-review", "task": "do it"}),
        )
        .unwrap();
        assert!(
            matches!(delegate, Action::Delegate { skill, task } if skill == "ui-review" && task == "do it")
        );

        let done: Action = serde_json::from_value(serde_json::json!({"type": "done"})).unwrap();
        assert!(matches!(done, Action::Done));

        let yield_: Action = serde_json::from_value(serde_json::json!({"type": "yield"})).unwrap();
        assert!(matches!(yield_, Action::Yield));

        // Unknown type is still rejected by serde (schema-time validation
        // and parse-time validation are independent; both must agree here).
        let bad = serde_json::from_value::<Action>(serde_json::json!({"type": "nonsense"}));
        assert!(bad.is_err());
    }

    /// `Action::Continue` is harness-internal (never offered to the model,
    /// never accepted as a model output -- see `ParsedOutput::
    /// parse_with_fixes`'s rejection of a raw `"type": "continue"` in
    /// `core/src/skill.rs`), but its `serde` tag still round-trips like
    /// every other variant's: serializing `Action::Continue` and
    /// deserializing the result back into `Action` recovers the same value.
    /// This is what lets it flow through the session log (`LogKind::Action`
    /// payloads are `serde_json::to_value(&action)`) the same way every
    /// other action does.
    #[test]
    fn action_continue_round_trips_through_serde() {
        let action = Action::Continue {
            note: "plan step 1 is still open".to_string(),
        };
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type": "continue", "note": "plan step 1 is still open"})
        );
        let back: Action = serde_json::from_value(value).unwrap();
        assert!(matches!(back, Action::Continue { note } if note == "plan step 1 is still open"));
    }

    /// The flat schema's `type` enum is the harness's hard boundary on what
    /// the model may ever be offered: `"continue"` must never appear in it,
    /// in either the unrestricted schema or any `restrict_action_types`
    /// output, however `allowed` is built.
    #[test]
    fn action_schema_type_enum_never_offers_continue() {
        let schema = Schema::of::<StepOutput<serde_json::Value>>();
        let enum_values = action_enum(&schema);
        assert!(
            !enum_values.iter().any(|v| v == "continue"),
            "the unrestricted schema must never offer \"continue\": {enum_values:?}"
        );

        let mut restricted = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(
            &mut restricted,
            &["tool", "ask_user", "delegate", "done", "yield"],
        );
        let restricted_values = action_enum(&restricted);
        assert!(
            !restricted_values.iter().any(|v| v == "continue"),
            "restrict_action_types must never offer \"continue\" either: {restricted_values:?}"
        );
    }

    // -----------------------------------------------------------------
    // restrict_action_types
    // -----------------------------------------------------------------

    fn action_enum(schema: &Schema) -> Vec<String> {
        schema
            .as_value()
            .pointer("/properties/action/properties/type/enum")
            .and_then(Value::as_array)
            .expect("type should have an enum")
            .iter()
            .map(|v| {
                v.as_str()
                    .expect("enum values should be strings")
                    .to_string()
            })
            .collect()
    }

    fn action_description(schema: &Schema) -> String {
        schema
            .as_value()
            .pointer("/properties/action/properties/type/description")
            .and_then(Value::as_str)
            .expect("type should have a description")
            .to_string()
    }

    #[test]
    fn restrict_action_types_keeps_only_allowed_in_original_order() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done", "ask_user"]);
        assert_eq!(action_enum(&schema), vec!["tool", "done", "ask_user"]);
    }

    #[test]
    fn restrict_action_types_description_mentions_only_kept_types() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done", "ask_user"]);
        let description = action_description(&schema);
        assert!(description.contains("ask_user"), "{description}");
        assert!(description.contains("message"), "{description}");
        assert!(description.contains("name"), "{description}");
        assert!(description.contains("args"), "{description}");
        assert!(!description.contains("yield"), "{description}");
        // "delegate" itself is dropped from the enum, but `skill`/`task`
        // still appear as placeholder instructions (set to "" for every
        // kept, non-delegate type) since those fields remain required.
        assert!(!description.contains("\"delegate\""), "{description}");
        assert!(description.contains("skill"), "{description}");
        assert!(description.contains("`task`"), "{description}");
    }

    #[test]
    fn restrict_action_types_drops_delegate_fields_when_delegate_dropped() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done"]);
        let action = schema
            .as_value()
            .pointer("/properties/action")
            .expect("action property should exist");
        assert!(
            action.pointer("/properties/skill").is_none(),
            "skill property should be removed: {action}"
        );
        assert!(
            action.pointer("/properties/task").is_none(),
            "task property should be removed: {action}"
        );

        let required: Vec<&str> = action
            .get("required")
            .and_then(Value::as_array)
            .expect("action should have required")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert!(
            !required.contains(&"skill") && !required.contains(&"task"),
            "skill/task should be removed from required when delegate is dropped: {required:?}"
        );
        assert_eq!(
            required,
            vec!["type", "name", "args", "message"],
            "required should keep type/name/args/message: {required:?}"
        );
    }

    #[test]
    fn restrict_action_types_keeps_delegate_fields_when_delegate_kept() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done", "delegate"]);
        let action = schema
            .as_value()
            .pointer("/properties/action")
            .expect("action property should exist");
        assert!(action.pointer("/properties/skill").is_some());
        assert!(action.pointer("/properties/task").is_some());
    }

    #[test]
    fn restrict_action_types_keeps_message_when_ask_user_kept() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done", "ask_user"]);
        let action = schema
            .as_value()
            .pointer("/properties/action")
            .expect("action property should exist");
        assert!(action.pointer("/properties/message").is_some());
    }

    #[test]
    fn restrict_action_types_is_noop_when_path_missing() {
        // A schema with no `properties.action.properties.type` at all (e.g.
        // a bare workspace schema, not a `StepOutput`).
        let mut schema = Schema::of::<serde_json::Value>();
        let before = schema.as_value().clone();
        restrict_action_types(&mut schema, &["tool", "done"]);
        assert_eq!(schema.as_value(), &before);
    }

    #[test]
    fn restrict_action_types_is_noop_when_no_action_property() {
        #[derive(Serialize, Deserialize, JsonSchema)]
        struct NoAction {
            foo: String,
        }
        let mut schema = Schema::of::<NoAction>();
        let before = schema.as_value().clone();
        restrict_action_types(&mut schema, &["tool", "done"]);
        assert_eq!(schema.as_value(), &before);
    }

    #[test]
    fn restrict_action_types_validation_rejects_yield_accepts_tool() {
        let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
        restrict_action_types(&mut schema, &["tool", "done"]);
        let validator = jsonschema::validator_for(schema.as_value())
            .expect("restricted schema should still compile as a JSON Schema");

        let yield_ = serde_json::json!({
            "patch": {},
            "action": {"type": "yield", "name": "", "args": {}, "message": ""},
            "ack_inbox": []
        });
        assert!(
            !validator.is_valid(&yield_),
            "yield should be rejected after restriction: {yield_}"
        );

        let tool_call = serde_json::json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": "a.rs"}, "message": ""},
            "ack_inbox": []
        });
        assert!(
            validator.is_valid(&tool_call),
            "tool call should still validate: {tool_call}"
        );
    }
}
