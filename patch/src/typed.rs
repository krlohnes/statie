//! [`Typed`]: a list of strongly-typed, skill-defined operations.

use std::marker::PhantomData;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use statie_core::{PatchError, PatchStrategy};

/// A single strongly-typed operation on workspace `S`.
///
/// Skills define their own `Op` enum/struct implementing this trait; the
/// model is constrained to `Vec<Op>` and each op is applied in order to a
/// clone of the current state.
pub trait TypedOp<S>: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static {
    fn apply(&self, s: &mut S) -> Result<(), PatchError>;
}

/// Typed-operation patch strategy. `Patch = Vec<Op>`; ops apply in order on
/// a clone of the current state. The first error aborts and is returned;
/// nothing partially applies to the returned state (the clone is discarded
/// on error).
pub struct Typed<S, Op>(PhantomData<(S, Op)>);

impl<S, Op> PatchStrategy<S> for Typed<S, Op>
where
    S: Serialize + DeserializeOwned + JsonSchema + Clone + Send + Sync + 'static,
    Op: TypedOp<S>,
{
    type Patch = Vec<Op>;

    fn name() -> &'static str {
        "typed"
    }

    fn describe() -> &'static str {
        "Send a list of typed operations, applied in order to the current \
         state. Each operation is validated and applied on its own terms; \
         if any operation fails, the whole patch is rejected and none of it \
         applies."
    }

    fn apply(cur: &S, patch: &Self::Patch) -> Result<S, PatchError> {
        let mut next = cur.clone();
        for op in patch {
            op.apply(&mut next)?;
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::Ws;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "op")]
    enum WsOp {
        SetCount { value: u64 },
        SetName { value: String },
        Fail,
    }

    impl TypedOp<Ws> for WsOp {
        fn apply(&self, s: &mut Ws) -> Result<(), PatchError> {
            match self {
                WsOp::SetCount { value } => {
                    s.count = *value;
                    Ok(())
                }
                WsOp::SetName { value } => {
                    s.name = value.clone();
                    Ok(())
                }
                WsOp::Fail => Err(PatchError::ApplyFailed("intentional failure".to_string())),
            }
        }
    }

    #[test]
    fn ops_apply_in_order() {
        let cur = Ws::sample();
        let ops = vec![
            WsOp::SetCount { value: 10 },
            WsOp::SetName {
                value: "bob".to_string(),
            },
        ];
        let next = Typed::<Ws, WsOp>::apply(&cur, &ops).unwrap();
        assert_eq!(next.count, 10);
        assert_eq!(next.name, "bob");
    }

    #[test]
    fn second_op_failing_does_not_partially_apply() {
        let cur = Ws::sample();
        let ops = vec![WsOp::SetCount { value: 10 }, WsOp::Fail];
        let err = Typed::<Ws, WsOp>::apply(&cur, &ops).unwrap_err();
        assert!(matches!(err, PatchError::ApplyFailed(_)));
        // `apply` returns Result<S, _>, not a mutation of `cur`: the
        // original is untouched regardless of the error.
        assert_eq!(cur, Ws::sample());
    }

    #[test]
    fn round_trip_parses_valid_output() {
        use statie_core::{Action, RawOutput, Version};
        let content = json!({
            "patch": [
                {"op": "SetCount", "value": 3}
            ],
            "action": {"type": "yield"},
            "ack_inbox": []
        })
        .to_string();
        let raw = RawOutput::new(Version(3), None, content);
        let parsed =
            statie_core::ParsedOutput::<Ws>::parse::<Typed<Ws, WsOp>>(raw).unwrap();
        assert_eq!(parsed.version(), Version(3));
        assert!(matches!(parsed.action(), Action::Yield));
    }
}
