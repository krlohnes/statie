//! The `delegate` executor's core-side contract (milestone 9,
//! ARCHITECTURE.md "The `delegate` action"). Core cannot depend on
//! `statie-skills` (where `ChildSkill` lives), so it defines a narrow
//! [`ChildRunner`] trait instead: something that knows how to run a named
//! configured child to completion and hand back a [`Report`]. The concrete
//! implementation (`ConfiguredChildRunner`, built from `ChildSkill`s) lives
//! in `statie-skills`.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::SessionError;
use crate::model::StepSink;
use crate::report::Report;

/// The session's cancellation primitive, re-exported under a name that
/// doesn't presuppose `tokio_util` at every call site.
pub type CancelToken = CancellationToken;

/// A single inbox-shaped item to seed a child's first turn with. Milestone 9
/// only ever seeds the parent's delegation text as the child's first
/// `Event::User`, so this is deliberately just a `String` wrapper rather
/// than a full `InboxItem` (which also carries harness-owned `id`/`acked`
/// bookkeeping the child's own session assigns itself).
#[derive(Clone, Debug)]
pub struct InboxItem {
    pub text: String,
}

/// A request to run one configured child to completion.
pub struct ChildRequest {
    /// The configured child's name (as it appears in `allowed_delegates()`
    /// and the `subagents` config stanza).
    pub child: String,
    /// The parent's delegation text, verbatim -- becomes the child's first
    /// `Event::User` and `Report::task`.
    pub task: String,
    /// The child's agent path, e.g. `"implement/ui-review#3"`, used to tag
    /// every log entry the child produces (`LogEntry.agent`).
    pub agent_path: String,
    /// The parent step this delegation was requested from, for diagnostics.
    pub parent_step: u64,
    /// Cancelled when the parent's in-flight step is cancelled; the runner
    /// must stop the child and still return a `Report` (outcome `Capped`)
    /// rather than erroring.
    pub cancel: CancelToken,
    /// The child's first turn's inbox seed -- just the task text.
    pub inbox_seed: Vec<InboxItem>,
}

/// Something that can run a named configured child to completion. Core only
/// needs to validate against [`children`](ChildRunner::children) and drive
/// [`run`](ChildRunner::run); `statie-skills` supplies the implementation
/// that actually knows about `ChildSkill`/config.
#[async_trait]
pub trait ChildRunner: Send + Sync {
    /// Names this runner can run (for validation messages and the TUI).
    fn children(&self) -> Vec<String>;

    /// Runs `req.child` to completion (`Done`, its own step cap, an
    /// unrecoverable error surfaced as `Failed`, or `req.cancel` firing,
    /// surfaced as `Capped`) and returns the resulting `Report`. Only an
    /// infrastructure failure (e.g. the child's store could not be built)
    /// returns `Err`; a child that merely fails, blocks, or gets capped
    /// still returns `Ok(Report)` with the corresponding outcome.
    async fn run(&self, req: ChildRequest, sink: &mut dyn StepSink)
    -> Result<Report, SessionError>;
}

/// Computes the JSON merge-patch value that carries just the top-level keys
/// of `next` that differ (by value) from `cur`, restricted to the keys
/// present in `next`. Used by the delegate executor to build a minimal,
/// generic patch (`{"reports": {"<child>": <report>}}`) for a
/// harness-authored absorb-report commit, so it goes through the same
/// `RawPatch -> ... -> ValidatedPatch` pipeline as a model patch instead of
/// requiring a bespoke commit path.
pub(crate) fn changed_top_level_keys(
    cur: &serde_json::Value,
    next: &serde_json::Value,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let (Some(cur_obj), Some(next_obj)) = (cur.as_object(), next.as_object()) {
        for (key, next_val) in next_obj {
            let changed = match cur_obj.get(key) {
                Some(cur_val) => cur_val != next_val,
                None => true,
            };
            if changed {
                out.insert(key.clone(), next_val.clone());
            }
        }
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changed_top_level_keys_includes_only_differing_keys() {
        let cur = json!({"a": 1, "b": 2, "c": {"x": 1}});
        let next = json!({"a": 1, "b": 3, "c": {"x": 1}, "d": 4});
        let patch = changed_top_level_keys(&cur, &next);
        assert_eq!(patch, json!({"b": 3, "d": 4}));
    }

    #[test]
    fn changed_top_level_keys_empty_when_nothing_changed() {
        let cur = json!({"a": 1});
        let next = json!({"a": 1});
        assert_eq!(changed_top_level_keys(&cur, &next), json!({}));
    }
}
