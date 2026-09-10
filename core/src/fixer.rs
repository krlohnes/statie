//! The `try_fixes` proposer (statie-spec.md's "Hypotheses and verify"
//! pattern, BACKLOG.md item 4): a second kind of model call, made with
//! thinking off and JSON output, that samples k candidate fixes for a
//! repeated test failure. This module also defines [`CandidateRunner`], the
//! verification primitive the session drives each candidate through
//! (snapshot the sandbox once, try each candidate in its own worktree, run
//! the failing test there, apply the winner).
//!
//! `statie-core` cannot depend on `statie-tools` (dependency runs the other
//! way: `tools` depends on `core`), but the worktree snapshot/candidate/
//! run/apply/remove primitive that actually verifies a candidate lives in
//! `tools::worktree`, since it shells out to `git` and a subprocess the
//! same way the `shell`/`test` tools do. So the session (`core::Session::
//! execute_try_fixes`) is written against this trait, not against `tools::
//! worktree` directly: `statie_tools::worktree::WorktreeRunner` (added
//! alongside this unit) implements it for real, wired in from `cli/src/
//! main.rs`; [`MockCandidateRunner`] is a scripted stand-in for core's own
//! tests (which cannot depend on `statie-tools` for anything but dev-only
//! fixtures).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::model::Usage;

/// The fixed system prompt used by every `Fixer` backend.
pub const FIX_SYSTEM_PROMPT: &str = "You are a code-fixing subagent inside a coding harness. You are given a coding task's prompt and a failing test's output. Propose candidate fixes. Reply with ONLY a JSON array of up to k objects, each shaped {\"path\": string, \"old\": string, \"new\": string, \"rationale\": string}: `path` is the file to edit (sandbox-relative), `old` must be copied verbatim from a file you have seen in the prompt and must be unique in that file (it will be replaced exactly once), `new` is the replacement text, and `rationale` is a short reason. Each candidate must be a different hypothesis about what is wrong -- do not propose minor variations of the same fix. Output nothing but the JSON array: no prose before or after it, no markdown fence.";

/// Everything a `try_fixes` call sees: the calling step's own prompt
/// (system and user, verbatim, mirroring `ThinkInput`), the normalised
/// failure summary, and how many candidates to ask for.
pub struct FixInput {
    /// The system prompt of the step that called `try_fixes`, verbatim.
    pub system: String,
    /// The user prompt of that step, verbatim (workspace state,
    /// observation, etc.).
    pub user: String,
    /// The normalised failing-test summary (same shape
    /// `normalize_failure_summary` produces for the repeated-failure
    /// trigger).
    pub failure: String,
    /// How many candidate fixes to ask for.
    pub k: u32,
}

/// One concrete `edit_file`-shaped change: replace the exact text `old`
/// with `new` in `path` (sandbox-relative), with a short rationale for why
/// this might be the fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateEdit {
    pub path: String,
    pub old: String,
    pub new: String,
    pub rationale: String,
}

/// The proposer's output: the candidates it produced (already parsed,
/// deduped, and unioned across however many backend calls it took), and
/// usage from the last call that contributed a candidate.
#[derive(Clone, Debug)]
pub struct FixOutput {
    pub candidates: Vec<CandidateEdit>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum FixError {
    #[error("fixer backend error: {0}")]
    Backend(String),
    #[error("fixer call timed out")]
    Timeout,
    #[error("fixer backend returned no candidates")]
    Empty,
}

/// Drives one `try_fixes` call. Mirrors `Thinker`'s shape so `Arc<dyn
/// Fixer>` works the same way `Arc<dyn Thinker>` does.
#[async_trait]
pub trait Fixer: Send + Sync {
    async fn propose(
        &self,
        input: FixInput,
        cancel: CancellationToken,
    ) -> Result<FixOutput, FixError>;
}

/// An owned snapshot of one `FixInput`, recorded by `MockFixer` for test
/// assertions.
#[derive(Clone, Debug)]
pub struct FixCall {
    pub system: String,
    pub user: String,
    pub failure: String,
    pub k: u32,
}

/// A scripted `Fixer` for tests: consumes a `Vec<Result<...>>` in order,
/// recording every input it received. Once the script is exhausted,
/// returns `Err(FixError::Backend("script exhausted"))`.
pub struct MockFixer {
    script: Mutex<VecDeque<Result<FixOutput, FixError>>>,
    calls: Mutex<Vec<FixCall>>,
}

impl MockFixer {
    pub fn new(script: Vec<Result<FixOutput, FixError>>) -> Self {
        MockFixer {
            script: Mutex::new(script.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every `FixInput` this fixer has received, in call order.
    pub fn inputs(&self) -> Vec<FixCall> {
        self.calls.lock().expect("MockFixer calls poisoned").clone()
    }
}

#[async_trait]
impl Fixer for MockFixer {
    async fn propose(
        &self,
        input: FixInput,
        _cancel: CancellationToken,
    ) -> Result<FixOutput, FixError> {
        let call = FixCall {
            system: input.system,
            user: input.user,
            failure: input.failure,
            k: input.k,
        };
        self.calls
            .lock()
            .expect("MockFixer calls poisoned")
            .push(call);

        let mut script = self.script.lock().expect("MockFixer script poisoned");
        match script.pop_front() {
            Some(result) => result,
            None => Err(FixError::Backend("script exhausted".to_string())),
        }
    }
}

// ---------------------------------------------------------------------
// Verification: `CandidateRunner` and the types it moves. See the module
// doc for why this lives in `core` (the trait) with a real implementation
// in `tools::worktree` (the git-worktree mechanics) rather than the other
// way around.
// ---------------------------------------------------------------------

/// A snapshot of the sandbox's current working tree, opaque to the caller
/// beyond what it needs to create candidates from it and to know which
/// sandbox root it belongs to. `WorktreeRunner`'s is `tools::worktree::
/// Snapshot`; `MockCandidateRunner`'s is a plain marker. `Any` for the same
/// downcasting reason as [`RunnerCandidate`].
pub trait RunnerSnapshot: Send + Sync + std::any::Any {
    fn as_any(&self) -> &dyn std::any::Any;
}
impl<T: Send + Sync + std::any::Any> RunnerSnapshot for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// One candidate: an isolated copy of the sandbox with one edit applied,
/// opaque to the caller beyond what `run`/`apply`/`remove` need. Kept
/// generic (not a concrete struct) so `WorktreeRunner` can carry the real
/// `tools::worktree::Candidate` (worktree path, snapshot sha, root) and
/// `MockCandidateRunner` can carry whatever its script needs, with no
/// `core` type standing in for either. `Any` so an implementation's own
/// `diff`/`apply_to_sandbox`/`run` can downcast the trait object back to
/// its own concrete type (the only way to get at implementation-specific
/// data through a `Box<dyn RunnerCandidate>`).
pub trait RunnerCandidate: Send + Sync + std::any::Any {
    fn as_any(&self) -> &dyn std::any::Any;
}
impl<T: Send + Sync + std::any::Any> RunnerCandidate for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The result of running the failing-test command inside a candidate,
/// mirroring `tools::worktree::RunResult`'s shape (duplicated here rather
/// than imported, since `core` cannot depend on `tools`).
#[derive(Debug, Clone)]
pub struct CandidateRunResult {
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CandidateRunResult {
    /// Whether this run counts as a pass: exit code 0 and not timed out.
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit == Some(0)
    }
}

/// One `edit_file`-shaped change to apply inside a candidate worktree:
/// replace the exact text `old` with `new` in `path` (sandbox-relative).
/// The runner reads `<worktree>/<path>`, requires `old` to occur exactly
/// once, and writes the replacement -- see `CandidateRunner::candidate`'s
/// doc for the exact contract an implementation must honour.
#[derive(Clone, Debug)]
pub struct RunnerEdit {
    pub path: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RunnerError {
    /// `old` does not occur in the target file.
    #[error("anchor not found")]
    AnchorNotFound,
    /// `old` occurs more than once in the target file.
    #[error("anchor is ambiguous ({0} matches)")]
    AnchorAmbiguous(usize),
    /// The candidate's path escapes the worktree, or is otherwise invalid.
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// Any other failure creating/removing a worktree or applying a diff
    /// (a `git` failure, an I/O error): carries the backend's own message
    /// verbatim, since `core` has no `tools::worktree::WorktreeError` to
    /// match on.
    #[error("{0}")]
    Backend(String),
}

/// The verification primitive `try_fixes` drives: one snapshot of the
/// sandbox, then each candidate edit tried in its own isolated copy,
/// verified by running the failing test there, with the first passer
/// applied back to the real sandbox. Mirrors `tools::worktree`'s free
/// functions (`snapshot`/`candidate`/`run_in`/`apply_to_sandbox`/`remove`)
/// as trait methods so `core::Session::execute_try_fixes` can be written
/// against it without depending on `statie-tools`.
///
/// Implementations: `statie_tools::worktree::WorktreeRunner` (real git
/// worktrees) and [`MockCandidateRunner`] (core's own tests).
#[async_trait]
pub trait CandidateRunner: Send + Sync {
    /// Snapshots the sandbox's current working tree once per `try_fixes`
    /// call, before any candidate is created.
    async fn snapshot(&self, root: &Path) -> Result<Box<dyn RunnerSnapshot>, RunnerError>;

    /// Creates one isolated candidate from `snap` with `edit` applied:
    /// reads `<worktree>/<edit.path>`, rejects a path that escapes the
    /// worktree (`RunnerError::InvalidPath`), requires `edit.old` to occur
    /// exactly once in the file's current contents (zero occurrences ->
    /// `AnchorNotFound`, more than one -> `AnchorAmbiguous`), replaces it
    /// with `edit.new`, and writes the file.
    async fn candidate(
        &self,
        snap: &dyn RunnerSnapshot,
        edit: &RunnerEdit,
    ) -> Result<Box<dyn RunnerCandidate>, RunnerError>;

    /// Runs `cmd` inside `cand`'s isolated copy with the given timeout and
    /// output cap, mirroring `tools::worktree::run_in`.
    async fn run(
        &self,
        cand: &dyn RunnerCandidate,
        cmd: &str,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<CandidateRunResult, RunnerError>;

    /// The candidate's diff against the snapshot it was created from, for
    /// the winner's observation text.
    async fn diff(&self, cand: &dyn RunnerCandidate) -> Result<String, RunnerError>;

    /// Applies `cand`'s diff to the real sandbox working tree (index and
    /// HEAD untouched).
    async fn apply_to_sandbox(&self, cand: &dyn RunnerCandidate) -> Result<(), RunnerError>;

    /// Tears a candidate's isolated copy down. Called for every candidate
    /// created, on every path (winner, loser, or error), so no worktree
    /// under `.statie/worktrees/` outlives the `try_fixes` call.
    async fn remove(&self, cand: Box<dyn RunnerCandidate>) -> Result<(), RunnerError>;
}

/// One scripted step of a [`MockCandidateRunner`]'s run script: the
/// `CandidateRunResult` (or error) to return for the Nth `run` call, in
/// order.
pub type MockRunStep = Result<CandidateRunResult, RunnerError>;

/// A marker `RunnerSnapshot`/`RunnerCandidate` carrying just enough for
/// `MockCandidateRunner` to do its own bookkeeping: an index into the
/// script (`candidate`) or nothing at all (`snapshot`).
struct MockSnap;
struct MockCand {
    /// Which candidate call this is (0-based), so `run`/`diff` can look up
    /// the right scripted step/edit.
    index: usize,
    edit: RunnerEdit,
}

/// A scripted `CandidateRunner` for core's own tests (`core/tests/
/// try_fixes.rs`): `snapshot`/`candidate` always succeed (candidate
/// creation never fails in these tests -- the anchor-uniqueness check
/// happens against an in-memory file map, not real git worktrees), `run`
/// consumes one `MockRunStep` per call in order, `diff` returns a fixed
/// string per candidate (`"diff for candidate N"`, matching `apply`'s
/// bookkeeping), and `apply_to_sandbox` records which candidate index won
/// and actually writes the edit's `new` text to `<root>/<path>` so a test
/// can assert on the sandbox's real file contents afterward, the same as a
/// real `WorktreeRunner` would leave behind.
pub struct MockCandidateRunner {
    /// In-memory file contents, keyed by sandbox-relative path, used to
    /// evaluate `edit.old`'s occurrence count the same way a real worktree
    /// would from the file on disk.
    files: Mutex<std::collections::HashMap<String, String>>,
    run_script: Mutex<VecDeque<MockRunStep>>,
    /// Every `run` call's command, in order, for test assertions.
    run_calls: Mutex<Vec<String>>,
    /// Every candidate's edit, in creation order, for test assertions.
    candidates_created: Mutex<Vec<RunnerEdit>>,
    /// Set by `apply_to_sandbox` to the candidate index that was applied.
    applied_index: Mutex<Option<usize>>,
    /// Sandbox root, captured from `snapshot`, so `apply_to_sandbox` can
    /// write the winning edit's `new` text to the real file.
    root: Mutex<Option<PathBuf>>,
    /// Set true by `remove`, once per candidate created, so a test can
    /// assert every candidate was cleaned up.
    removed_count: Mutex<usize>,
}

impl MockCandidateRunner {
    /// Builds a runner over `files` (sandbox-relative path -> contents,
    /// read once at construction to seed the in-memory map `candidate`
    /// checks anchors against) and `run_script` (consumed one result per
    /// `run` call, in order; once exhausted, `run` returns
    /// `RunnerError::Backend("script exhausted")`).
    pub fn new(
        files: std::collections::HashMap<String, String>,
        run_script: Vec<MockRunStep>,
    ) -> Self {
        MockCandidateRunner {
            files: Mutex::new(files),
            run_script: Mutex::new(run_script.into_iter().collect()),
            run_calls: Mutex::new(Vec::new()),
            candidates_created: Mutex::new(Vec::new()),
            applied_index: Mutex::new(None),
            root: Mutex::new(None),
            removed_count: Mutex::new(0),
        }
    }

    /// Every `run` call's command, in order.
    pub fn run_calls(&self) -> Vec<String> {
        self.run_calls.lock().expect("poisoned").clone()
    }

    /// Every candidate's edit, in creation order.
    pub fn candidates_created(&self) -> Vec<RunnerEdit> {
        self.candidates_created.lock().expect("poisoned").clone()
    }

    /// The candidate index (0-based, creation order) that was applied to
    /// the sandbox, if any.
    pub fn applied_index(&self) -> Option<usize> {
        *self.applied_index.lock().expect("poisoned")
    }

    /// How many candidates have been `remove`d.
    pub fn removed_count(&self) -> usize {
        *self.removed_count.lock().expect("poisoned")
    }
}

#[async_trait]
impl CandidateRunner for MockCandidateRunner {
    async fn snapshot(&self, root: &Path) -> Result<Box<dyn RunnerSnapshot>, RunnerError> {
        *self.root.lock().expect("poisoned") = Some(root.to_path_buf());
        Ok(Box::new(MockSnap))
    }

    async fn candidate(
        &self,
        _snap: &dyn RunnerSnapshot,
        edit: &RunnerEdit,
    ) -> Result<Box<dyn RunnerCandidate>, RunnerError> {
        if edit.path.contains("..") || edit.path.starts_with('/') {
            return Err(RunnerError::InvalidPath(edit.path.clone()));
        }
        let files = self.files.lock().expect("poisoned");
        let contents = files
            .get(&edit.path)
            .ok_or_else(|| RunnerError::InvalidPath(edit.path.clone()))?;
        let occurrences = contents.matches(edit.old.as_str()).count();
        if occurrences == 0 {
            return Err(RunnerError::AnchorNotFound);
        }
        if occurrences > 1 {
            return Err(RunnerError::AnchorAmbiguous(occurrences));
        }
        drop(files);

        let index = {
            let mut created = self.candidates_created.lock().expect("poisoned");
            created.push(edit.clone());
            created.len() - 1
        };
        Ok(Box::new(MockCand {
            index,
            edit: edit.clone(),
        }))
    }

    async fn run(
        &self,
        _cand: &dyn RunnerCandidate,
        cmd: &str,
        _timeout: Duration,
        _max_output_bytes: usize,
    ) -> Result<CandidateRunResult, RunnerError> {
        self.run_calls
            .lock()
            .expect("poisoned")
            .push(cmd.to_string());
        let mut script = self.run_script.lock().expect("poisoned");
        match script.pop_front() {
            Some(result) => result,
            None => Err(RunnerError::Backend("script exhausted".to_string())),
        }
    }

    async fn diff(&self, cand: &dyn RunnerCandidate) -> Result<String, RunnerError> {
        // Safe: every `RunnerCandidate` this mock hands out is a `MockCand`.
        let cand = cand
            .as_any()
            .downcast_ref::<MockCand>()
            .expect("MockCandidateRunner always produces MockCand");
        Ok(format!(
            "diff for candidate {}: {} old={:?} new={:?}",
            cand.index, cand.edit.path, cand.edit.old, cand.edit.new
        ))
    }

    async fn apply_to_sandbox(&self, cand: &dyn RunnerCandidate) -> Result<(), RunnerError> {
        let cand = cand
            .as_any()
            .downcast_ref::<MockCand>()
            .expect("MockCandidateRunner always produces MockCand");
        *self.applied_index.lock().expect("poisoned") = Some(cand.index);

        let root = self.root.lock().expect("poisoned").clone();
        if let Some(root) = root {
            let mut files = self.files.lock().expect("poisoned");
            if let Some(contents) = files.get(&cand.edit.path) {
                let updated = contents.replacen(&cand.edit.old, &cand.edit.new, 1);
                files.insert(cand.edit.path.clone(), updated.clone());
                let _ = std::fs::write(root.join(&cand.edit.path), updated);
            }
        }
        Ok(())
    }

    async fn remove(&self, _cand: Box<dyn RunnerCandidate>) -> Result<(), RunnerError> {
        *self.removed_count.lock().expect("poisoned") += 1;
        Ok(())
    }
}

/// Extracts the first JSON array of candidate edits from a model reply:
/// tolerates a ```json fence and leading/trailing prose around the array.
/// Entries with an empty `path` or `old`, or where `old == new`, are
/// dropped; identical `(path, old, new)` triples are deduped (first
/// occurrence kept); order is otherwise preserved. Malformed input (no
/// parseable JSON array) returns an empty vec.
pub fn parse_candidates(text: &str) -> Vec<CandidateEdit> {
    let Some(json_slice) = extract_json_array(text) else {
        return Vec::new();
    };

    let raw: Vec<serde_json::Value> = match serde_json::from_str(json_slice) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in raw {
        let path = item
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let old = item
            .get("old")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let new = item
            .get("new")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let rationale = item
            .get("rationale")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        if path.is_empty() || old.is_empty() {
            continue;
        }
        if old == new {
            continue;
        }
        let key = (path.clone(), old.clone(), new.clone());
        if !seen.insert(key) {
            continue;
        }
        out.push(CandidateEdit {
            path,
            old,
            new,
            rationale,
        });
    }
    out
}

/// Finds the first JSON array in `text`: strips a ```json (or bare ```)
/// fence if present, then locates the first `[` and its matching `]`
/// (tracking string/escape state so brackets inside string values don't
/// confuse the scan), tolerating prose before and after. Returns `None` if
/// no balanced array is found.
fn extract_json_array(text: &str) -> Option<&str> {
    let text = strip_code_fence(text);
    let start = text.find('[')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strips a leading ```json / ``` fence and trailing ``` if the whole
/// (trimmed) text is fenced; otherwise returns the input unchanged. Prose
/// around a fenced block elsewhere in the text is left for `extract_json_
/// array`'s bracket scan to skip past.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_candidates_bare_array() {
        let text = r#"[{"path":"a.rs","old":"foo","new":"bar","rationale":"r1"}]"#;
        let out = parse_candidates(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[0].old, "foo");
        assert_eq!(out[0].new, "bar");
        assert_eq!(out[0].rationale, "r1");
    }

    #[test]
    fn parse_candidates_json_fenced() {
        let text = "```json\n[{\"path\":\"a.rs\",\"old\":\"foo\",\"new\":\"bar\",\"rationale\":\"r1\"}]\n```";
        let out = parse_candidates(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "a.rs");
    }

    #[test]
    fn parse_candidates_bare_fence_no_lang_tag() {
        let text =
            "```\n[{\"path\":\"a.rs\",\"old\":\"foo\",\"new\":\"bar\",\"rationale\":\"r1\"}]\n```";
        let out = parse_candidates(text);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn parse_candidates_prose_wrapped() {
        let text = "Here are my candidates:\n[{\"path\":\"a.rs\",\"old\":\"foo\",\"new\":\"bar\",\"rationale\":\"r1\"}]\nHope this helps!";
        let out = parse_candidates(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "a.rs");
    }

    #[test]
    fn parse_candidates_malformed_returns_empty() {
        assert!(parse_candidates("not json at all").is_empty());
        assert!(parse_candidates("[{\"path\": \"a.rs\", \"old\": }]").is_empty());
        assert!(parse_candidates("").is_empty());
    }

    #[test]
    fn parse_candidates_dedupes_identical_triples() {
        let text = r#"[
            {"path":"a.rs","old":"foo","new":"bar","rationale":"r1"},
            {"path":"a.rs","old":"foo","new":"bar","rationale":"r2 different rationale"},
            {"path":"a.rs","old":"foo","new":"baz","rationale":"r3"}
        ]"#;
        let out = parse_candidates(text);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].new, "bar");
        assert_eq!(out[0].rationale, "r1", "first occurrence kept");
        assert_eq!(out[1].new, "baz");
    }

    #[test]
    fn parse_candidates_drops_empty_path_or_old() {
        let text = r#"[
            {"path":"","old":"foo","new":"bar","rationale":"r1"},
            {"path":"a.rs","old":"","new":"bar","rationale":"r2"},
            {"path":"a.rs","old":"foo","new":"bar","rationale":"r3"}
        ]"#;
        let out = parse_candidates(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rationale, "r3");
    }

    #[test]
    fn parse_candidates_drops_old_equals_new() {
        let text = r#"[{"path":"a.rs","old":"same","new":"same","rationale":"r1"}]"#;
        assert!(parse_candidates(text).is_empty());
    }

    #[test]
    fn parse_candidates_respects_k_up_to_limit() {
        let text = r#"[
            {"path":"a.rs","old":"foo1","new":"bar1","rationale":"r1"},
            {"path":"a.rs","old":"foo2","new":"bar2","rationale":"r2"},
            {"path":"a.rs","old":"foo3","new":"bar3","rationale":"r3"}
        ]"#;
        let out = parse_candidates(text);
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn mock_fixer_records_inputs_in_order() {
        let mock = MockFixer::new(vec![
            Ok(FixOutput {
                candidates: vec![CandidateEdit {
                    path: "a.rs".to_string(),
                    old: "foo".to_string(),
                    new: "bar".to_string(),
                    rationale: "r".to_string(),
                }],
                usage: None,
            }),
            Err(FixError::Empty),
        ]);

        let out1 = mock
            .propose(
                FixInput {
                    system: "sys1".to_string(),
                    user: "user1".to_string(),
                    failure: "fail1".to_string(),
                    k: 3,
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out1.expect("ok").candidates.len(), 1);

        let out2 = mock
            .propose(
                FixInput {
                    system: "sys2".to_string(),
                    user: "user2".to_string(),
                    failure: "fail2".to_string(),
                    k: 2,
                },
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(out2, Err(FixError::Empty)));

        let calls = mock.inputs();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].system, "sys1");
        assert_eq!(calls[0].user, "user1");
        assert_eq!(calls[0].failure, "fail1");
        assert_eq!(calls[0].k, 3);
        assert_eq!(calls[1].k, 2);
    }

    #[tokio::test]
    async fn mock_fixer_exhausts_script() {
        let mock = MockFixer::new(vec![Ok(FixOutput {
            candidates: Vec::new(),
            usage: None,
        })]);

        let _ = mock
            .propose(
                FixInput {
                    system: "s".to_string(),
                    user: "u".to_string(),
                    failure: "f".to_string(),
                    k: 1,
                },
                CancellationToken::new(),
            )
            .await;

        let err = mock
            .propose(
                FixInput {
                    system: "s".to_string(),
                    user: "u".to_string(),
                    failure: "f".to_string(),
                    k: 1,
                },
                CancellationToken::new(),
            )
            .await
            .expect_err("script exhausted should error");
        match err {
            FixError::Backend(msg) => assert_eq!(msg, "script exhausted"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
