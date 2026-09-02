//! `InMemoryStore`: an in-process, `Mutex`-guarded [`StateStore`] impl.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

use statie_core::{
    Action, ApplyFn, CommittedState, HarnessState, LoggedPatch, StateStore, StoreError,
    ValidatedPatch, Version,
};

struct Inner<W> {
    initial: CommittedState<W>,
    current: CommittedState<W>,
    version: Version,
    log: Vec<LoggedPatch>,
}

/// An in-memory `StateStore`. State lives behind a `std::sync::Mutex` and is
/// lost when the process exits.
pub struct InMemoryStore<W> {
    inner: Mutex<Option<Inner<W>>>,
}

impl<W> InMemoryStore<W> {
    pub fn new() -> Self {
        InMemoryStore {
            inner: Mutex::new(None),
        }
    }
}

impl<W> Default for InMemoryStore<W> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<W> StateStore<W> for InMemoryStore<W>
where
    W: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError> {
        let guard = self.inner.lock().expect("store mutex poisoned");
        Ok(guard
            .as_ref()
            .map(|inner| (inner.current.clone(), inner.version)))
    }

    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().expect("store mutex poisoned");
        if guard.is_some() {
            return Err(StoreError::Backend("already initialized".to_string()));
        }
        let version = Version(0);
        *guard = Some(Inner {
            initial: initial.clone(),
            current: initial,
            version,
            log: Vec::new(),
        });
        Ok(version)
    }

    async fn commit(
        &self,
        patch: ValidatedPatch<W>,
        harness: HarnessState,
        action: &Action,
        expect: Version,
    ) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().expect("store mutex poisoned");
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("store not initialized".to_string()))?;

        if patch.version() != expect {
            return Err(StoreError::VersionMismatch {
                expected: patch.version(),
                actual: expect,
            });
        }

        if expect != inner.version {
            return Err(StoreError::VersionMismatch {
                expected: expect,
                actual: inner.version,
            });
        }

        let (raw, next, _validated_version) = patch.into_parts();
        let new_version = Version(inner.version.0 + 1);

        inner.current.agent = next;
        inner.current.harness = harness.clone();
        inner.version = new_version;
        inner.log.push(LoggedPatch {
            version: new_version,
            patch: raw,
            action: action.clone(),
            harness,
        });

        Ok(new_version)
    }

    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError> {
        let guard = self.inner.lock().expect("store mutex poisoned");
        let inner = guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("store not initialized".to_string()))?;
        Ok(inner.log.clone())
    }

    async fn replay(
        &self,
        to: Version,
        apply: ApplyFn<'_, W>,
    ) -> Result<CommittedState<W>, StoreError> {
        let guard = self.inner.lock().expect("store mutex poisoned");
        let inner = guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("store not initialized".to_string()))?;

        let mut agent = inner.initial.agent.clone();
        let mut harness = inner.initial.harness.clone();

        for entry in &inner.log {
            if entry.version > to {
                break;
            }
            agent = apply(&agent, &entry.patch).map_err(|e| StoreError::Backend(e.to_string()))?;
            harness = entry.harness.clone();
        }

        Ok(CommittedState { harness, agent })
    }

    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError> {
        let mut guard = self.inner.lock().expect("store mutex poisoned");
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("store not initialized".to_string()))?;
        inner.current.harness = harness;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use statie_core::{InboxItem, apply_fn};

    use crate::test_support::{
        ReplaceStrategy, Workspace, committed_state, harness, make_validated_patch,
    };

    #[tokio::test]
    async fn load_before_init_is_none_then_returns_initial_state() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        assert!(store.load().await.unwrap().is_none());

        let initial = committed_state(0, 0);
        let v = store.init(initial.clone()).await.unwrap();
        assert_eq!(v, Version(0));

        let (loaded, version) = store.load().await.unwrap().expect("initialized");
        assert_eq!(version, Version(0));
        assert_eq!(loaded.agent, initial.agent);
    }

    #[tokio::test]
    async fn init_twice_errors() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        store.init(committed_state(0, 0)).await.unwrap();
        let err = store.init(committed_state(0, 0)).await.unwrap_err();
        assert!(matches!(err, StoreError::Backend(msg) if msg == "already initialized"));
    }

    #[tokio::test]
    async fn valid_commit_bumps_version_and_persists_agent_and_harness() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let (patch, action) = make_validated_patch(&initial, Version(0), 42, &[]);
        let mut h = harness(1);
        h.step = 1;
        let new_version = store
            .commit(patch, h.clone(), &action, Version(0))
            .await
            .unwrap();
        assert_eq!(new_version, Version(1));

        let (loaded, version) = store.load().await.unwrap().unwrap();
        assert_eq!(version, Version(1));
        assert_eq!(loaded.agent.value, 42);
        assert_eq!(loaded.harness.step, 1);
    }

    #[tokio::test]
    async fn stale_expect_is_rejected_and_state_unchanged() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        // First commit takes the store to version 1.
        let (patch1, action1) = make_validated_patch(&initial, Version(0), 1, &[]);
        store
            .commit(patch1, harness(1), &action1, Version(0))
            .await
            .unwrap();

        let (before, before_version) = store.load().await.unwrap().unwrap();

        // Now try to commit again with expect = Version(0), which is stale.
        let cur_at_v1 = CommittedState {
            harness: harness(1),
            agent: Workspace { value: 1 },
        };
        let (patch2, action2) = make_validated_patch(&cur_at_v1, Version(0), 2, &[]);
        let err = store
            .commit(patch2, harness(2), &action2, Version(0))
            .await
            .unwrap_err();
        match err {
            StoreError::VersionMismatch { expected, actual } => {
                assert_eq!(expected, Version(0));
                assert_eq!(actual, Version(1));
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }

        let (after, after_version) = store.load().await.unwrap().unwrap();
        assert_eq!(after_version, before_version);
        assert_eq!(after.agent, before.agent);
    }

    #[tokio::test]
    async fn patch_validated_against_stale_version_is_rejected_even_with_matching_expect() {
        // The patch was validated against Version(0), but by the time it is
        // committed the store has moved to Version(1) via another commit.
        // Committing with expect = Version(1) must still fail because the
        // patch itself carries version 0 (B1 defensive check).
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        // Build a patch validated against version 0 up front.
        let (stale_patch, stale_action) = make_validated_patch(&initial, Version(0), 99, &[]);

        // Meanwhile another commit happens, bumping the store to version 1.
        let (other_patch, other_action) = make_validated_patch(&initial, Version(0), 1, &[]);
        store
            .commit(other_patch, harness(1), &other_action, Version(0))
            .await
            .unwrap();

        // Now attempt to commit the stale patch with expect = Version(1)
        // (matching the store's actual version, but not the patch's own
        // validated version).
        let err = store
            .commit(stale_patch, harness(2), &stale_action, Version(1))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn three_commits_produce_three_log_entries_with_versions_and_actions() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let mut cur = committed_state(0, 0);
        store.init(cur.clone()).await.unwrap();

        let mut version = Version(0);
        for i in 1..=3 {
            let (patch, action) = make_validated_patch(&cur, version, i, &[]);
            let mut h = harness(i as u64);
            h.step = i as u64;
            version = store
                .commit(patch, h.clone(), &action, version)
                .await
                .unwrap();
            cur = CommittedState {
                harness: h,
                agent: Workspace { value: i },
            };
        }

        let log = store.patch_log().await.unwrap();
        assert_eq!(log.len(), 3);
        for (idx, entry) in log.iter().enumerate() {
            assert_eq!(entry.version, Version((idx + 1) as u64));
            assert!(matches!(entry.action, Action::Yield));
        }
    }

    #[tokio::test]
    async fn replay_reproduces_state_at_each_version() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let mut cur = initial.clone();
        let mut version = Version(0);
        let mut captured_after_second: Option<CommittedState<Workspace>> = None;

        for i in 1..=3 {
            let (patch, action) = make_validated_patch(&cur, version, i * 10, &[]);
            let mut h = harness(i as u64);
            h.step = i as u64;
            version = store
                .commit(patch, h.clone(), &action, version)
                .await
                .unwrap();
            cur = CommittedState {
                harness: h,
                agent: Workspace { value: i * 10 },
            };
            if i == 2 {
                let (loaded, _) = store.load().await.unwrap().unwrap();
                captured_after_second = Some(loaded);
            }
        }

        let apply = apply_fn::<Workspace, ReplaceStrategy>();

        let replayed_v2 = store.replay(Version(2), &apply).await.unwrap();
        let expected_v2 = captured_after_second.unwrap();
        assert_eq!(replayed_v2.agent, expected_v2.agent);
        assert_eq!(replayed_v2.harness.step, expected_v2.harness.step);

        let replayed_v0 = store.replay(Version(0), &apply).await.unwrap();
        assert_eq!(replayed_v0.agent, initial.agent);

        let replayed_beyond = store.replay(Version(99), &apply).await.unwrap();
        let (current, _) = store.load().await.unwrap().unwrap();
        assert_eq!(replayed_beyond.agent, current.agent);
        assert_eq!(replayed_beyond.harness.step, current.harness.step);
    }

    #[tokio::test]
    async fn set_harness_changes_harness_without_bumping_version() {
        let store: InMemoryStore<Workspace> = InMemoryStore::new();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let (_, version_before) = store.load().await.unwrap().unwrap();

        let mut new_harness = harness(0);
        new_harness.inbox.push(InboxItem {
            id: 1,
            text: "hi".to_string(),
            acked: false,
        });
        store.set_harness(new_harness.clone()).await.unwrap();

        let (loaded, version_after) = store.load().await.unwrap().unwrap();
        assert_eq!(version_after, version_before);
        assert_eq!(loaded.harness.inbox.len(), 1);
        assert_eq!(loaded.harness.inbox[0].id, 1);
    }
}
