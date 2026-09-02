//! [`JsonPatch`] (RFC 6902): a list of JSON Patch operations.

use std::marker::PhantomData;

use json_patch::PatchOperation;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::Serialize;
use statie_core::{PatchError, PatchStrategy};

use crate::util::deserialize_with_path;

/// JSON Patch (RFC 6902) strategy: the model sends a list of `add` /
/// `remove` / `replace` / `move` / `copy` / `test` operations, applied in
/// order against the current state.
///
/// `json_patch::PatchOperation` implements `JsonSchema` directly (via the
/// `json-patch` crate's `schemars` feature), so it is used as the wire type
/// without needing a hand-rolled newtype.
pub struct JsonPatch<S>(PhantomData<S>);

impl<S> PatchStrategy<S> for JsonPatch<S>
where
    S: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    type Patch = Vec<PatchOperation>;

    fn name() -> &'static str {
        "json-patch (RFC 6902)"
    }

    fn describe() -> &'static str {
        "Send a JSON Patch: a list of operations (add, remove, replace, \
         move, copy, test), each with a JSON Pointer `path` (and `value` or \
         `from` where applicable), applied in order against the current \
         state. `test` checks a value without changing anything and aborts \
         the whole patch if it does not match; use it to guard against \
         stale reads. A failing operation aborts the entire patch -- none \
         of it applies."
    }

    fn apply(cur: &S, patch: &Self::Patch) -> Result<S, PatchError> {
        let mut doc =
            serde_json::to_value(cur).map_err(|e| PatchError::ApplyFailed(e.to_string()))?;
        json_patch::patch(&mut doc, patch).map_err(|e| PatchError::ApplyFailed(e.to_string()))?;
        deserialize_with_path(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::Ws;
    use json_patch::{AddOperation, RemoveOperation, ReplaceOperation, TestOperation};
    use serde_json::json;

    #[test]
    fn add_replace_remove_succeed() {
        let cur = Ws::sample();
        let ops = vec![
            PatchOperation::Replace(ReplaceOperation {
                path: "/count".parse().unwrap(),
                value: json!(42),
            }),
            PatchOperation::Add(AddOperation {
                path: "/nested/note".parse().unwrap(),
                value: json!("added"),
            }),
        ];
        let next = JsonPatch::<Ws>::apply(&cur, &ops).unwrap();
        assert_eq!(next.count, 42);
        assert_eq!(next.nested.note, Some("added".to_string()));

        let ops2 = vec![PatchOperation::Remove(RemoveOperation {
            path: "/nested/note".parse().unwrap(),
        })];
        let next2 = JsonPatch::<Ws>::apply(&next, &ops2).unwrap();
        assert_eq!(next2.nested.note, None);
    }

    #[test]
    fn failing_test_op_is_apply_failed() {
        let cur = Ws::sample();
        let ops = vec![PatchOperation::Test(TestOperation {
            path: "/count".parse().unwrap(),
            value: json!(999),
        })];
        let err = JsonPatch::<Ws>::apply(&cur, &ops).unwrap_err();
        assert!(matches!(err, PatchError::ApplyFailed(_)), "got {err:?}");
    }

    #[test]
    fn removing_required_field_is_an_error_mentioning_the_field() {
        let cur = Ws::sample();
        let ops = vec![PatchOperation::Remove(RemoveOperation {
            path: "/name".parse().unwrap(),
        })];
        let err = JsonPatch::<Ws>::apply(&cur, &ops).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("name"),
            "error should mention the missing field `name`: {msg}"
        );
        assert!(
            matches!(err, PatchError::Malformed(_) | PatchError::ApplyFailed(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn round_trip_parses_valid_output() {
        use statie_core::{Action, RawOutput, Version};
        let content = json!({
            "patch": [
                {"op": "replace", "path": "/count", "value": 9}
            ],
            "action": {"type": "yield"},
            "ack_inbox": []
        })
        .to_string();
        let raw = RawOutput::new(Version(3), None, content);
        let parsed = statie_core::ParsedOutput::<Ws>::parse::<JsonPatch<Ws>>(raw).unwrap();
        assert_eq!(parsed.version(), Version(3));
        assert!(matches!(parsed.action(), Action::Yield));
    }
}

