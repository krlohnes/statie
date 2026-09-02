//! [`Replace`]: the model sends a complete replacement state each step.

use std::marker::PhantomData;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use statie_core::{PatchError, PatchStrategy};

/// Whole-state replacement strategy. `Patch = S`: the model returns a
/// complete workspace object, which becomes the next state verbatim.
pub struct Replace<S>(PhantomData<S>);

impl<S> PatchStrategy<S> for Replace<S>
where
    S: Serialize + DeserializeOwned + JsonSchema + Clone + Send + Sync + 'static,
{
    type Patch = S;

    fn name() -> &'static str {
        "replace"
    }

    fn describe() -> &'static str {
        "Send the complete workspace object as the patch every step. The \
         value you send becomes the new state exactly as written; there is \
         no merging. Omitting a field is not supported by the schema -- \
         every field must be present."
    }

    fn apply(_cur: &S, patch: &Self::Patch) -> Result<S, PatchError> {
        Ok(patch.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::Ws;

    #[test]
    fn apply_returns_patch_cloned() {
        let cur = Ws::sample();
        let mut next = cur.clone();
        next.count = 99;
        let applied = Replace::<Ws>::apply(&cur, &next).unwrap();
        assert_eq!(applied, next);
    }

    #[test]
    fn schema_has_required() {
        let schema = statie_core::Schema::of::<<Replace<Ws> as PatchStrategy<Ws>>::Patch>();
        let value = schema.as_value();
        assert!(
            value.get("required").is_some(),
            "Replace patch schema should require all fields, got {value}"
        );
    }
}
