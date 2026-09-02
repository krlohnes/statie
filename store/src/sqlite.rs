//! `SqliteStore`: a `rusqlite`-backed, file- or memory-persisted
//! [`StateStore`] impl.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use statie_core::{
    Action, ApplyFn, CommittedState, HarnessState, LoggedPatch, StateStore, StoreError,
    ValidatedPatch, Version,
};

/// A `StateStore` backed by a SQLite database via `rusqlite`. All operations
/// are short and run synchronously inside a `std::sync::Mutex<Connection>`,
/// even though the trait methods are `async fn`.
pub struct SqliteStore<W> {
    conn: Mutex<Connection>,
    _workspace: PhantomData<fn() -> W>,
}

// Safety note: `Connection` is `Send` but not `Sync`; the `Mutex` gives us
// `Sync` for the store as a whole, which is what `StateStore: Send + Sync`
// requires.

fn backend_err(e: rusqlite::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn serde_err(e: serde_json::Error) -> StoreError {
    StoreError::Serde(e.to_string())
}

fn io_err(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

const MIGRATIONS: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
CREATE TABLE IF NOT EXISTS state (
    id      INTEGER PRIMARY KEY CHECK(id = 1),
    version INTEGER NOT NULL,
    harness TEXT NOT NULL,
    agent   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS patch_log (
    version INTEGER PRIMARY KEY,
    patch   TEXT NOT NULL,
    action  TEXT NOT NULL,
    harness TEXT NOT NULL,
    ts      TEXT NOT NULL
);
";

impl<W> SqliteStore<W>
where
    W: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(backend_err)?;
        conn.execute_batch(MIGRATIONS).map_err(backend_err)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
            _workspace: PhantomData,
        })
    }

    /// Opens (creating if necessary) a SQLite database at `path`, creating
    /// parent directories as needed, and runs migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let conn = Connection::open(path).map_err(backend_err)?;
        Self::from_connection(conn)
    }

    /// Opens an in-memory SQLite database (not persisted, not shared across
    /// connections). Useful for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(backend_err)?;
        Self::from_connection(conn)
    }
}

#[async_trait]
impl<W> StateStore<W> for SqliteStore<W>
where
    W: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT version, harness, agent FROM state WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(backend_err)?;

        let Some((version, harness_json, agent_json)) = row else {
            return Ok(None);
        };

        let harness: HarnessState = serde_json::from_str(&harness_json).map_err(serde_err)?;
        let agent: W = serde_json::from_str(&agent_json).map_err(serde_err)?;
        Ok(Some((
            CommittedState { harness, agent },
            Version(version as u64),
        )))
    }

    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError> {
        let mut conn = self.conn.lock().expect("sqlite mutex poisoned");
        let tx = conn.transaction().map_err(backend_err)?;

        let exists: Option<i64> = tx
            .query_row("SELECT id FROM state WHERE id = 1", [], |r| r.get(0))
            .optional()
            .map_err(backend_err)?;
        if exists.is_some() {
            return Err(StoreError::Backend("already initialized".to_string()));
        }

        let initial_json = serde_json::to_string(&initial).map_err(serde_err)?;
        let harness_json = serde_json::to_string(&initial.harness).map_err(serde_err)?;
        let agent_json = serde_json::to_string(&initial.agent).map_err(serde_err)?;

        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('initial', ?1)",
            params![initial_json],
        )
        .map_err(backend_err)?;
        tx.execute(
            "INSERT INTO state (id, version, harness, agent) VALUES (1, 0, ?1, ?2)",
            params![harness_json, agent_json],
        )
        .map_err(backend_err)?;

        tx.commit().map_err(backend_err)?;
        Ok(Version(0))
    }

    async fn commit(
        &self,
        patch: ValidatedPatch<W>,
        harness: HarnessState,
        action: &Action,
        expect: Version,
    ) -> Result<Version, StoreError> {
        // B1 defensive check: the patch must have been validated against the
        // version we are about to commit against.
        if patch.version() != expect {
            return Err(StoreError::VersionMismatch {
                expected: patch.version(),
                actual: expect,
            });
        }

        let (raw, next, _validated_version) = patch.into_parts();
        let new_version = Version(expect.0 + 1);

        let mut conn = self.conn.lock().expect("sqlite mutex poisoned");
        let tx = conn.transaction().map_err(backend_err)?;

        // Single transaction covering both the state UPDATE and the
        // patch_log INSERT: if the process were to die between the two
        // statements, the transaction's atomicity guarantees that either
        // both are durable or neither is — there is no way to observe a
        // bumped `state.version` without the matching `patch_log` row, or
        // vice versa.
        let harness_json = serde_json::to_string(&harness).map_err(serde_err)?;
        let agent_json = serde_json::to_string(&next).map_err(serde_err)?;

        let changed = tx
            .execute(
                "UPDATE state SET version = ?1, harness = ?2, agent = ?3 \
                 WHERE id = 1 AND version = ?4",
                params![
                    new_version.0 as i64,
                    harness_json,
                    agent_json,
                    expect.0 as i64
                ],
            )
            .map_err(backend_err)?;

        if changed == 0 {
            // Either the store was never initialized, or another commit
            // already moved past `expect`. Roll back (dropping `tx` without
            // committing does this) and report the actual current version.
            let actual: Option<i64> = tx
                .query_row("SELECT version FROM state WHERE id = 1", [], |r| r.get(0))
                .optional()
                .map_err(backend_err)?;
            drop(tx);
            return match actual {
                Some(v) => Err(StoreError::VersionMismatch {
                    expected: expect,
                    actual: Version(v as u64),
                }),
                None => Err(StoreError::Backend("store not initialized".to_string())),
            };
        }

        let patch_json = serde_json::to_string(&raw).map_err(serde_err)?;
        let action_json = serde_json::to_string(action).map_err(serde_err)?;
        let ts = chrono::Utc::now().to_rfc3339();

        tx.execute(
            "INSERT INTO patch_log (version, patch, action, harness, ts) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                new_version.0 as i64,
                patch_json,
                action_json,
                harness_json,
                ts
            ],
        )
        .map_err(backend_err)?;

        tx.commit().map_err(backend_err)?;
        Ok(new_version)
    }

    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT version, patch, action, harness FROM patch_log ORDER BY version ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |r| {
                let version: i64 = r.get(0)?;
                let patch: String = r.get(1)?;
                let action: String = r.get(2)?;
                let harness: String = r.get(3)?;
                Ok((version, patch, action, harness))
            })
            .map_err(backend_err)?;

        let mut out = Vec::new();
        for row in rows {
            let (version, patch, action, harness) = row.map_err(backend_err)?;
            let patch: serde_json::Value = serde_json::from_str(&patch).map_err(serde_err)?;
            let action: Action = serde_json::from_str(&action).map_err(serde_err)?;
            let harness: HarnessState = serde_json::from_str(&harness).map_err(serde_err)?;
            out.push(LoggedPatch {
                version: Version(version as u64),
                patch,
                action,
                harness,
            });
        }
        Ok(out)
    }

    async fn replay(
        &self,
        to: Version,
        apply: ApplyFn<'_, W>,
    ) -> Result<CommittedState<W>, StoreError> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");

        let initial_json: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'initial'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(backend_err)?;
        let Some(initial_json) = initial_json else {
            return Err(StoreError::Backend("store not initialized".to_string()));
        };
        let initial: CommittedState<W> = serde_json::from_str(&initial_json).map_err(serde_err)?;

        let mut stmt = conn
            .prepare("SELECT version, patch, harness FROM patch_log ORDER BY version ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |r| {
                let version: i64 = r.get(0)?;
                let patch: String = r.get(1)?;
                let harness: String = r.get(2)?;
                Ok((version, patch, harness))
            })
            .map_err(backend_err)?;

        let mut agent = initial.agent;
        let mut harness = initial.harness;

        for row in rows {
            let (version, patch_json, harness_json) = row.map_err(backend_err)?;
            if Version(version as u64) > to {
                break;
            }
            let patch: serde_json::Value = serde_json::from_str(&patch_json).map_err(serde_err)?;
            agent = apply(&agent, &patch).map_err(|e| StoreError::Backend(e.to_string()))?;
            harness = serde_json::from_str(&harness_json).map_err(serde_err)?;
        }

        Ok(CommittedState { harness, agent })
    }

    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let harness_json = serde_json::to_string(&harness).map_err(serde_err)?;
        let changed = conn
            .execute(
                "UPDATE state SET harness = ?1 WHERE id = 1",
                params![harness_json],
            )
            .map_err(backend_err)?;
        if changed == 0 {
            return Err(StoreError::Backend("store not initialized".to_string()));
        }
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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
        store.init(committed_state(0, 0)).await.unwrap();
        let err = store.init(committed_state(0, 0)).await.unwrap_err();
        assert!(matches!(err, StoreError::Backend(msg) if msg == "already initialized"));
    }

    #[tokio::test]
    async fn valid_commit_bumps_version_and_persists_agent_and_harness() {
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let (patch1, action1) = make_validated_patch(&initial, Version(0), 1, &[]);
        store
            .commit(patch1, harness(1), &action1, Version(0))
            .await
            .unwrap();

        let (before, before_version) = store.load().await.unwrap().unwrap();

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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let (stale_patch, stale_action) = make_validated_patch(&initial, Version(0), 99, &[]);

        let (other_patch, other_action) = make_validated_patch(&initial, Version(0), 1, &[]);
        store
            .commit(other_patch, harness(1), &other_action, Version(0))
            .await
            .unwrap();

        let err = store
            .commit(stale_patch, harness(2), &stale_action, Version(1))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn three_commits_produce_three_log_entries_with_versions_and_actions() {
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
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
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
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

    #[tokio::test]
    async fn survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");

        let replay_before_v1;
        {
            let store: SqliteStore<Workspace> = SqliteStore::open(&path).unwrap();
            let initial = committed_state(0, 0);
            store.init(initial.clone()).await.unwrap();

            let (patch1, action1) = make_validated_patch(&initial, Version(0), 1, &[]);
            let mut h1 = harness(1);
            h1.step = 1;
            store
                .commit(patch1, h1.clone(), &action1, Version(0))
                .await
                .unwrap();

            let cur_v1 = CommittedState {
                harness: h1,
                agent: Workspace { value: 1 },
            };
            let (patch2, action2) = make_validated_patch(&cur_v1, Version(1), 2, &[]);
            let mut h2 = harness(2);
            h2.step = 2;
            store
                .commit(patch2, h2, &action2, Version(1))
                .await
                .unwrap();

            let apply = apply_fn::<Workspace, ReplaceStrategy>();
            replay_before_v1 = store.replay(Version(1), &apply).await.unwrap();

            // store dropped here, closing the connection.
        }

        let store: SqliteStore<Workspace> = SqliteStore::open(&path).unwrap();
        let (loaded, version) = store.load().await.unwrap().expect("state persisted");
        assert_eq!(version, Version(2));
        assert_eq!(loaded.agent.value, 2);
        assert_eq!(loaded.harness.step, 2);

        let log = store.patch_log().await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].version, Version(1));
        assert_eq!(log[1].version, Version(2));

        let apply = apply_fn::<Workspace, ReplaceStrategy>();
        let replayed_v1 = store.replay(Version(1), &apply).await.unwrap();
        assert_eq!(replayed_v1.agent, replay_before_v1.agent);
        assert_eq!(replayed_v1.harness.step, replay_before_v1.harness.step);
    }

    #[tokio::test]
    async fn rejected_commit_does_not_grow_log_or_change_state() {
        let store: SqliteStore<Workspace> = SqliteStore::open_in_memory().unwrap();
        let initial = committed_state(0, 0);
        store.init(initial.clone()).await.unwrap();

        let (patch1, action1) = make_validated_patch(&initial, Version(0), 1, &[]);
        store
            .commit(patch1, harness(1), &action1, Version(0))
            .await
            .unwrap();

        let log_before = store.patch_log().await.unwrap();
        let (state_before, version_before) = store.load().await.unwrap().unwrap();

        // Stale expect: store is now at version 1, but we claim expect = 0.
        let cur_at_v1 = CommittedState {
            harness: harness(1),
            agent: Workspace { value: 1 },
        };
        let (patch2, action2) = make_validated_patch(&cur_at_v1, Version(0), 2, &[]);
        let err = store
            .commit(patch2, harness(2), &action2, Version(0))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::VersionMismatch { .. }));

        let log_after = store.patch_log().await.unwrap();
        let (state_after, version_after) = store.load().await.unwrap().unwrap();

        assert_eq!(log_before.len(), log_after.len());
        assert_eq!(version_before, version_after);
        assert_eq!(state_before.agent, state_after.agent);
    }

    #[tokio::test]
    async fn open_creates_nested_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/session.db");
        assert!(!path.parent().unwrap().exists());

        let store: SqliteStore<Workspace> = SqliteStore::open(&path).unwrap();
        store.init(committed_state(0, 0)).await.unwrap();

        assert!(path.exists());
    }
}
