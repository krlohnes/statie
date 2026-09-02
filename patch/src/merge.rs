//! [`JsonMergePatch`] (RFC 7386): the project default patch strategy.

use std::borrow::Cow;
use std::marker::PhantomData;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use statie_core::{PatchError, PatchStrategy};

use crate::util::deserialize_with_path;

/// A JSON Merge Patch (RFC 7386) for workspace `S`.
///
/// Wraps a raw [`serde_json::Value`]; its [`JsonSchema`] is `S`'s schema with
/// every `required` array stripped (so any subset of fields is valid) and
/// every subschema inlined (no `$ref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePatch<S>(pub Value, PhantomData<S>);

impl<S> MergePatch<S> {
    pub fn new(value: Value) -> Self {
        MergePatch(value, PhantomData)
    }

    pub fn value(&self) -> &Value {
        &self.0
    }
}

impl<S> Serialize for MergePatch<S> {
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, S> Deserialize<'de> for MergePatch<S> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(MergePatch(value, PhantomData))
    }
}

impl<S: JsonSchema> JsonSchema for MergePatch<S> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        format!("MergePatch_{}", S::schema_name()).into()
    }

    fn schema_id() -> Cow<'static, str> {
        format!("statie_patch::MergePatch<{}>", S::schema_id()).into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> schemars::Schema {
        let settings = SchemaSettings::draft2020_12().with(|s| {
            s.inline_subschemas = true;
        });
        let inner_generator = settings.into_generator();
        let root = inner_generator.into_root_schema_for::<S>();
        let mut value = root.to_value();

        strip_required(&mut value);
        if let Value::Object(ref mut map) = value {
            map.remove("$schema");
            map.insert(
                "title".to_string(),
                Value::String(format!("MergePatch<{}>", S::schema_name())),
            );
        }

        schemars::Schema::try_from(value).expect("stripped schema is still an object")
    }
}

/// Recursively remove every `required` key found at any object level of the
/// schema value (i.e. every JSON Schema `"required": [...]` array, wherever
/// it appears -- at the root and inside nested `properties`).
fn strip_required(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("required");
            for v in map.values_mut() {
                strip_required(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                strip_required(v);
            }
        }
        _ => {}
    }
}

/// RFC 7386 merge: `patch` is merged into `doc` in place.
/// - `null` in the patch deletes the corresponding key from an object.
/// - an object in the patch merges recursively into an object in `doc`.
/// - anything else in the patch replaces the value in `doc` wholesale.
fn merge(doc: &mut Value, patch: &Value) {
    if let Value::Object(patch_map) = patch {
        if !doc.is_object() {
            *doc = Value::Object(serde_json::Map::new());
        }
        let doc_map = doc.as_object_mut().expect("just ensured object");
        for (k, v) in patch_map {
            if v.is_null() {
                doc_map.remove(k);
            } else {
                let entry = doc_map.entry(k.clone()).or_insert(Value::Null);
                merge(entry, v);
            }
        }
    } else {
        *doc = patch.clone();
    }
}

/// JSON Merge Patch (RFC 7386) strategy. The project default.
pub struct JsonMergePatch<S>(PhantomData<S>);

impl<S> PatchStrategy<S> for JsonMergePatch<S>
where
    S: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    type Patch = MergePatch<S>;

    fn name() -> &'static str {
        "json-merge-patch (RFC 7386)"
    }

    fn describe() -> &'static str {
        "Send only the fields you want to change, as a JSON object shaped \
         like the workspace. Fields you omit are left untouched. Nested \
         objects are merged recursively -- you can update one field of a \
         nested object without repeating the others. Setting a field to \
         null deletes it (only valid for optional fields). Arrays are \
         replaced whole: to change an array, send the entire new array, not \
         a diff of it."
    }

    fn apply(cur: &S, patch: &Self::Patch) -> Result<S, PatchError> {
        let mut doc =
            serde_json::to_value(cur).map_err(|e| PatchError::ApplyFailed(e.to_string()))?;
        merge(&mut doc, &patch.0);
        deserialize_with_path(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{Nested, Ws};
    use serde_json::json;
    use statie_core::{Action, Schema, StepOutput};

    fn walk_no_required_under(value: &Value, path: &str) {
        match value {
            Value::Object(map) => {
                assert!(
                    !map.contains_key("required"),
                    "found `required` at {path}: {value}"
                );
                for (k, v) in map {
                    walk_no_required_under(v, &format!("{path}.{k}"));
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk_no_required_under(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn merge_patch_schema_strips_required_inside_patch_but_not_action() {
        let schema = Schema::of::<StepOutput<MergePatch<Ws>>>();
        let value = schema.as_value().clone();

        let patch_schema = value
            .pointer("/properties/patch")
            .expect("patch property should exist");

        // The patch subtree itself (produced by MergePatch<Ws>::json_schema,
        // via `inline_subschemas = true`) must have no `required` anywhere
        // and no `$ref` anywhere -- everything is inlined. `StepOutput`'s
        // own schema (derived by core's plain `schemars::schema_for!`, which
        // we don't control and must not modify) still uses `$ref` for
        // `Action`; that's outside the `patch` subtree, so it's fine.
        let patch_text = serde_json::to_string(patch_schema).unwrap();
        assert!(
            !patch_text.contains("$ref"),
            "patch subschema should have no $ref (subschemas inlined): {patch_text}"
        );
        walk_no_required_under(patch_schema, "properties.patch");

        let flag = value.pointer("/properties/patch/properties/nested/properties/flag");
        assert!(
            flag.is_some(),
            "nested.flag should be inlined under properties.patch, got {value}"
        );

        let action = value
            .pointer("/properties/action")
            .expect("action property should exist");
        // core's plain `schemars::schema_for!` (used by `Schema::of`, which
        // we don't control) refs shared subschemas into `$defs` rather than
        // inlining them, so `action` here is a `$ref`; resolve it before
        // checking that its own required/oneOf survived (only the `patch`
        // subtree had `required` stripped).
        let resolved = if let Some(reference) = action.get("$ref").and_then(Value::as_str) {
            let pointer = reference.trim_start_matches('#');
            value
                .pointer(pointer)
                .unwrap_or_else(|| panic!("$ref {reference} should resolve in {value}"))
        } else {
            action
        };
        let action_text = serde_json::to_string(resolved).unwrap();
        assert!(
            action_text.contains("required") || action_text.contains("oneOf"),
            "action schema should keep its own required/oneOf: {action_text}"
        );
    }

    #[test]
    fn merge_apply_partial_changes_only_that_field() {
        let cur = Ws::sample();
        let patch = MergePatch::<Ws>::new(json!({"count": 5}));
        let next = JsonMergePatch::<Ws>::apply(&cur, &patch).unwrap();
        assert_eq!(next.count, 5);
        assert_eq!(next.name, cur.name);
        assert_eq!(next.tags, cur.tags);
        assert_eq!(next.nested, cur.nested);
    }

    #[test]
    fn merge_apply_nested_partial_keeps_flag() {
        let cur = Ws::sample();
        let patch = MergePatch::<Ws>::new(json!({"nested": {"note": "x"}}));
        let next = JsonMergePatch::<Ws>::apply(&cur, &patch).unwrap();
        assert_eq!(next.nested.flag, cur.nested.flag);
        assert_eq!(next.nested.note, Some("x".to_string()));
    }

    #[test]
    fn merge_apply_null_deletes_optional_field() {
        let cur = Ws::sample();
        let patch = MergePatch::<Ws>::new(json!({"nested": {"note": null}}));
        let next = JsonMergePatch::<Ws>::apply(&cur, &patch).unwrap();
        assert_eq!(next.nested.note, None);
        assert_eq!(next.nested.flag, cur.nested.flag);
    }

    #[test]
    fn merge_apply_null_on_required_field_is_malformed() {
        let cur = Ws::sample();
        let patch = MergePatch::<Ws>::new(json!({"name": null}));
        let err = JsonMergePatch::<Ws>::apply(&cur, &patch).unwrap_err();
        assert!(matches!(err, PatchError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn merge_apply_array_replaced_whole() {
        let cur = Ws::sample();
        let patch = MergePatch::<Ws>::new(json!({"tags": ["a"]}));
        let next = JsonMergePatch::<Ws>::apply(&cur, &patch).unwrap();
        assert_eq!(next.tags, vec!["a".to_string()]);
    }

    #[test]
    fn round_trip_parses_valid_output() {
        use statie_core::{RawOutput, Version};
        let cur = Ws::sample();
        let _ = cur; // silence unused in case Nested import moves
        let content = json!({
            "patch": {"count": 7},
            "action": {"type": "yield"},
            "ack_inbox": []
        })
        .to_string();
        let raw = RawOutput::new(Version(3), None, content);
        let parsed = statie_core::ParsedOutput::<Ws>::parse::<JsonMergePatch<Ws>>(raw).unwrap();
        assert_eq!(parsed.version(), Version(3));
        assert!(matches!(parsed.action(), Action::Yield));
    }

    #[test]
    fn nested_type_used_directly_compiles() {
        // Keep `Nested` import exercised for clarity/documentation purposes.
        let n = Nested {
            flag: false,
            note: None,
        };
        assert!(!n.flag);
    }
}

