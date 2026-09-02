//! Shared helpers.

use statie_core::PatchError;

/// Deserialize `doc` into `S`, naming the offending field path on failure so
/// the model sees *which* field was wrong (spec §8: rejections must be
/// actionable).
pub(crate) fn deserialize_with_path<S: serde::de::DeserializeOwned>(
    doc: serde_json::Value,
) -> Result<S, PatchError> {
    serde_path_to_error::deserialize(doc).map_err(|e| {
        let path = e.path().to_string();
        let inner = e.into_inner();
        if path.is_empty() || path == "." {
            PatchError::Malformed(inner.to_string())
        } else {
            PatchError::Malformed(format!("field `{path}`: {inner}"))
        }
    })
}
