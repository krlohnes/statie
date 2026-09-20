//! Unit 26 (situational rules): a static procedure graph for the coding
//! skills, driving `guards.situational_rules` (`core/src/guards.rs`).
//!
//! Procedural Graphs (RELATED-WORK.md) found that localised guidance -- the
//! 2-hop neighbourhood of the last action -- beat injecting the whole
//! procedure text by 9 points and 71% fewer tokens; whole-text injection
//! hurt. This module is the localised form: [`situation`] derives a
//! [`Node`] (what just happened) from harness-owned state alone (`cur.
//! harness`, plus `cur.agent.tests` for the test outcome), and [`render`]
//! turns a node into at most 10 lines of bullets drawn from that node's
//! outgoing edges and their targets' own outgoing edges (2 hops).
//!
//! The edge text below is MOVED verbatim from the "after X" bullets of
//! `PlanSkill`/`ImplementSkill`/`ReviewSkill`'s `spec()` instructions
//! (`skills/src/coding/mod.rs`) -- wording unchanged, no new rules invented.
//! Bullets that hold in every situation (not conditioned on "what just
//! happened") stay in `spec()` as universal text; see each skill's `spec()`
//! for what remained. `ReviewSkill`'s instructions had no bullet shaped
//! "what to do after X" against this node set, so nothing moved out of it.

use statie_core::{CommittedState, HarnessState};

use super::{CodingWorkspace, FailureKind, classify_failure};

/// A situation in the procedure graph: "what just happened", derived
/// entirely from harness-owned state (never from the model's own
/// reasoning or workspace text, which the model could shape to steer its
/// own guidance -- see `guards.grounded_findings`, item 12, for the same
/// principle applied to review findings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    /// No tool call has executed yet this activation
    /// (`cur.harness.tool_calls == 0`).
    Start,
    /// The most recently executed tool was a read (`read_file`/
    /// `list_files`).
    Read,
    /// The most recently executed tool was `search`.
    Search,
    /// The most recently executed tool was a mutating edit
    /// (`edit_file`/`write_file`/`revert_file`/`shell`) and it succeeded.
    EditOk,
    /// The most recent tool call errored, and it was one of the mutating
    /// edit tools (`last_tool_error` names one of
    /// `edit_file`/`write_file`/`revert_file`/`shell`).
    EditError,
    /// The most recently executed tool was `test` and it passed.
    TestPass,
    /// The most recently executed tool was `test`, it failed, and the
    /// failure classifies as a compile error (`FailureKind::CompileError`).
    TestFailCompile,
    /// The most recently executed tool was `test`, it failed, and the
    /// failure classifies as an assertion (`FailureKind::Assertion`).
    TestFailAssert,
    /// The most recently executed tool was `test`, it failed, and the
    /// failure is neither a compile error nor an assertion (panic, timeout,
    /// or unclassified).
    TestFailOther,
    /// A `think` (or `try_fixes`) call ran since the last other tool call
    /// (`last_think_step == cur.harness.step`... see `situation`'s doc for
    /// the exact check).
    ThinkDone,
    /// The harness declined to re-run an identical read-only call
    /// (`last_skip` set).
    Skipped,
    /// The model is going back and forth on one path -- a skip or an
    /// edit-tool error recurring on the same path (`last_stall` set).
    Stalled,
}

/// One edge out of a node: the condition under which it applies (informal,
/// documentation only -- `situation` is the actual dispatcher), the
/// guidance bullet, and any pitfall bullet that goes with it.
pub(crate) struct Edge {
    pub(crate) to: Node,
    /// Human-readable condition this edge fires under; not evaluated by
    /// non-test code, purely documentation of why `situation` routes here --
    /// asserted non-empty by `tests::every_edge_documents_its_condition`
    /// (`#[cfg(not(test))]` on the `allow` below, so a normal `cargo build`
    /// doesn't warn about a field only the test module reads).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) condition: &'static str,
    /// The moved-verbatim guidance bullet.
    pub(crate) guidance: &'static str,
    /// A moved-verbatim pitfall/caution bullet, if the source text had one
    /// attached to this same situation. `None` when there wasn't one.
    pub(crate) pitfalls: Option<&'static str>,
}

/// Derives the current [`Node`] from harness-owned state only
/// (`cur.harness`) plus `cur.agent.tests`/`cur.harness.last_failure_summary`
/// for the test outcome (both are harness-written: see `CodingWorkspace::
/// tests`'s doc and `HarnessState::last_failure_summary`'s doc -- neither
/// is ever patched by the model). Checked in this fixed precedence order,
/// each mutually exclusive from the last-tool-call facts below:
///
/// 1. `stalled` (`last_stall` set) -- going back and forth on one path is
///    worth flagging over whatever the individual last call was.
/// 2. `skipped` (`last_skip` set) -- the harness declined to re-run a call;
///    nothing new happened.
/// 3. `think_done` -- `last_think_step` equals the current step, i.e. the
///    most recently executed tool was `think`/`try_fixes`.
/// 4. `edit_error` -- `last_tool_error` is set and names a mutating edit
///    tool (`edit_file`/`write_file`/`revert_file`/`shell`).
/// 5. Otherwise, dispatch on `last_tool` (the harness's own record of the
///    most recently *successfully* executed tool, unit 26): `test` splits
///    into `test_pass`/`test_fail_*` via `last_failure_summary`
///    (`None` -> pass; `Some` -> classified with `classify_failure`);
///    `read_file`/`list_files` -> `read`; `search` -> `search`;
///    `edit_file`/`write_file`/`revert_file`/`shell` -> `edit_ok`.
/// 6. `start` -- no tool call has executed yet this activation
///    (`tool_calls == 0`, or `last_tool` is still `None`).
pub fn situation(cur: &CommittedState<CodingWorkspace>) -> Node {
    let harness: &HarnessState = &cur.harness;

    if harness.last_stall.is_some() {
        return Node::Stalled;
    }
    if harness.last_skip.is_some() {
        return Node::Skipped;
    }
    if harness.last_think_step == Some(harness.step) {
        return Node::ThinkDone;
    }
    if let Some(err) = &harness.last_tool_error
        && matches!(
            err.tool.as_str(),
            "edit_file" | "write_file" | "revert_file" | "shell"
        )
    {
        return Node::EditError;
    }

    match harness.last_tool.as_deref() {
        Some("test") => match &harness.last_failure_summary {
            None => Node::TestPass,
            Some(summary) => match classify_failure(summary) {
                FailureKind::CompileError => Node::TestFailCompile,
                FailureKind::Assertion => Node::TestFailAssert,
                FailureKind::Panic | FailureKind::Timeout | FailureKind::Other => {
                    Node::TestFailOther
                }
            },
        },
        Some("read_file") | Some("list_files") => Node::Read,
        Some("search") => Node::Search,
        Some("edit_file") | Some("write_file") | Some("revert_file") | Some("shell") => {
            Node::EditOk
        }
        _ => Node::Start,
    }
}

// ---------------------------------------------------------------------
// Edge text, moved verbatim from `PlanSkill`/`ImplementSkill`'s `spec()`.
// ---------------------------------------------------------------------

/// From `start` (plan): moved from `PlanSkill::spec`'s "For a design fork
/// with more than one reasonable approach..." bullet.
const START_THINK_OPTIONS: Edge = Edge {
    to: Node::Start,
    condition: "before writing the plan, when there is a design fork",
    guidance: "For a design fork with more than one reasonable approach, call `think` with \
               `mode: \"options\"` before writing the plan.",
    pitfalls: None,
};

/// From `start`/`test_fail_*` (implement): moved from `ImplementSkill::
/// spec`'s "`try_fixes` is only useful after a failing `test`" bullet.
const TRY_FIXES_AFTER_FAILURE: Edge = Edge {
    to: Node::TestFailOther,
    condition: "no test has failed yet this activation, or the most recent one did",
    guidance: "`try_fixes` is only useful after a failing `test`: it proposes and verifies \
               candidate fixes for the most recent failure, so call `test` first if none has \
               failed yet.",
    pitfalls: None,
};

/// From `edit_ok` (implement): moved from `ImplementSkill::spec`'s "After
/// finishing a step that changes code, run `test`..." bullet.
const EDIT_OK_RUN_TEST: Edge = Edge {
    to: Node::EditOk,
    condition: "a step that changes code just finished",
    guidance: "After finishing a step that changes code, run `test` and read the `result:` \
               line; do not mark a step `done` while `test` reports `fail` unless the failure \
               predates your change (check with `recall`, e.g. query \"test output\" to see the \
               last test run).",
    pitfalls: None,
};

/// From `edit_error` (implement): moved from `ImplementSkill::spec`'s
/// "If `edit_file` reports the text was not found or ambiguous..." bullet.
const EDIT_ERROR_RETRY: Edge = Edge {
    to: Node::EditError,
    condition: "`edit_file` reported the text was not found or ambiguous",
    guidance: "If `edit_file` reports the text was not found or ambiguous, re-read the file and \
               retry with a more specific anchor -- never fall back to rewriting the whole \
               file.",
    pitfalls: None,
};

/// From `edit_error`/`test_fail_compile` (implement): moved from
/// `ImplementSkill::spec`'s "When a tool reports a line number..." bullet.
const READ_FROM_REPORTED_OFFSET: Edge = Edge {
    to: Node::EditError,
    condition: "a tool (an edit-not-found error, or a compile error) reported a line number",
    guidance: "When a tool reports a line number, read from that offset (`read_file` with \
               `offset`) instead of paging from the top.",
    pitfalls: None,
};

/// From `test_fail_compile`/`test_fail_assert`/`test_fail_other`
/// (implement): moved from `ImplementSkill::spec`'s "`think` asks a
/// reasoning subagent... use it when a fix did not change the failure or
/// you cannot tell which cause is real" bullet.
const THINK_WHEN_STUCK: Edge = Edge {
    to: Node::TestFailOther,
    condition: "a fix did not change the failure, or the cause is unclear",
    guidance: "`think` asks a reasoning subagent that sees what you see; use it when a fix did \
               not change the failure or you cannot tell which cause is real, then act on its \
               `decision`.",
    pitfalls: None,
};

/// Outgoing edges for one node -- the first hop.
pub(crate) fn edges(node: Node) -> Vec<Edge> {
    match node {
        Node::Start => vec![START_THINK_OPTIONS, TRY_FIXES_AFTER_FAILURE],
        Node::Read => vec![],
        Node::Search => vec![],
        Node::EditOk => vec![EDIT_OK_RUN_TEST],
        Node::EditError => vec![EDIT_ERROR_RETRY, READ_FROM_REPORTED_OFFSET],
        Node::TestPass => vec![],
        Node::TestFailCompile => vec![THINK_WHEN_STUCK, TRY_FIXES_AFTER_FAILURE],
        Node::TestFailAssert => vec![THINK_WHEN_STUCK, TRY_FIXES_AFTER_FAILURE],
        Node::TestFailOther => vec![THINK_WHEN_STUCK, TRY_FIXES_AFTER_FAILURE],
        Node::ThinkDone => vec![],
        Node::Skipped => vec![],
        Node::Stalled => vec![],
    }
}

/// Renders `node`'s outgoing edges and their targets' own outgoing edges
/// (2 hops), as bullets, deduped by guidance text (a bullet already shown
/// at hop 1 is not repeated if a hop-2 edge names the same text -- e.g.
/// `try_fixes` reachable both directly from a fail node and via that same
/// node's own `to`), at most 10 lines. Empty (no edges at either hop)
/// renders an empty string, which `PromptBuilder::situational` treats the
/// same as `None` -- no `## Now` section.
pub fn render(node: Node) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut seen_guidance: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();

    let hop1 = edges(node);
    let mut all_edges: Vec<&Edge> = Vec::new();
    all_edges.extend(hop1.iter());
    // Second hop: each hop-1 edge's target's own outgoing edges.
    let hop1_targets: Vec<Node> = hop1.iter().map(|e| e.to).collect();
    let hop2_owned: Vec<Vec<Edge>> = hop1_targets.iter().map(|&t| edges(t)).collect();
    for edge_set in &hop2_owned {
        all_edges.extend(edge_set.iter());
    }

    for edge in all_edges {
        if lines.len() >= 10 {
            break;
        }
        if !seen_guidance.insert(edge.guidance) {
            continue;
        }
        lines.push(format!("- {}", edge.guidance));
        if let Some(p) = edge.pitfalls {
            if lines.len() >= 10 {
                break;
            }
            lines.push(format!("- {p}"));
        }
    }

    lines.truncate(10);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use statie_core::{Budgets, HarnessState, InboxItem, RepeatedToolError, SkippedRead, Stall};

    fn base_harness() -> HarnessState {
        HarnessState {
            inbox: Vec::<InboxItem>::new(),
            active_tools: vec![],
            budgets: Budgets {
                total: 1000,
                observation: 100,
                augmentation: 100,
                notes: 100,
                working_set: 100,
            },
            activation: "implement".to_string(),
            step: 5,
            last_overflow: None,
            pinned: None,
            green_step: None,
            green_filtered: false,
            tool_calls: 1,
            last_test_cmd: None,
            edits_since_test: 0,
            last_think_step: None,
            repeated_failure_step: None,
            try_fixes_available: false,
            last_try_fixes_step: None,
            consecutive_non_question_asks: 0,
            consecutive_continues: 0,
            last_failure_summary: None,
            last_skip: None,
            last_tool_error: None,
            last_stall: None,
            last_tool: None,
            fails_since_green: 0,
            notes_step: None,
        }
    }

    fn committed(harness: HarnessState) -> CommittedState<CodingWorkspace> {
        CommittedState {
            harness,
            agent: CodingWorkspace::default(),
        }
    }

    #[test]
    fn no_tool_call_yet_is_start() {
        let mut h = base_harness();
        h.tool_calls = 0;
        h.last_tool = None;
        assert_eq!(situation(&committed(h)), Node::Start);
    }

    #[test]
    fn last_tool_read_file_is_read() {
        let mut h = base_harness();
        h.last_tool = Some("read_file".to_string());
        assert_eq!(situation(&committed(h)), Node::Read);
    }

    #[test]
    fn last_tool_list_files_is_read() {
        let mut h = base_harness();
        h.last_tool = Some("list_files".to_string());
        assert_eq!(situation(&committed(h)), Node::Read);
    }

    #[test]
    fn last_tool_search_is_search() {
        let mut h = base_harness();
        h.last_tool = Some("search".to_string());
        assert_eq!(situation(&committed(h)), Node::Search);
    }

    #[test]
    fn last_tool_edit_file_with_no_error_is_edit_ok() {
        let mut h = base_harness();
        h.last_tool = Some("edit_file".to_string());
        assert_eq!(situation(&committed(h)), Node::EditOk);
    }

    #[test]
    fn last_tool_write_file_is_edit_ok() {
        let mut h = base_harness();
        h.last_tool = Some("write_file".to_string());
        assert_eq!(situation(&committed(h)), Node::EditOk);
    }

    #[test]
    fn last_tool_revert_file_is_edit_ok() {
        let mut h = base_harness();
        h.last_tool = Some("revert_file".to_string());
        assert_eq!(situation(&committed(h)), Node::EditOk);
    }

    #[test]
    fn last_tool_shell_is_edit_ok() {
        let mut h = base_harness();
        h.last_tool = Some("shell".to_string());
        assert_eq!(situation(&committed(h)), Node::EditOk);
    }

    #[test]
    fn edit_tool_error_is_edit_error() {
        let mut h = base_harness();
        h.last_tool = Some("edit_file".to_string());
        h.last_tool_error = Some(RepeatedToolError {
            tool: "edit_file".to_string(),
            args: serde_json::json!({}),
            reason: "text not found".to_string(),
            shape: "not_found".to_string(),
            count: 1,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::EditError);
    }

    #[test]
    fn shell_tool_error_is_edit_error() {
        let mut h = base_harness();
        h.last_tool = Some("shell".to_string());
        h.last_tool_error = Some(RepeatedToolError {
            tool: "shell".to_string(),
            args: serde_json::json!({}),
            reason: "nonzero exit".to_string(),
            shape: "nonzero_exit".to_string(),
            count: 1,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::EditError);
    }

    #[test]
    fn read_tool_error_is_not_edit_error() {
        // `last_tool_error` naming a READ tool must not be treated as an
        // edit error -- only the mutating edit tools count.
        let mut h = base_harness();
        h.last_tool = Some("read_file".to_string());
        h.last_tool_error = Some(RepeatedToolError {
            tool: "read_file".to_string(),
            args: serde_json::json!({}),
            reason: "not found".to_string(),
            shape: "not_found".to_string(),
            count: 1,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::Read);
    }

    #[test]
    fn test_pass_when_last_tool_is_test_and_no_failure_summary() {
        let mut h = base_harness();
        h.last_tool = Some("test".to_string());
        h.last_failure_summary = None;
        assert_eq!(situation(&committed(h)), Node::TestPass);
    }

    #[test]
    fn test_fail_compile_classified_from_failure_summary() {
        let mut h = base_harness();
        h.last_tool = Some("test".to_string());
        h.last_failure_summary =
            Some("error[E0382]: use of moved value\n --> src/main.rs:5:1".to_string());
        assert_eq!(situation(&committed(h)), Node::TestFailCompile);
    }

    #[test]
    fn test_fail_assert_classified_from_failure_summary() {
        let mut h = base_harness();
        h.last_tool = Some("test".to_string());
        h.last_failure_summary = Some("assertion failed: left: 1 right: 2".to_string());
        assert_eq!(situation(&committed(h)), Node::TestFailAssert);
    }

    #[test]
    fn test_fail_other_classified_from_failure_summary() {
        let mut h = base_harness();
        h.last_tool = Some("test".to_string());
        h.last_failure_summary = Some("connection timed out".to_string());
        assert_eq!(situation(&committed(h)), Node::TestFailOther);
    }

    #[test]
    fn think_done_when_last_think_step_equals_current_step() {
        let mut h = base_harness();
        h.step = 9;
        h.last_think_step = Some(9);
        h.last_tool = Some("think".to_string());
        assert_eq!(situation(&committed(h)), Node::ThinkDone);
    }

    #[test]
    fn think_done_takes_precedence_over_edit_error() {
        // A `think` call substituted right after an edit error must still
        // report `think_done`, not `edit_error` -- `think_done` is checked
        // first.
        let mut h = base_harness();
        h.step = 9;
        h.last_think_step = Some(9);
        h.last_tool = Some("think".to_string());
        h.last_tool_error = Some(RepeatedToolError {
            tool: "edit_file".to_string(),
            args: serde_json::json!({}),
            reason: "text not found".to_string(),
            shape: "not_found".to_string(),
            count: 1,
            step: 8,
        });
        assert_eq!(situation(&committed(h)), Node::ThinkDone);
    }

    #[test]
    fn skipped_when_last_skip_set() {
        let mut h = base_harness();
        h.last_tool = Some("read_file".to_string());
        h.last_skip = Some(SkippedRead {
            tool: "read_file".to_string(),
            key: "read_file:{}".to_string(),
            count: 1,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::Skipped);
    }

    #[test]
    fn skipped_takes_precedence_over_read() {
        let mut h = base_harness();
        h.last_tool = Some("edit_file".to_string());
        h.last_skip = Some(SkippedRead {
            tool: "read_file".to_string(),
            key: "read_file:{}".to_string(),
            count: 1,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::Skipped);
    }

    #[test]
    fn stalled_when_last_stall_set() {
        let mut h = base_harness();
        h.last_tool = Some("edit_file".to_string());
        h.last_stall = Some(Stall {
            path: "a.rs".to_string(),
            count: 2,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::Stalled);
    }

    #[test]
    fn stalled_takes_precedence_over_everything_else() {
        let mut h = base_harness();
        h.last_tool = Some("test".to_string());
        h.last_failure_summary = Some("assertion failed: left: 1 right: 2".to_string());
        h.last_stall = Some(Stall {
            path: "a.rs".to_string(),
            count: 3,
            step: 5,
        });
        assert_eq!(situation(&committed(h)), Node::Stalled);
    }

    #[test]
    fn render_is_at_most_ten_lines_for_every_node() {
        for node in [
            Node::Start,
            Node::Read,
            Node::Search,
            Node::EditOk,
            Node::EditError,
            Node::TestPass,
            Node::TestFailCompile,
            Node::TestFailAssert,
            Node::TestFailOther,
            Node::ThinkDone,
            Node::Skipped,
            Node::Stalled,
        ] {
            let text = render(node);
            let line_count = if text.is_empty() {
                0
            } else {
                text.lines().count()
            };
            assert!(
                line_count <= 10,
                "render({node:?}) produced {line_count} lines, wanted <= 10: {text:?}"
            );
        }
    }

    #[test]
    fn render_edit_ok_contains_run_test_guidance() {
        let text = render(Node::EditOk);
        assert!(
            text.contains("run `test`"),
            "render(EditOk) must include the run-test bullet, got: {text:?}"
        );
    }

    #[test]
    fn render_edit_error_contains_retry_guidance() {
        let text = render(Node::EditError);
        assert!(
            text.contains("re-read the file and retry with a more specific anchor"),
            "render(EditError) must include the retry bullet, got: {text:?}"
        );
    }

    #[test]
    fn render_test_fail_assert_contains_think_guidance_not_edit_ok_guidance() {
        let text = render(Node::TestFailAssert);
        assert!(
            text.contains("use it when a fix did not change the failure"),
            "render(TestFailAssert) must include the think-when-stuck bullet, got: {text:?}"
        );
        assert!(
            !text.contains("run `test`"),
            "render(TestFailAssert) must not include the after-edit run-test bullet, got: {text:?}"
        );
    }

    #[test]
    fn render_read_and_search_and_test_pass_are_empty() {
        assert_eq!(render(Node::Read), "");
        assert_eq!(render(Node::Search), "");
        assert_eq!(render(Node::TestPass), "");
    }

    /// Every edge in the graph documents a non-empty `condition` and
    /// `guidance` -- keeps `condition` genuinely read (not dead code) and
    /// enumerates the whole graph in one place for review.
    #[test]
    fn every_edge_documents_its_condition() {
        for node in [
            Node::Start,
            Node::Read,
            Node::Search,
            Node::EditOk,
            Node::EditError,
            Node::TestPass,
            Node::TestFailCompile,
            Node::TestFailAssert,
            Node::TestFailOther,
            Node::ThinkDone,
            Node::Skipped,
            Node::Stalled,
        ] {
            for edge in edges(node) {
                assert!(
                    !edge.condition.is_empty(),
                    "edge from {node:?} to {:?} has an empty condition",
                    edge.to
                );
                assert!(
                    !edge.guidance.is_empty(),
                    "edge from {node:?} to {:?} has empty guidance",
                    edge.to
                );
            }
        }
    }
}
