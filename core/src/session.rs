//! The session loop: `Session<W>`, activation typestate (`Active<'a, K>`),
//! and the `step` algorithm (ARCHITECTURE.md "Loop, session, activation").

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::delegate::{
    ChildRequest, ChildRunner, InboxItem as ChildInboxItem, changed_top_level_keys,
};
use crate::error::SessionError;
use crate::fixer::{CandidateRunner, FixInput, Fixer, RunnerEdit};
use crate::guards::Guards;
use crate::log::{LogEntry, LogKind, SessionLog, StepRecord};
use crate::loop_types::{Event, Executable, Outcome, Park};
use crate::maintainer::{MaintainError, MaintainInputOwned, Maintainer};
use crate::model::{
    Action, Model, RawOutput, Schema, StepOutput, StepSink, Usage, restrict_action_types,
};
use crate::patch::PatchStrategy;
use crate::prompt::{AgentState, Augmentation, HarnessView, PromptBuilder, Snippet, SystemPrompt};
use crate::promptable::{Notes, Observation, Promptable};
use crate::read_window::parse_read_header;
use crate::report::ReportOutcome;
use crate::sandbox::SandboxRoot;
use crate::skill::{Commitment, ParsedOutput, Skill, ValidatedStep, validate_user_patch};
use crate::store::{LoggedPatch, StateStore, apply_fn};
use crate::thinker::{OPTIONS_MODE_INSTRUCTION, ThinkInput, Thinker};
use crate::tokenizer::{Budgets, TokenBudget, Tokenizer};
use crate::tool::ToolSet;
use crate::version::{CommittedState, HarnessState, InboxItem, Version};
use crate::working_set::{WorkingSet, WorkingSetConfig};

/// How many recent committed tool calls the loop guard remembers when
/// looking for repeated (name, args, output) triples, including
/// alternating patterns (e.g. A/B/A/B), not just consecutive repeats.
const TOOL_CALL_HISTORY: usize = 8;

/// Read-only tools tracked by the read-without-edit guard: any of these
/// naming a path in `args.path` counts toward that path's consecutive-read
/// streak. `search` and `recall` calls that name no path are ignored (there
/// is nothing to key the counter on).
const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "search",
    "filter_last_tool_call",
    "list_files",
    "recall",
];

/// Tools that mutate the workspace or run arbitrary code. Any of these
/// resets every path's read-streak counter outright, not just the path (if
/// any) it names: `write_file`/`edit_file` could reset only their own
/// path, but `shell`/`test` may have changed anything, so all four reset
/// everything for a single, simple rule.
const MUTATING_TOOLS: &[&str] = &["write_file", "edit_file", "revert_file", "shell", "test"];

/// Unit 25 part 3: the edit tools whose `tool_error` counts toward the
/// stalled-path streak (`HarnessState::last_stall`) alongside a refused
/// re-read of the same path -- a subset of `MUTATING_TOOLS` that excludes
/// `shell`/`test` (neither names a single `path` the way an edit call does,
/// so an error from either can't be attributed to "the path" the way this
/// guard needs).
const EDIT_TOOLS: &[&str] = &["edit_file", "write_file", "revert_file"];

/// From `4`, a consecutive-read note fires on every further read of that
/// path until an edit resets it.
const READ_GUARD_THRESHOLD: u32 = 4;

/// How many consecutive `Action::Continue` steps may produce the SAME note
/// before the session parks instead of continuing (see `Session::
/// last_continue_note`/`continue_repeat_count` and `Active::execute`'s
/// `Action::Continue` arm). A skill's `validate` override converts a
/// doomed-to-repeat step (e.g. review approving with no tool call) into
/// `Action::Continue` rather than rejecting it, which is the right call the
/// first couple of times -- but a live run sent the identical approval 35
/// times in a row before the retry cap (which never applies to a committed
/// `Continue` step) ever intervened. At the third identical note in a row,
/// the harness commits the patch as usual but parks on `Park::AskUser`
/// instead of returning `Outcome::Continue`, so a human (or the TUI/headless
/// driver) breaks the loop instead of the model repeating itself forever.
/// Also caps `Session::skip_repeat_count` (the same treatment for a
/// repeated `skipped_redundant` call) and `Session::tool_error_repeat_count`
/// (the same treatment for a repeated identical `tool_error`, identity
/// `(tool name, reason string)`) -- the tool-error guard parks through the
/// mixed-shape non-progress guard's own path (`park_non_progress_cap`)
/// rather than a bespoke message, but still fires at this cap, not
/// `NON_PROGRESS_CAP`.
const CONTINUE_REPEAT_CAP: u32 = 3;

/// How many consecutive non-progress steps of ANY shape may commit before
/// the session parks, regardless of whether consecutive occurrences repeat
/// the same shape. `CONTINUE_REPEAT_CAP` (via `continue_repeat_count`/
/// `skip_repeat_count`) already catches the same `Continue` note or the same
/// skipped call recurring three times running, but a live run alternated
/// three DIFFERENT non-progress shapes -- a skipped repeat of the test, an
/// empty `ask_user`, a premature `done` -- for roughly 30 steps with neither
/// cap ever firing, since neither streak was ever identical to itself twice
/// running. A step counts as non-progress here when its committed outcome
/// was a `Continue` conversion (a skill's `validate` override rewriting the
/// model's action, e.g. `ParsedOutput::set_action`, INCLUDING the no-`action`
/// recovery in `ParsedOutput::parse_with_fixes`) or a skipped-redundant call
/// (7.7/7.8's "do NOT run the tool" path); an executed tool call, a park, a
/// handoff, or `done` resets the count to 0. See
/// `Session::non_progress_count`/`non_progress_shapes`.
const NON_PROGRESS_CAP: u32 = 6;

/// Unit 27 ("no green cap"): the `HarnessState::fails_since_green` count at
/// which the coding skills' `implement_no_green_thinks`
/// (`skills/src/coding/mod.rs`) substitutes a `think` call, asking whether
/// the plan itself is wrong, once no `think` has run since the last failing
/// `test`'s step.
pub const NO_GREEN_THINK_AT: u32 = 12;

/// Unit 27: the `HarnessState::fails_since_green` count at which a failing
/// `test` call itself parks the session, through `park_non_progress_cap`,
/// with the shape `"no green in {NO_GREEN_PARK_AT} test runs"` -- a run that
/// has failed this many times since its last green checkpoint (in this
/// activation) is not making progress, regardless of what shape each
/// individual step took.
pub const NO_GREEN_PARK_AT: u32 = 16;

/// Large enough window to find any `ToolOutput` entry from the current
/// activation when looking up a previous `read_file` call's header for
/// 7.8's read-paging special case (mirrors `working_set.rs`'s
/// `LOG_SCAN_LIMIT`, which core cannot import since it's private to that
/// module).
const READ_HEADER_LOOKUP_LIMIT: usize = 1_000_000;

/// While a commitment (`Skill::commitment`) is set, at most this many reads
/// of the committed path are allowed before the commitment-slot rule starts
/// rejecting further reads and insists on the committed call (or a changed
/// `next`).
const COMMITMENT_READ_CAP: u32 = 2;

/// Result of `Active::apply_trusted_patch`: the new version, the harness
/// step it committed at, and the sorted top-level workspace keys the patch
/// actually changed.
struct AppliedTrustedPatch {
    version: Version,
    step: u64,
    keys: Vec<String>,
}

/// Unit 43 (D17 addendum: the maintainer's call is 1.3 s of a 3.25 s step):
/// the maintainer call spawned right
/// after the step it was given (`Active::spawn_maintainer`) runs off the
/// critical path. Held on `Session` between the step that spawned it and
/// the step that awaits it (`Active::drain_pending_maintainer`) -- never
/// more than one at a time (spec item 1: "at most one maintainer call in
/// flight"), since a drain always fully empties this back to `None` before
/// `spawn_maintainer` fills it again. The task itself
/// (`tokio::task::JoinHandle`) only calls `Maintainer::maintain` -- it never
/// touches the store or the log, so no commit ever races the task with the
/// session loop's own writes (spec item 4: "the store is never written from
/// two tasks at once").
struct PendingMaintainer {
    /// The harness step the spawned call was given (`MaintainInputOwned`'s
    /// step at spawn time) -- becomes `HarnessState.notes_step` once this
    /// task's result commits.
    step: u64,
    /// The token budget `notes` must fit, captured at spawn time alongside
    /// the rest of `MaintainInputOwned` -- needed again at drain time to
    /// check `out.notes` against it, exactly as `run_maintainer` does today.
    budget: usize,
    /// `reasoning.map(str::len).unwrap_or(0)` at spawn time, carried through
    /// to the `maintainer` System entry the drain logs (unchanged from
    /// today's `run_maintainer`).
    reasoning_chars: usize,
    /// The observation text's length at spawn time, carried through the
    /// same way as `reasoning_chars`.
    observation_chars: usize,
    handle: tokio::task::JoinHandle<Result<crate::maintainer::MaintainOutput, MaintainError>>,
}

/// A verification command (`shell` or `test`) that ran to completion, kept
/// so an exact repeat with nothing changed in between can be rejected (see
/// `Session::last_verify` / the redundant-verification guard in
/// `Active::step`).
#[derive(Clone, Debug, PartialEq)]
struct VerifyRun {
    /// The tool that ran the command (`"shell"` or `"test"`). A repeat is
    /// only redundant when both the tool AND the command match: `test`
    /// listens for its own green checkpoint (`note_test_result`), which a
    /// `shell` run of the identical command never produces, so a `test`
    /// call must still run even if a `shell` of the same command already
    /// ran (and vice versa) -- see the redundant-verification guard in
    /// `Active::step`.
    tool: String,
    /// The command as normalized by `normalize_command`.
    cmd: String,
    /// Short human-readable summary of what the command returned, shown in
    /// the rejection message.
    summary: String,
}

/// The result of the 7.7 (redundant-verification) or 7.8
/// (redundant-identical-read) guard recognizing this step's tool call as an
/// exact repeat: the step still commits (patch + action + acks), but the
/// call itself is not re-run in `Active::execute` -- see that guard's
/// commentary in `Active::step` and the "do NOT run the tool" handling in
/// `Active::execute`.
struct SkippedCall {
    /// The guard's rejection text, unchanged from what `reject` would have
    /// sent -- used verbatim as (or as the first line of) the pending
    /// observation.
    reason: String,
    /// The previous result to show the model in place of re-running the
    /// call: 7.7's `last.summary` (what the command returned last time).
    /// Empty for 7.8, whose guard reason already points at the log
    /// (`"...that output is in the log"`), so there is nothing further to
    /// show.
    cached: String,
}

/// `read_file`'s own default `limit` (`tools/src/read_file.rs`'s
/// `default_limit`), used when paging forward past a truncated window and
/// the model's repeat named no `limit` of its own.
const DEFAULT_READ_FILE_LIMIT: u64 = 100;

/// 7.8's special case for `read_file`: an identical repeat whose previous
/// window was truncated pages forward instead of being skipped (see
/// `Active::step`'s 7.8 commentary). Carries what's needed to rewrite the
/// call's args before it runs, and to render the "you repeated the read"
/// note and the `paged_read` System entry afterward.
struct PagedRead {
    /// `args.path` of the repeated call, used in the `paged_read` log entry.
    path: String,
    /// The previous window's `(from, to)` line range, parsed from its
    /// output header.
    from: (u64, u64),
    /// The model's original args for this call, unchanged -- logged
    /// verbatim in the `paged_read` System entry "for the record" even
    /// though the committed `Action` entry carries the rewritten args.
    original_args: Value,
    /// The offset the rewritten call will read from (`from.1 + 1`).
    to_offset: u64,
}

/// Normalizes a shell/test command string for the redundant-verification
/// guard: trims, cuts at the first `|` (so `echo hi` and `echo hi | cat` are
/// treated the same), strips a trailing `2>&1`, collapses runs of
/// whitespace to one space, and trims again.
fn normalize_command(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let cut = match trimmed.find('|') {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    let cut = cut.trim();
    let cut = cut.strip_suffix("2>&1").unwrap_or(cut);
    let collapsed = cut.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim().to_string()
}

/// Canonical key for one read-only tool call, used by the
/// redundant-identical-read guard (`Session::reads_since_mutation`):
/// `"{name} {args}"`, where `args` is `args`'s compact JSON. `serde_json`'s
/// `Value::Object` is a `BTreeMap` unless the `preserve_order` feature is
/// enabled (it is not, workspace-wide -- see `Cargo.lock`), so `Value`'s
/// `Display`/`to_string()` already serializes object keys in sorted order;
/// no separate normalization step is needed for the args to be canonical.
///
/// `pub`: the coding skills' `implement_repeated_skip_thinks`
/// (`skills/src/coding/mod.rs`) computes the same key from a read-only
/// tool call to match it against `HarnessState::last_skip`'s recorded key.
pub fn read_call_key(name: &str, args: &Value) -> String {
    format!("{name} {args}")
}

/// A whitespace-delimited token's path-like-ness for [`error_shape`]: `true`
/// when it contains `/` or ends in one of a fixed set of source/doc
/// extensions (`.rs`, `.py`, `.toml`, `.txt`, `.md`). A trailing punctuation
/// character right after the extension (`.`, `,`, `;`, `:`, `)`, `"`, `'`)
/// is allowed and kept outside the replacement, so `"src/lib.rs."` and
/// `"src/lib.rs,"` still normalize their path portion and keep the
/// punctuation, matching how such tokens actually appear in a sentence.
fn path_like_token(token: &str) -> Option<usize> {
    const PATH_EXTENSIONS: &[&str] = &[".rs", ".py", ".toml", ".txt", ".md"];
    let trimmed_end = token.trim_end_matches(['.', ',', ';', ':', ')', '"', '\'']);
    if trimmed_end.is_empty() {
        return None;
    }
    let is_path =
        trimmed_end.contains('/') || PATH_EXTENSIONS.iter().any(|ext| trimmed_end.ends_with(ext));
    if is_path {
        Some(trimmed_end.len())
    } else {
        None
    }
}

/// Unit 25: the reason string's SHAPE, ignoring the specific numbers and
/// paths it names, so two errors of the same kind (e.g. "old text not found
/// in <path>") key together regardless of which line number or which file
/// they happened to name. Used by the identical-tool-error streak
/// (`Session::last_tool_error_key`/`tool_error_repeat_count`,
/// `HarnessState::last_tool_error`) and the stalled-path streak
/// (`HarnessState::last_stall`) so a model that re-issues the same KIND of
/// failing edit with a different line number is still recognized as
/// repeating itself, instead of looking like a fresh error every time.
///
/// Transform, in order:
/// 1. Every whitespace-delimited token that is path-like (contains `/`, or
///    ends in `.rs`/`.py`/`.toml`/`.txt`/`.md`, ignoring a trailing
///    punctuation character) is replaced by `<path>` (punctuation kept).
/// 2. Every run of ASCII digits remaining is replaced by a single `#`.
/// 3. Every backtick-quoted span (from one `` ` `` to the next) is replaced
///    by `` `…` `` -- this runs last so a path or number quoted inside
///    backticks (e.g. `` `pub fn new(...)` ``) never leaks digits or path
///    text into the shape, since the whole span collapses regardless of
///    what it contains.
/// 4. Whitespace is collapsed (runs of whitespace become one space, and the
///    result is trimmed).
/// 5. The result is cut to 120 characters.
pub fn error_shape(reason: &str) -> String {
    // Step 1: path-like tokens -> `<path>`, token by token, preserving
    // everything else (including inter-token whitespace) unchanged so step
    // 4's whitespace collapse still sees the original spacing.
    let mut after_paths = String::with_capacity(reason.len());
    for token in split_keep_whitespace(reason) {
        if token.chars().next().is_some_and(char::is_whitespace) {
            after_paths.push_str(token);
            continue;
        }
        if let Some(end) = path_like_token(token) {
            after_paths.push_str("<path>");
            after_paths.push_str(&token[end..]);
        } else {
            after_paths.push_str(token);
        }
    }

    // Step 2: runs of ASCII digits -> `#`.
    let mut after_digits = String::with_capacity(after_paths.len());
    let mut chars = after_paths.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            after_digits.push('#');
        } else {
            after_digits.push(c);
        }
    }

    // Step 3: backtick-quoted spans -> `` `…` ``.
    let mut after_backticks = String::with_capacity(after_digits.len());
    let mut in_span = false;
    for c in after_digits.chars() {
        if c == '`' {
            if in_span {
                after_backticks.push('…');
                after_backticks.push('`');
                in_span = false;
            } else {
                after_backticks.push('`');
                in_span = true;
            }
        } else if !in_span {
            after_backticks.push(c);
        }
        // Characters inside a span are dropped -- the `…` placeholder
        // already pushed when the span opened stands in for all of them.
    }

    // Step 4: collapse whitespace.
    let collapsed = after_backticks
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Step 5: cut to 120 characters.
    if collapsed.chars().count() <= 120 {
        collapsed
    } else {
        collapsed.chars().take(120).collect()
    }
}

/// The fixed opening of the redundant-identical-read refusal below (7.8),
/// exported so `tools/src/read_file.rs` can quote it in `read_file`'s
/// description without a second copy of the text (D13).
pub const REDUNDANT_READ_PREFIX: &str = "you already ran";

/// Unit 39: true when `reason` is one of the `tool_rejected` texts produced
/// when the call named no USABLE tool at all, rather than a real tool that
/// then failed -- so the repeated-tool-error streak below can key every such
/// call together as one class (`is_not_a_tool_rejection`'s caller in
/// `Active::execute`), instead of splintering into one key per distinct
/// (often garbage) `name`. Bench `qwen_json_short_b23/rename_across_files/
/// seed1`: the model sent 107 rejected calls in a row, alternating
/// `name: ""` and `name: "src/config.rs"`; keyed on name as before, the two
/// alternated and the streak's count never reached `CONTINUE_REPEAT_CAP`, so
/// the run timed out at 126 steps instead of parking. Recognises, by fixed
/// prefix/phrase (not the whole text, so the path/skill names and the
/// active-tool list embedded in the real messages don't have to match):
/// - the empty-`name` text ("action.name is empty: put the tool's name in
///   `name` and its arguments in `args`; available tools: ..."), produced
///   right here in `Active::execute` when `name.is_empty()`;
/// - the "not available in skill" text's TWO forms, both produced right
///   here in `Active::execute`'s inactive-tool branch -- the hand-off form
///   ("tool `X` is not available in skill `S`; `T` has it. Finish this
///   skill's job, then hand off: ...", when `Skill::skill_with_tool` found
///   another skill that has it) and the plain form ("tool `X` is not
///   available in skill `S`; available tools: ...There is no tool by that
///   name; if `X` is a path, it belongs in `args`.", when no skill has it).
///
/// A real tool's own `tool_error` reason (e.g. `read_file`'s "no such file",
/// `edit_file`'s anchor-miss) never starts with either phrase, so this
/// returns `false` for those, keeping their own `(tool, error_shape(reason))`
/// key untouched.
pub fn is_not_a_tool_rejection(reason: &str) -> bool {
    reason.starts_with("action.name is empty: ")
        || (reason.starts_with("tool `") && reason.contains("` is not available in skill `"))
}

/// Splits `s` into alternating non-whitespace/whitespace runs, in order,
/// covering the whole string -- used by [`error_shape`] to walk tokens while
/// keeping the original whitespace between them intact for step 4's later
/// collapse.
fn split_keep_whitespace(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    if s.is_empty() {
        return parts;
    }
    let mut start = 0;
    let mut cur_is_ws = s.chars().next().is_some_and(char::is_whitespace);
    for (idx, c) in s.char_indices() {
        let is_ws = c.is_whitespace();
        if is_ws != cur_is_ws && idx != start {
            parts.push(&s[start..idx]);
            start = idx;
            cur_is_ws = is_ws;
        }
    }
    parts.push(&s[start..]);
    parts
}

#[cfg(test)]
mod error_shape_tests {
    use super::error_shape;

    #[test]
    fn line_numbers_and_paths_collapse_to_the_same_shape() {
        let a = "line 42 of src/lib.rs already has `pub fn new(...)`";
        let b = "line 53 of src/lib.rs already has `pub fn count(...)`";
        assert_eq!(
            error_shape(a),
            error_shape(b),
            "different line numbers and different backtick-quoted spans must still be the SAME \
             shape: {:?} vs {:?}",
            error_shape(a),
            error_shape(b)
        );
        assert_eq!(error_shape(a), "line # of <path> already has `…`");
    }

    #[test]
    fn different_kinds_of_error_are_different_shapes() {
        let a = "old text not found in src/config.rs";
        let b = "anchor line occurs 2 times in src/main_logic.rs";
        assert_ne!(
            error_shape(a),
            error_shape(b),
            "a missing-old-text error and an ambiguous-anchor error must be DIFFERENT shapes: \
             {:?} vs {:?}",
            error_shape(a),
            error_shape(b)
        );
        assert_eq!(error_shape(a), "old text not found in <path>");
        assert_eq!(error_shape(b), "anchor line occurs # times in <path>");
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(error_shape("a   b\t\tc\n\nd"), "a b c d");
    }

    #[test]
    fn long_reason_is_cut_to_120_characters() {
        let long = "x".repeat(500);
        let shape = error_shape(&long);
        assert_eq!(shape.chars().count(), 120);
    }
}

/// True when `action` is a `think` call (`Action::Tool { name: "think", .. }`)
/// -- whether the model chose it itself or a skill's `validate` substituted
/// it (`execute_think`'s own doc: "a harness substitution is indistinguishable
/// from the model choosing `think` itself" by the time an action reaches
/// `execute`). Used by the repeated-skip and repeated-tool-error guards
/// (`Active::step`, right after `execute` returns) to exempt a `think` step
/// from resetting either streak: unit 23 part (b) originally reset both
/// streaks on any non-matching committed step, `think` included, which let a
/// model alternate error/think/error/think forever without ever reaching
/// `CONTINUE_REPEAT_CAP` (the substituted `think` itself cleared
/// `last_tool_error`/`last_skip`, so the next identical call always looked
/// like a fresh streak). A `think` step now leaves both streaks exactly as
/// they were; every other non-matching step (an executed tool call, a
/// skipped-redundant call of a different shape, a rejection) still resets
/// them as before.
fn is_think_action(action: &Action) -> bool {
    matches!(action, Action::Tool { name, .. } if name == "think")
}

/// True when `patch` is an empty JSON object (`{}`) -- the raw patch object
/// has no keys, so this step's patch recorded nothing to the workspace.
/// Anything else (a non-empty object, or a patch strategy that never sends
/// an object at all) is not empty: it wrote, or attempted to write,
/// something.
fn is_empty_patch(patch: &Value) -> bool {
    matches!(patch, Value::Object(map) if map.is_empty())
}

/// Maximum length, in characters, of one shape description in the
/// `NON_PROGRESS_CAP` park message/log entry (see `non_progress_shape`) --
/// long enough for a short clause, short enough that six of them stay
/// readable on one line.
const NON_PROGRESS_SHAPE_MAX_CHARS: usize = 60;

/// Short, human-readable description of one non-progress step, for the
/// `NON_PROGRESS_CAP` guard's park message and `non_progress_cap` log entry
/// (see `Session::non_progress_shapes`). For a skipped-redundant call, names
/// the tool and command/key, matching the existing repeated-skip guard's own
/// text. For a `Continue` conversion, core has no visibility into what the
/// model's original action was (a skill's `validate` override already
/// replaced it before this point) -- the best generic description available
/// is a short lead-in of the note itself: up to the first `.`/`:`/newline,
/// capped at `NON_PROGRESS_SHAPE_MAX_CHARS` chars (with a trailing "..." when
/// cut).
fn non_progress_shape(tool_skipped: bool, action: &Action) -> String {
    if tool_skipped && let Action::Tool { name, args } = action {
        let described = extract_command(args).unwrap_or_else(|| read_call_key(name, args));
        return format!("skipped {name} {described}");
    }
    if let Action::Continue { note } = action {
        let cut = note
            .find(['.', ':', '\n'])
            .map(|i| &note[..i])
            .unwrap_or(note.as_str())
            .trim();
        let truncated: String = cut.chars().take(NON_PROGRESS_SHAPE_MAX_CHARS).collect();
        return if truncated.chars().count() < cut.chars().count() {
            format!("{truncated}...")
        } else {
            truncated
        };
    }
    "non-progress step".to_string()
}

/// One candidate's loser line, as it appears in a `try_fixes` observation:
/// `"<n>. <path> — <rationale> -> <reason>"`.
struct Loser {
    line: String,
}

/// The result of `Active::verify_candidates`: either the first candidate
/// that passed (with its diff, ready to report, already applied to the
/// sandbox) or no candidate passed at all -- either way, every candidate's
/// loser line so far (for `NoneWon`, that is all of them; for `Winner`,
/// every OTHER candidate, including the untried tail).
enum VerifyOutcome {
    Winner {
        index: usize,
        edit: crate::fixer::CandidateEdit,
        diff: String,
        cmd: String,
        losers: Vec<Loser>,
    },
    NoneWon {
        cmd: String,
        losers: Vec<Loser>,
    },
}

/// The first line of `stdout`/`stderr` (stdout first, then stderr) that
/// starts with (trimmed) one of a fixed set of failure markers, else the
/// last non-empty line of the combined output, capped at 200 chars. Used
/// to build a `try_fixes` loser's `-> <reason>` suffix.
fn loser_reason(stdout: &str, stderr: &str, timed_out: bool) -> String {
    const MARKERS: &[&str] = &[
        "error[",
        "error:",
        "thread '",
        "panicked at",
        "assertion",
        "AssertionError",
        "Error:",
        "FAILED",
        "FAIL:",
        "Traceback",
    ];

    if timed_out {
        return cap_reason("timed out");
    }

    let combined = format!("{stdout}\n{stderr}");
    for line in combined.lines() {
        let trimmed = line.trim();
        if MARKERS.iter().any(|m| trimmed.starts_with(m)) {
            return cap_reason(trimmed);
        }
    }

    let last_non_empty = combined
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)");
    cap_reason(last_non_empty)
}

fn cap_reason(text: &str) -> String {
    if text.chars().count() <= 200 {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(200).collect();
        truncated
    }
}

/// Extracts the command string for `shell`/`test` args: `args.cmd`, falling
/// back to `args.command` (the alias the tools themselves accept), then
/// normalizes it. `None` when neither key is a string.
fn extract_command(args: &Value) -> Option<String> {
    let raw = args
        .get("cmd")
        .and_then(Value::as_str)
        .or_else(|| args.get("command").and_then(Value::as_str))?;
    Some(normalize_command(raw))
}

/// Recovers the command a `test` call actually ran when its args carried no
/// `cmd`/`command` (auto-detected by `tools/src/test_tool.rs`'s
/// `detect_test_command`), by parsing it back out of the tool's own output:
/// `format_result` (`tools/src/test_tool.rs`) always prints `command: <cmd>`
/// as the first line, whether the run passed, failed, or its exit code
/// couldn't be parsed. `None` when `output` has no such line (never happens
/// for a real `test` call, but guards a malformed/mocked one). Normalized the
/// same way `extract_command` normalizes an explicit `cmd` argument, so
/// `harness.last_test_cmd` is comparable either way it was populated.
pub fn detected_test_command(output: &str) -> Option<String> {
    let line = output.lines().next()?;
    let raw = line.strip_prefix("command: ")?;
    Some(normalize_command(raw))
}

/// True for a line that is pure framing added by `run_shell`/the `test`
/// tool around a command's real output (`exit: N`, `--- stdout ---`,
/// `--- stderr ---`), not content the command itself returned.
fn is_verify_framing_line(line: &str) -> bool {
    line.starts_with("exit: ")
        || line.starts_with("command: ")
        || line == "--- stdout ---"
        || line == "--- stderr ---"
        || line == "--- failure summary ---"
}

/// First non-empty, non-framing line of `text` (skipping `run_shell`'s
/// `exit:`/`--- stdout ---`/`--- stderr ---` markers, which describe the
/// wrapper, not what the command returned), followed by up to three further
/// lines that contain `FAILED`, `error[`, or `panicked`, joined by `; ` and
/// truncated to 300 chars. Used as the `summary` of a `VerifyRun`.
fn summarize_verify_output(text: &str) -> String {
    let mut lines = text.lines().map(str::trim);
    let first = lines
        .find(|l| !l.is_empty() && !is_verify_framing_line(l))
        .unwrap_or("");
    let mut parts: Vec<&str> = vec![first];
    for line in lines {
        if parts.len() >= 4 {
            break;
        }
        if line.contains("FAILED") || line.contains("error[") || line.contains("panicked") {
            parts.push(line);
        }
    }
    let joined = parts.join("; ");
    joined.chars().take(300).collect()
}

/// Normalizes a failing `test` tool output (`tools/src/test_tool.rs`'s
/// `format_result`) down to just its failure summary, for the
/// repeated-failure guard to compare consecutive fails by. When the
/// `--- failure summary ---` marker is present, returns the (trimmed) text
/// after it, up to the next line starting with `--- ` (typically
/// `--- stdout ---`) or the end of the text. When the marker is absent
/// (e.g. `result:` couldn't be parsed), falls back to everything after the
/// first line (dropping the `command: ...` line), trimmed.
fn normalize_failure_summary(output: &str) -> String {
    const MARKER: &str = "--- failure summary ---\n";
    let raw = if let Some(start) = output.find(MARKER) {
        let rest = &output[start + MARKER.len()..];
        let summary_lines: Vec<&str> = rest
            .lines()
            .take_while(|l| !l.starts_with("--- "))
            .collect();
        summary_lines.join("\n")
    } else {
        match output.split_once('\n') {
            Some((_, rest)) => rest.to_string(),
            None => output.to_string(),
        }
    };
    scrub_volatile(&raw).trim().to_string()
}

/// Removes the parts of a test failure that change between two runs of
/// the same failing test, so the repeated-failure guard compares what the
/// model can act on: a parenthesised run of digits (Rust's panic line
/// carries the thread id, `thread 'name' (12100280) panicked at ...`, and
/// it differs every run) and `finished in 0.00s` timings. Without this,
/// two byte-identical assertion failures never counted as a repeat: the
/// first live battery with the trap task re-ran the same failing test five
/// times and the guard fired zero times.
fn scrub_volatile(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && chars[j] == ')' {
                i = j + 1;
                continue;
            }
        }
        const TIMING: &[char] = &['f', 'i', 'n', 'i', 's', 'h', 'e', 'd', ' ', 'i', 'n', ' '];
        if chars[i..].starts_with(TIMING) {
            let mut j = i + TIMING.len();
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            if j > i + TIMING.len() && j < chars.len() && chars[j] == 's' {
                out.push_str("finished in Xs");
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A cloneable, shareable handle that can cancel the in-flight `step()`
/// call. A fresh `CancellationToken` is installed at the start of every
/// `step`, so a handle obtained before a given step still cancels it (the
/// handle always points at whichever token is "current").
#[derive(Clone)]
pub struct CancelHandle(Arc<Mutex<CancellationToken>>);

impl CancelHandle {
    fn new() -> Self {
        CancelHandle(Arc::new(Mutex::new(CancellationToken::new())))
    }

    /// Installs and returns a fresh token, used for the step about to run.
    fn fresh_token(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut guard = self.0.lock().expect("cancel handle mutex poisoned");
        *guard = token.clone();
        token
    }

    /// Cancels whichever token is currently installed.
    pub fn cancel(&self) {
        let guard = self.0.lock().expect("cancel handle mutex poisoned");
        guard.cancel();
    }
}

/// Session-wide configuration: budgets, retry cap, and sandbox root.
pub struct SessionConfig {
    pub budgets: Budgets,
    pub retries: u32,
    pub root: SandboxRoot,
}

/// A session: persistent workspace store, session log, model backend,
/// augmenters, tools, tokenizer, and config. Owns a sequence of skill
/// activations (`Active<K>`).
pub struct Session<W> {
    pub(crate) store: Box<dyn StateStore<W>>,
    pub(crate) log: Arc<dyn SessionLog>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) augmenters: Vec<(Box<dyn crate::augmenter::Augmenter<W>>, TokenBudget)>,
    pub(crate) tools: ToolSet,
    pub(crate) tokenizer: Box<dyn Tokenizer>,
    pub(crate) config: SessionConfig,
    pending_observation: Option<Observation>,
    /// (tool name, args, output) of the last `TOOL_CALL_HISTORY` committed
    /// tool calls, most-recent-last; used to warn the model when a call
    /// recurs identically, whether consecutively or in an alternating
    /// pattern (e.g. A/B/A/B).
    recent_tool_calls: VecDeque<(String, Value, String)>,
    /// Per-path count of consecutive read-only tool calls targeting that
    /// path since the last mutating call (to any path) or the last mutating
    /// call naming that path. Read-without-edit guard: see `READ_ONLY_TOOLS`
    /// / `MUTATING_TOOLS`.
    read_streaks: HashMap<String, u32>,
    /// Reads of the committed path (see `Commitment`) since the commitment
    /// was set or last changed. Reset to 0 whenever `last_commitment`
    /// changes (including going from `Some` to `None` or vice versa).
    commitment_reads: u32,
    /// The commitment as of the end of the previous step, used to detect a
    /// change (which resets `commitment_reads`) and to note in the
    /// observation when a mutating call doesn't match it.
    last_commitment: Option<Commitment>,
    /// The most recent `shell`/`test` call that completed successfully
    /// (normalized command + output summary), used by the
    /// redundant-verification guard to reject an exact repeat run with
    /// nothing changed in between. Reset to `None` on `activate`/
    /// `activate_forced`.
    last_verify: Option<VerifyRun>,
    /// Whether anything has mutated the workspace (an `edit_file`/
    /// `write_file` call executed) since `last_verify` was last set. A
    /// `shell`/`test` repeat is only rejected while this is `false`.
    mutated_since_verify: bool,
    /// Consecutive read-only calls (any tool in `READ_ONLY_TOOLS`) executed
    /// with an EMPTY raw patch and no commitment set. A read whose patch
    /// writes anything resets this to 0 instead of incrementing it -- only a
    /// look that records nothing counts. Also reset to 0 after any executed
    /// mutating call, on activation, and whenever the commitment changes
    /// from `None` to `Some`. Gated by `Skill::silent_read_cap`.
    silent_reads: u32,
    /// Redundant-identical-read guard: every successfully executed read-only
    /// call (any tool in `READ_ONLY_TOOLS` except `filter_last_tool_call`,
    /// which by design re-reads a previous output) is keyed by
    /// `read_call_key` (tool name + canonical JSON of its args) and recorded
    /// here with the step it ran at. An identical call is rejected while its
    /// key is still present -- nothing has changed, so it would return the
    /// same output already in the log. Cleared on any successfully executed
    /// mutating call (`MUTATING_TOOLS`) and on activation, since either may
    /// have changed what a repeat of that call would return.
    reads_since_mutation: HashMap<String, u64>,
    last_prompt_tokens: Option<usize>,
    last_usage: Option<Usage>,
    failures: u32,
    /// The `reason` of the most recent rejection, kept so `reject` can tell
    /// the model when it has repeated the exact same mistake. Cleared on
    /// any successful commit (alongside `failures`).
    last_rejection_reason: Option<String>,
    cancel: CancelHandle,
    last_activation: Option<(String, Vec<String>)>,
    /// Configured child runner (milestone 9). `None` means this session
    /// cannot delegate; `Action::Delegate` is then always rejected.
    child_runner: Option<Arc<dyn ChildRunner>>,
    /// Number of delegations issued so far, used to number agent paths
    /// (`"<parent_skill>/<child>#<n>"`).
    delegations: u64,
    /// Context maintenance (statie-spec.md "9. Context maintenance"):
    /// `None` (the default) means no behaviour change from before this
    /// feature existed. When set, called after every committed `Tool` step
    /// and after `AskUser`/`Yield` parks -- never on a rejection or after
    /// `Done` -- to distil the step's dropped reasoning into the
    /// harness-owned `notes` field.
    maintainer: Option<Arc<dyn Maintainer>>,
    /// Unit 43: the in-flight maintainer task (if any), spawned right after
    /// some earlier step committed and not yet awaited. `None` whenever no
    /// call is outstanding -- always true right after a drain, and before
    /// the first maintainer-eligible step of an activation. See
    /// `PendingMaintainer`'s doc for the "at most one in flight" invariant.
    pending_maintainer: Option<PendingMaintainer>,
    /// Unit 48 (D24: per-model behaviour is a config choice, not a code
    /// default): whether `spawn_maintainer`'s call is left in flight for the
    /// next eligible step to drain (`true`, unit 43's overlapped behaviour,
    /// the default) or drained immediately after it is spawned (`false`,
    /// the pre-unit-43 synchronous behaviour, same spawn/drain code path --
    /// no second implementation). Live bench evidence set the default: Qwen
    /// gained a third of its wall clock at unchanged landing with overlap on
    /// (builds 27, 29, 30), but gemma's refusal walks lost two to four runs
    /// on every such build (16, 15 -> 12, 13) with notes one step stale, so
    /// gemma configs turn this off while Qwen keeps it on.
    maintainer_overlap: bool,
    /// The `think` tool (statie-spec.md's `think` tool): `None` means
    /// `think` is registered (if the active tool set includes it) but every
    /// call reports itself as unconfigured, telling the model to decide
    /// from what it has instead of parking on a call that can never
    /// resolve. When set, `Active::execute`'s `Action::Tool` interception
    /// for `name == "think"` calls it directly instead of dispatching to
    /// `self.session.tools` (`think` is not run by the registry -- see
    /// `statie_tools::Think`).
    thinker: Option<Arc<dyn Thinker>>,
    /// The `try_fixes` tool's proposer backend and verification primitive
    /// (BACKLOG.md item 4): `None` means `try_fixes` is registered (if the
    /// active tool set includes it) but every call reports itself as
    /// unconfigured, the same as an unconfigured `thinker` does for
    /// `think`. When set, `Active::execute`'s `Action::Tool` interception
    /// for `name == "try_fixes"` calls `fixer.propose` and verifies each
    /// candidate through `runner` directly, instead of dispatching to
    /// `self.session.tools` (`try_fixes` is not run by the registry -- see
    /// `statie_tools::TryFixes`). `runner`/`default_k`/`try_fixes_timeout`/
    /// `try_fixes_output_cap` are only meaningful once `fixer` is `Some`
    /// (see `with_fixer`); `runner` defaults to `None` even when `fixer` is
    /// set until `with_fixer` supplies one, in which case `try_fixes`
    /// reports the same "not configured" shape as no fixer at all (there is
    /// nothing to verify candidates with).
    fixer: Option<Arc<dyn Fixer>>,
    /// The verification primitive for `try_fixes` (see `fixer::
    /// CandidateRunner`'s doc for why this is a trait rather than a direct
    /// dependency on `statie-tools`). Set by `with_fixer` alongside
    /// `fixer`.
    candidate_runner: Option<Arc<dyn CandidateRunner>>,
    /// How many candidates a bare `try_fixes {}` call (no `k` in its args)
    /// asks the fixer for. Set by `with_fixer`.
    default_k: u32,
    /// Timeout for each candidate's verification run (`CandidateRunner::
    /// run`), mirroring the `test`/`shell` tools' own `shell_timeout_secs`.
    /// Set by `with_fixer` from the same tools config `cli/src/main.rs`
    /// builds `statie_tools::default_tools` from, since `SessionConfig`
    /// carries no shell timeout of its own for the session to read.
    try_fixes_timeout: Duration,
    /// Output cap (bytes) for each candidate's verification run, mirroring
    /// the `test`/`shell` tools' own `max_output_bytes`. Set by `with_fixer`
    /// the same way as `try_fixes_timeout`.
    try_fixes_output_cap: usize,
    /// A copy of the current step's rendered prompt (`system()`/`user()`),
    /// kept so a `think` call made during this step can hand the reasoning
    /// subagent exactly what the worker itself saw. Set once per step, right
    /// after `BoundedPrompt` is built; cheap clone of two already-rendered
    /// strings.
    last_prompt: Option<(String, String)>,
    /// Working-set config (statie-spec.md "10. Working set"). Defaults to
    /// `WorkingSetConfig::default()` (on: 4 windows, 6 recent-call lines);
    /// `max_windows: 0` (via `with_working_set`) disables the section and
    /// restores pre-working-set behaviour.
    working_set: WorkingSetConfig,
    /// The step of each window rendered into the working set the last time
    /// `Active::step` built one (empty before the first step, or whenever
    /// `WorkingSet::from_log` returned `None`/rendered no windows). Item 19's
    /// redundant-read guard consults this: a repeat read whose earlier
    /// window's step is NOT in this set has been evicted from what the model
    /// can currently see and must run again, rather than being refused as a
    /// no-op. Updated once per step, right after the working set is built,
    /// the same way `last_prompt` is.
    last_window_steps: BTreeSet<u64>,
    /// Rule-audit switches (statie-spec.md "10. After it works: the rule
    /// audit", milestone 12). Defaults to `Guards::default()` (all `true`
    /// except `done_stays_done`, off by default since the first bench
    /// battery's audit -- see `core/src/guards.rs`).
    guards: Guards,
    /// Green checkpoint (statie-spec.md "10. Working set", "Green
    /// checkpoint"): every file the activation had edited as of the last
    /// step at which `test` reported `result: pass`, snapshotted from the
    /// sandbox. `None` until the first pass. Kept in memory only (C2:
    /// never persisted to the store) and deliberately carried across a
    /// `Handoff` within the same `Session` (e.g. implement -> review may
    /// hand back to implement) rather than cleared on activation -- a
    /// fresh activation starting mid-session still has a green state worth
    /// protecting until a new one replaces it.
    green_checkpoint: Option<crate::green::GreenCheckpoint>,
    /// Whether the most recently completed `test` tool call (own entries
    /// only) reported `result: pass`. `None` before any `test` call has
    /// run. Updated once, right when the `test` ToolOutput is produced, so
    /// the park-time check (`Session::checkpoint_is_stale`) never rescans
    /// the log.
    last_test_result: Option<bool>,
    /// Repeated-failure guard: the normalized failure summary (see
    /// `normalize_failure_summary`) of the most recent FAILING `test` call.
    /// `None` before any failing `test` call has run since activation, and
    /// cleared back to `None` whenever a `test` call passes -- a pass means
    /// there is nothing to compare the next failure against. Reset to
    /// `None` on `activate`/`activate_forced`, because this copy only backs
    /// `repeated_failure_count` (a per-activation streak counter) and the
    /// repeated-failure prefix/log entry -- unlike the harness-owned,
    /// persisted `HarnessState::last_failure_summary` it is mirrored into
    /// (capped at 600 characters) at the same `test` call site, which is a
    /// fact about the sandbox and deliberately does NOT reset on
    /// activation. The two are allowed to diverge across a re-activation
    /// for exactly this reason -- `execute_try_fixes` reads the harness
    /// copy, not this one, precisely so it survives a re-activation. See
    /// `HarnessState::last_failure_summary`'s doc.
    last_test_failure: Option<String>,
    /// How many consecutive failing `test` calls (including the current
    /// one) have shared `last_test_failure`'s summary. 0 when
    /// `last_test_failure` is `None` (including right after a pass); always
    /// at least 1 whenever `last_test_failure` is `Some`. Reset alongside
    /// `last_test_failure`.
    repeated_failure_count: u32,
    /// Whether an `edit_file`/`write_file` call has executed successfully
    /// since the most recent `test` call (of either verdict). Set `true` by
    /// the same call site that sets `mutated_since_verify = true`; set back
    /// to `false` on every `test` call (pass or fail), mirroring
    /// `mutated_since_verify`'s reset on a `shell`/`test` call -- but kept
    /// as a separate flag because `mutated_since_verify` is already cleared
    /// by the time `note_test_result` runs (see that call site's comment).
    /// Reset to `false` on `activate`/`activate_forced`.
    mutated_since_test: bool,
    /// Repeated-`Continue`-note guard: the note text of the most recent
    /// `Action::Continue` step (see `Active::execute`'s handling of that
    /// variant), kept so the next `Continue` step can tell whether it
    /// produced the SAME note again. `None` initially, on activation, and
    /// after any step whose outcome is not `Outcome::Continue` produced by
    /// `Action::Continue` -- i.e. any other outcome breaks the streak, the
    /// same as a different note would.
    last_continue_note: Option<String>,
    /// How many consecutive steps have produced `last_continue_note`
    /// (including the one that set it, so this is always >= 1 whenever
    /// `last_continue_note` is `Some`). Reset to 0 alongside
    /// `last_continue_note` -- see that field's doc. At
    /// `CONTINUE_REPEAT_CAP`, the step that would make the note recur a
    /// third time parks instead of continuing (`Active::execute`'s
    /// `Action::Continue` arm).
    continue_repeat_count: u32,
    /// Repeated-skip guard: the `(tool, call key)` (see `read_call_key`) of
    /// the most recent `skipped_redundant` step (7.7/7.8's "do NOT run the
    /// tool" path in `Active::execute`), kept so the next skip can tell
    /// whether it is the SAME call being skipped again. `None` initially,
    /// on activation, and after any step that isn't itself a skip AND isn't
    /// a `think` step (`is_think_action`, unit 23's correction) -- i.e. an
    /// executed non-`think` tool call, a rejection, or any other outcome
    /// breaks the streak, the same as a different skipped call would, but a
    /// `think` step (model-chosen or substituted by
    /// `implement_repeated_skip_thinks`) leaves the streak exactly as it
    /// was: a think between two identical skips must not reset this, or the
    /// model can alternate skip/think forever without ever reaching
    /// `CONTINUE_REPEAT_CAP`. A `skipped_redundant` observation is the same
    /// kind of non-progress as a repeated `Continue` note (see
    /// `last_continue_note`) but wasn't counted before this guard existed,
    /// letting a run repeat the same skip dozens of times.
    last_skip_key: Option<(String, String)>,
    /// How many consecutive steps have produced `last_skip_key` (including
    /// the one that set it, so this is always >= 1 whenever `last_skip_key`
    /// is `Some`). Reset to 0 alongside `last_skip_key` -- including the
    /// think exemption: a `think` step leaves this count exactly as it was.
    /// At `CONTINUE_REPEAT_CAP`, the step that would make the skip recur a
    /// third time (thinks don't count towards this) parks instead of
    /// continuing (`Active::execute`'s skip handling), the same as
    /// `continue_repeat_count` does for `Continue`.
    skip_repeat_count: u32,
    /// Repeated-tool-error guard: the `(tool name, error_shape(reason))`
    /// (unit 25: the SHAPE of the reason, not its exact text -- see
    /// `error_shape`) of the most recent `tool_error` step, kept so the
    /// next tool error can tell whether it is the SAME KIND of error
    /// recurring. `None` initially, on activation, and after any step that
    /// is not itself an identical-shape tool error AND is not a `think`
    /// step (`is_think_action`, unit 23's correction) -- i.e. a successful
    /// non-`think` tool call, a different-shape tool error, a rejection, or
    /// any other outcome breaks the streak, the same as `last_skip_key`
    /// does for a skip, but a `think` step (model-chosen or substituted by
    /// `implement_repeated_error_thinks`) leaves the streak exactly as it
    /// was: without this exemption the model could alternate
    /// error/think/error/think forever (the substituted think would itself
    /// clear the streak, making every repeat look like a fresh occurrence)
    /// and never reach `CONTINUE_REPEAT_CAP`. A repeated identical-SHAPE tool
    /// error is the same kind of non-progress as a repeated skip or
    /// `Continue` note: the model keeps retrying a call whose result cannot
    /// change without a different action.
    last_tool_error_key: Option<(String, String)>,
    /// How many consecutive steps have produced `last_tool_error_key`
    /// (including the one that set it, so this is always >= 1 whenever
    /// `last_tool_error_key` is `Some`). Reset to 0 alongside
    /// `last_tool_error_key` -- including the think exemption: a `think`
    /// step leaves this count exactly as it was. At `CONTINUE_REPEAT_CAP`,
    /// the step that would make the error recur a third time (thinks don't
    /// count towards this) parks through the mixed-shape non-progress path
    /// instead of continuing, the same as `skip_repeat_count` does for a
    /// skip -- see the tool-error handling right after the repeated-skip
    /// guard in `Active::step`.
    tool_error_repeat_count: u32,
    /// Mixed-shape non-progress guard (`NON_PROGRESS_CAP`): count of
    /// consecutive committed steps whose outcome was a `Continue` conversion
    /// OR a skipped-redundant call, regardless of whether consecutive
    /// occurrences share a note/call key -- unlike `continue_repeat_count`/
    /// `skip_repeat_count`, which only track a streak of the SAME shape.
    /// Reset to 0 by an executed tool call, a park, a handoff, `done`, or
    /// activation. See `non_progress_shapes` for the human-readable
    /// description of each step in the current streak.
    non_progress_count: u32,
    /// One short human-readable description per step in the current
    /// `non_progress_count` streak, in order (e.g. "skipped test", "empty
    /// ask_user", "done with an open step"), used to build the park message
    /// and the `non_progress_cap` log entry when the cap fires. Cleared
    /// alongside `non_progress_count`.
    non_progress_shapes: Vec<String>,
    /// Unit 25 part 3: the path of the current "stalled on P" streak (see
    /// `HarnessState::last_stall`), mirrored in-memory the same way
    /// `last_skip_key`/`last_tool_error_key` mirror their own harness
    /// fields. `None` initially, on activation, and after any executed
    /// (non-error, non-skip, non-think) step -- i.e. a successful tool call
    /// on ANY path breaks the streak, not just one on a different path: the
    /// spec's "no executed ... step has happened since" is unconditional,
    /// not scoped to the stalled path itself.
    last_stall_path: Option<String>,
    /// Consecutive count of skip-or-edit-error events on `last_stall_path`
    /// with no executed step since (including the one that set it, so this
    /// is always >= 1 whenever `last_stall_path` is `Some`). Reset to 0
    /// alongside `last_stall_path`. At `count >= 6`, `Active::step` parks
    /// through `park_non_progress_cap` with shape "stalled on {path}".
    stall_count: u32,
}

impl<W> Session<W> {
    pub fn new(
        store: Box<dyn StateStore<W>>,
        log: Arc<dyn SessionLog>,
        model: Arc<dyn Model>,
        tools: ToolSet,
        tokenizer: Box<dyn Tokenizer>,
        config: SessionConfig,
    ) -> Self {
        Session {
            store,
            log,
            model,
            augmenters: Vec::new(),
            tools,
            tokenizer,
            config,
            pending_observation: None,
            recent_tool_calls: VecDeque::new(),
            read_streaks: HashMap::new(),
            commitment_reads: 0,
            last_commitment: None,
            last_verify: None,
            mutated_since_verify: false,
            silent_reads: 0,
            reads_since_mutation: HashMap::new(),
            last_prompt_tokens: None,
            last_usage: None,
            failures: 0,
            last_rejection_reason: None,
            cancel: CancelHandle::new(),
            last_activation: None,
            child_runner: None,
            delegations: 0,
            maintainer: None,
            pending_maintainer: None,
            maintainer_overlap: true,
            thinker: None,
            fixer: None,
            candidate_runner: None,
            default_k: 3,
            try_fixes_timeout: Duration::from_secs(120),
            try_fixes_output_cap: 200_000,
            last_prompt: None,
            working_set: WorkingSetConfig::default(),
            last_window_steps: BTreeSet::new(),
            guards: Guards::default(),
            green_checkpoint: None,
            last_test_result: None,
            last_test_failure: None,
            repeated_failure_count: 0,
            mutated_since_test: false,
            last_continue_note: None,
            continue_repeat_count: 0,
            last_skip_key: None,
            skip_repeat_count: 0,
            last_tool_error_key: None,
            tool_error_repeat_count: 0,
            non_progress_count: 0,
            non_progress_shapes: Vec::new(),
            last_stall_path: None,
            stall_count: 0,
        }
    }

    /// Attaches a context-maintenance backend (statie-spec.md "9. Context
    /// maintenance"). Without one, `notes` is never written and the step
    /// loop behaves exactly as it did before this feature existed.
    pub fn with_maintainer(mut self, m: Arc<dyn Maintainer>) -> Self {
        self.maintainer = Some(m);
        self
    }

    /// Unit 48 (D24: per-model behaviour is a config choice, not a code
    /// default): sets whether the maintainer call spawned for a step is left
    /// overlapping the next step (`true`, the default -- unit 43's
    /// behaviour) or drained immediately after it is spawned (`false`, the
    /// synchronous behaviour of builds through 26, same spawn/drain code
    /// path). Has no effect without `with_maintainer`.
    pub fn with_maintainer_overlap(mut self, overlap: bool) -> Self {
        self.maintainer_overlap = overlap;
        self
    }

    /// Attaches the `think` tool's reasoning backend. Without one, a `think`
    /// call still commits (never rejected) but its tool output tells the
    /// model `think` is not configured.
    pub fn with_thinker(mut self, t: Arc<dyn Thinker>) -> Self {
        self.thinker = Some(t);
        self
    }

    /// Attaches the `try_fixes` tool's proposer backend and verification
    /// primitive (BACKLOG.md item 4). Without a call, `try_fixes` still
    /// commits (never rejected) but its tool output tells the model
    /// `try_fixes` is not configured, the same as `think` without a
    /// `with_thinker` call. `default_k` is how many candidates a bare
    /// `try_fixes {}` (no `k` in its args) asks for.
    pub fn with_fixer(mut self, f: Arc<dyn Fixer>, default_k: u32) -> Self {
        self.fixer = Some(f);
        self.default_k = default_k;
        self
    }

    /// Supplies the verification primitive `try_fixes` runs each candidate
    /// through (`fixer::CandidateRunner`). Without a call, `try_fixes`
    /// reports itself unconfigured even when `with_fixer` attached a
    /// proposer -- there is nothing to verify a candidate with. Callers
    /// wire this from `statie_tools::worktree::WorktreeRunner` (see
    /// `cli/src/main.rs`).
    pub fn with_candidate_runner(mut self, r: Arc<dyn CandidateRunner>) -> Self {
        self.candidate_runner = Some(r);
        self
    }

    /// Overrides the timeout/output cap `try_fixes` uses for each
    /// candidate's verification run. Without a call, these default to the
    /// same values `statie_tools::ToolConfig::default()` gives `shell`/
    /// `test` (120s, 200_000 bytes) -- callers that build a non-default
    /// `ToolConfig` should call this with the same values so `try_fixes`
    /// matches the `test` tool's own timeout/cap.
    pub fn with_try_fixes_limits(mut self, timeout: Duration, output_cap: usize) -> Self {
        self.try_fixes_timeout = timeout;
        self.try_fixes_output_cap = output_cap;
        self
    }

    /// Configures the working set (statie-spec.md "10. Working set").
    /// Without a call, the session runs with `WorkingSetConfig::default()`
    /// (on). Pass `WorkingSetConfig { max_windows: 0, recent_calls: 0 }` to
    /// disable the section and restore pre-working-set behaviour.
    pub fn with_working_set(mut self, cfg: WorkingSetConfig) -> Self {
        self.working_set = cfg;
        self
    }

    /// Configures the rule-audit switches (statie-spec.md "10. After it
    /// works: the rule audit", milestone 12). Without a call, the session
    /// runs with `Guards::default()` (all rules on except `done_stays_done`,
    /// off by default -- see `core/src/guards.rs`).
    pub fn with_guards(mut self, guards: Guards) -> Self {
        self.guards = guards;
        self
    }

    pub fn with_augmenter(
        mut self,
        augmenter: Box<dyn crate::augmenter::Augmenter<W>>,
        budget: TokenBudget,
    ) -> Self {
        self.augmenters.push((augmenter, budget));
        self
    }

    /// Attaches the runner used to execute `Action::Delegate`. Without one,
    /// every delegation is rejected with an observation naming the (empty)
    /// set of configured children.
    pub fn with_child_runner(mut self, runner: Arc<dyn ChildRunner>) -> Self {
        self.child_runner = Some(runner);
        self
    }

    /// Sole constructor of `Active`. Checks the activation-transition rule
    /// against the session's remembered last activation, then initializes
    /// the store if empty, or updates `activation`/`active_tools` if not.
    pub async fn activate<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        self.check_transition(skill.name())?;
        self.activate_inner(skill).await
    }

    /// Same as `activate`, but skips the transition-allowed check.
    pub async fn activate_forced<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        self.activate_inner(skill).await
    }

    fn check_transition(&self, to: &str) -> Result<(), SessionError> {
        if let Some((prev, allowed)) = &self.last_activation
            && prev != to
            && !allowed.iter().any(|a| a == to)
        {
            return Err(SessionError::TransitionNotAllowed {
                from: Some(prev.clone()),
                to: to.to_string(),
            });
        }
        Ok(())
    }

    async fn activate_inner<K: Skill<Workspace = W> + ?Sized>(
        &mut self,
        skill: Arc<K>,
    ) -> Result<Active<'_, K>, SessionError>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let name = skill.name().to_string();

        match self.store.load().await? {
            None => {
                let initial = CommittedState {
                    harness: HarnessState {
                        budgets: self.config.budgets,
                        activation: name.clone(),
                        active_tools: skill.tools(),
                        inbox: Vec::new(),
                        step: 0,
                        last_overflow: None,
                        pinned: None,
                        green_step: None,
                        green_filtered: false,
                        tool_calls: 0,
                        last_test_cmd: None,
                        edits_since_test: 0,
                        last_think_step: None,
                        repeated_failure_step: None,
                        try_fixes_available: self.fixer.is_some(),
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
                    },
                    agent: skill.initial_workspace(),
                };
                self.store.init(initial).await?;
            }
            Some((cur, _version)) => {
                let mut harness = cur.harness;
                harness.activation = name.clone();
                harness.active_tools = skill.tools();
                // The config file, not the persisted snapshot, owns budgets.
                harness.budgets = self.config.budgets;
                // Reset per-activation: this activation has not yet run any
                // tool call (see `HarnessState::tool_calls`'s doc).
                harness.tool_calls = 0;
                // Mirrors `tool_calls`: this activation has not yet run a
                // `think` call either (see `HarnessState::last_think_step`'s
                // doc).
                harness.last_think_step = None;
                // Mirrors `thinker`/`last_think_step`: whether a fixer is
                // attached to THIS session, refreshed every activation the
                // same way `active_tools`/`budgets` are (the config, not the
                // persisted snapshot, owns it).
                harness.try_fixes_available = self.fixer.is_some();
                // Mirrors `last_skip_key`/`skip_repeat_count`'s own reset
                // just below: a new activation starts the repeated-skip
                // guard fresh.
                harness.last_skip = None;
                // Mirrors `last_tool_error_key`/`tool_error_repeat_count`'s
                // own reset just below (unit 23 part (b)): a new activation
                // starts the repeated-tool-error guard fresh, the same
                // treatment `last_skip` gets just above.
                harness.last_tool_error = None;
                // Mirrors `last_stall_path`/`stall_count`'s own reset just
                // below (unit 25 part 3): a new activation starts the
                // stalled-path guard fresh too.
                harness.last_stall = None;
                // Unit 27: `fails_since_green` counts failures "in this
                // activation" (see its doc) -- a new activation starts the
                // no-green cap fresh, the same treatment `tool_calls`/
                // `last_think_step` get above.
                harness.fails_since_green = 0;
                // Deliberately NOT reset here: `last_failure_summary` is a
                // fact about the sandbox (the most recent `test` call's
                // result), not about this activation, so it survives
                // `activate`/`activate_forced` the same way `last_test_cmd`
                // does just above -- a re-activation must not erase the
                // record of a still-unresolved failing test, or
                // `execute_try_fixes`'s harness-owned "no failing test on
                // record" check would wrongly refuse every `try_fixes` call
                // made right after a re-activation with no intervening
                // `test`. See that field's doc (`core/src/version.rs`) and
                // `last_test_failure`'s doc just below for the session-local
                // copy, which DOES reset per-activation because it only
                // drives the repeated-failure counter, not this check.
                self.store.set_harness(harness).await?;
            }
        }

        self.last_activation = Some((name.clone(), skill.allowed_transitions()));
        self.last_verify = None;
        self.silent_reads = 0;
        self.reads_since_mutation.clear();
        self.last_test_failure = None;
        self.repeated_failure_count = 0;
        self.mutated_since_test = false;
        self.last_continue_note = None;
        self.continue_repeat_count = 0;
        self.last_skip_key = None;
        self.skip_repeat_count = 0;
        self.last_tool_error_key = None;
        self.tool_error_repeat_count = 0;
        self.non_progress_count = 0;
        self.non_progress_shapes.clear();
        self.last_stall_path = None;
        self.stall_count = 0;

        let entry = LogEntry {
            step: self.log.last_step(),
            kind: LogKind::System,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({"event": "activate", "skill": name}),
            agent: None,
        };
        self.log.append(entry)?;

        let spec = skill.spec();
        let mut schema = Schema::of::<StepOutput<<K::Patch as PatchStrategy<W>>::Patch>>();
        // Narrow the advertised action.type enum (and, transitively, the
        // "Exact shapes" examples SystemPrompt::for_harness prints from it)
        // to exactly what this skill's rules will accept, so a
        // grammar-constrained decoder can't be steered into producing an
        // action that's doomed to be rejected (e.g. `yield` in a skill that
        // forbids it).
        let mut allowed_types: Vec<&str> = vec!["tool"];
        if skill.allows_ask_user() {
            allowed_types.push("ask_user");
        }
        if !skill.allowed_delegates().is_empty() {
            allowed_types.push("delegate");
        }
        allowed_types.push("done");
        if skill.allows_yield() {
            allowed_types.push("yield");
        }
        restrict_action_types(&mut schema, &allowed_types);

        let mut active = Active {
            session: self,
            skill,
            spec,
            schema,
        };
        active.run_on_activate().await?;
        Ok(active)
    }

    pub async fn state(&self) -> Result<(CommittedState<W>, Version), SessionError> {
        self.store.load().await?.ok_or(SessionError::NoActivation)
    }

    /// Appends a user message to the inbox and logs it.
    pub async fn push_user(&mut self, text: String) -> Result<(), SessionError> {
        let (cur, _version) = self.store.load().await?.ok_or(SessionError::NoActivation)?;
        let mut harness = cur.harness;
        let next_id = harness.inbox.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        harness.inbox.push(InboxItem {
            id: next_id,
            text: text.clone(),
            acked: false,
        });
        self.store.set_harness(harness).await?;

        let entry = LogEntry {
            step: self.log.last_step(),
            kind: LogKind::User,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({"id": next_id, "text": text}),
            agent: None,
        };
        self.log.append(entry)?;
        Ok(())
    }

    pub fn log(&self) -> Arc<dyn SessionLog> {
        Arc::clone(&self.log)
    }

    pub async fn patch_log(&self) -> Result<Vec<LoggedPatch>, SessionError> {
        Ok(self.store.patch_log().await?)
    }

    /// Token count of the most recently built prompt, if any.
    pub fn last_prompt_tokens(&self) -> Option<usize> {
        self.last_prompt_tokens
    }

    /// Server-reported usage from the most recent model call, if any.
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Read-without-edit guard: updates the per-path consecutive-read
    /// streak for one completed tool call (`name`/`args`, taking the path
    /// from `args.path` when present), and returns the new count for that
    /// path when a read-guard note should fire (count >= threshold).
    ///
    /// A mutating tool (`write_file`, `edit_file`, `shell`, `test`) resets
    /// every path's counter, since a `shell` run may have changed the
    /// world; a mutating call naming a specific path is a special case of
    /// this (the whole map is cleared, which also clears that path). A
    /// read-only tool with no path in `args` (bare `search`, or `recall`,
    /// which has no `path` field at all) is ignored: there is nothing to
    /// key the counter on.
    fn note_tool_call_for_read_guard(&mut self, name: &str, args: &Value) -> Option<(String, u32)> {
        if MUTATING_TOOLS.contains(&name) {
            self.read_streaks.clear();
            return None;
        }
        if !READ_ONLY_TOOLS.contains(&name) {
            return None;
        }
        let path = args.get("path").and_then(Value::as_str)?;
        let count = self.read_streaks.entry(path.to_string()).or_insert(0);
        *count += 1;
        let count = *count;
        if count >= READ_GUARD_THRESHOLD {
            Some((path.to_string(), count))
        } else {
            None
        }
    }

    /// Increments the committed `HarnessState::tool_calls` counter by one
    /// and persists it via `set_harness`, the same out-of-band commit
    /// pattern `execute`'s success path already uses for `green_step`/
    /// `last_overflow`. Called for every `Action::Tool` that actually
    /// executed -- the skipped-redundant path, the success path, and the
    /// tool-error path -- never for a step `reject`ed before it committed,
    /// and never for the "not active"/"unknown tool" guard rejections
    /// (nothing ran).
    async fn record_tool_call(&mut self) -> Result<(), SessionError> {
        let (cur, _version) = self.state().await?;
        let mut harness = cur.harness;
        harness.tool_calls += 1;
        self.store.set_harness(harness).await?;
        Ok(())
    }

    /// Green checkpoint (statie-spec.md "10. Working set", "Green
    /// checkpoint"): called right after a `test` ToolOutput is logged, with
    /// its output text. Updates `last_test_result`; on a pass, snapshots
    /// every path this activation has successfully `edit_file`/
    /// `write_file`'d so far (from the log, same derivation
    /// `working_set.rs` uses), replacing any earlier checkpoint, and
    /// returns the `green_checkpoint` System entry plus the observation
    /// prefix to prepend -- both `None` when the run was not a pass (the
    /// caller still gets `last_test_result` updated).
    fn note_test_result(&mut self, step: u64, output: &str) -> Option<(LogEntry, String)> {
        let pass = crate::green::is_pass(output);
        self.last_test_result = Some(pass);
        let mutated = self.mutated_since_test;
        self.mutated_since_test = false;

        if !pass {
            let summary = normalize_failure_summary(output);
            if self.last_test_failure.as_deref() == Some(summary.as_str()) {
                self.repeated_failure_count += 1;
            } else {
                self.last_test_failure = Some(summary);
                self.repeated_failure_count = 1;
            }

            if self.repeated_failure_count < 2 {
                return None;
            }

            let count = self.repeated_failure_count;
            let payload = serde_json::json!({
                "step": step,
                "count": count,
                "mutated_since_previous": mutated,
            });
            let entry = system_entry(step, "repeated_failure", payload);
            let prefix = if mutated {
                format!(
                    "note: this failure is identical to the previous test run's ({count} runs \
                     in a row); your edit(s) since did not change it. Before editing again, \
                     read the exact lines the error points to and fix that, not something \
                     else.\n\n"
                )
            } else {
                format!(
                    "note: this failure is identical to the previous test run's ({count} runs \
                     in a row) and nothing was edited in between. Read the exact lines the \
                     error points to, then fix that.\n\n"
                )
            };
            return Some((entry, prefix));
        }

        self.last_test_failure = None;
        self.repeated_failure_count = 0;

        let paths = crate::green::edited_paths(self.log.as_ref());
        let snap = crate::green::snapshot(&self.config.root, step, &paths);
        let all_paths: Vec<&str> = snap
            .checkpoint
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let mut payload = serde_json::json!({"step": step, "paths": all_paths});
        if !snap.skipped.is_empty()
            && let Value::Object(map) = &mut payload
        {
            map.insert(
                "skipped".to_string(),
                serde_json::to_value(&snap.skipped).unwrap_or(Value::Null),
            );
        }
        let entry = system_entry(step, "green_checkpoint", payload);
        self.green_checkpoint = Some(snap.checkpoint);

        let prefix = format!(
            "verification passed at step {step}; the files you edited are checkpointed. \
             Finish the plan's bookkeeping and hand off (`done` with `next_skill: review`); \
             cleanup beyond the task is review's call.\n\n"
        );
        Some((entry, prefix))
    }

    /// Green checkpoint: called wherever `Active::step`/`Active::execute`
    /// is about to return a parking outcome (`Park::Failure`/
    /// `Park::AskUser`/`Park::Yield`), a handoff, or `Done`. Restores the
    /// checkpointed files to the sandbox when a checkpoint exists AND the
    /// most recent `test` result is not a pass (a fail, or no `test` since
    /// activation -- anything other than `Some(true)`); a park while the
    /// checkpoint's own pass is still the most recent result needs no
    /// restore (disk already matches it). Returns the `restored_green`
    /// System entry and the sentence to append to the park/handoff message
    /// when a restore happened, `None` otherwise. Never restores on
    /// `Outcome::Continue` -- callers only invoke this at the sites listed
    /// above.
    fn maybe_restore_green(&mut self, step: u64) -> Option<(LogEntry, String)> {
        if self.last_test_result == Some(true) {
            return None;
        }
        let checkpoint = self.green_checkpoint.as_ref()?;

        // A path `edit_file`/`write_file`'d for the first time AFTER the
        // checkpoint (so it never appears in `checkpoint.files` at all --
        // it didn't exist yet when the snapshot was taken) needs to be
        // deleted on restore too, the same as a path that WAS present at
        // checkpoint time but has since been removed from disk
        // (`content: None` in the checkpoint already covers that case).
        // Re-derive the full set of paths this activation has ever edited
        // and add an absent entry for any that the checkpoint doesn't
        // already know about.
        let checkpointed: std::collections::HashSet<&str> =
            checkpoint.files.iter().map(|f| f.path.as_str()).collect();
        let mut effective = checkpoint.clone();
        for path in crate::green::edited_paths(self.log.as_ref()) {
            if !checkpointed.contains(path.as_str()) {
                effective.files.push(crate::green::CheckpointFile {
                    path,
                    content: None,
                });
            }
        }

        let restored_paths = crate::green::restore(&self.config.root, &effective);
        let from_step = checkpoint.step;
        let entry = system_entry(
            step,
            "restored_green",
            serde_json::json!({"from_step": from_step, "paths": restored_paths}),
        );
        let sentence = format!(
            "Restored the files to the state where the test passed at step {from_step}; the \
             edits since are in the log."
        );
        Some((entry, sentence))
    }
}

/// An active skill activation: the sole way to call `step`. Only
/// `Session::activate`/`activate_forced` construct one (A8).
pub struct Active<'a, K: Skill + ?Sized> {
    session: &'a mut Session<K::Workspace>,
    skill: Arc<K>,
    spec: Arc<crate::skill::SkillSpec>,
    schema: Schema,
}

impl<'a, K: Skill + ?Sized> Active<'a, K>
where
    K::Workspace: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    /// The step of the most recent own (non-agent) `ToolOutput` log entry
    /// at or before `step_at_start`, mirroring `WorkingSet::from_log`'s own
    /// `newest_output_step` -- that entry is THIS step's Observation
    /// section, never itself rendered as a working-set window. Item 19's
    /// eviction check (below) uses this instead of comparing `at_step` to
    /// `step_at_start` directly: an `after_tool`/maintainer hook can commit
    /// its own harness-only step in between a read and the next model
    /// turn, so the read's own step number can trail `step_at_start` by
    /// more than one even though its output is still what the model just
    /// saw. `None` when the log has no own `ToolOutput` entries yet.
    fn newest_tool_output_step(&self, step_at_start: u64) -> Option<u64> {
        self.session
            .log
            .entries(Some(LogKind::ToolOutput), READ_HEADER_LOOKUP_LIMIT)
            .into_iter()
            .filter(|e| e.agent.is_none() && e.step <= step_at_start)
            .map(|e| e.step)
            .max()
    }

    /// Looks up the `read_file` `ToolOutput` logged at `step` (as recorded
    /// in `Session::reads_since_mutation`) and parses its header, for 7.8's
    /// read-paging special case. `None` when no matching entry is found
    /// (should not happen in practice -- `reads_since_mutation` only ever
    /// points at a step whose call actually ran and logged a `ToolOutput`)
    /// or its header doesn't parse.
    fn previous_read_file_header(&self, step: u64) -> Option<crate::read_window::ReadWindowHeader> {
        let entries = self
            .session
            .log
            .entries(Some(LogKind::ToolOutput), READ_HEADER_LOOKUP_LIMIT);
        let entry = entries
            .iter()
            .rev()
            .find(|e| e.agent.is_none() && e.step == step)?;
        let output = entry.payload.get("output").and_then(Value::as_str)?;
        parse_read_header(output)
    }

    /// Green checkpoint (statie-spec.md "10. Working set", "Green
    /// checkpoint"): called at every site about to return a parking
    /// outcome, `Outcome::Done`, or `Outcome::Handoff` that carries no
    /// message of its own to append the restore sentence to (`Yield`,
    /// `Done`, `Handoff`). Restores the checkpoint into the sandbox and
    /// pushes the `restored_green` entry into `entries` when a restore was
    /// needed (checkpoint exists and the last `test` result is not a
    /// pass); appends the sentence to `pending_observation` instead of
    /// returning it, since none of these outcomes carry a string the
    /// caller could hand back. No-op (nothing pushed, nothing appended)
    /// when no restore was needed.
    fn restore_green_on_park(&mut self, step: u64, entries: &mut Vec<LogEntry>) {
        let Some((entry, sentence)) = self.session.maybe_restore_green(step) else {
            return;
        };
        entries.push(entry);
        let base = self
            .session
            .pending_observation
            .take()
            .map(|o| o.as_str().to_string())
            .unwrap_or_default();
        let text = if base.is_empty() {
            sentence
        } else {
            format!("{base}\n\n{sentence}")
        };
        let (obs, _overflow) = Observation::truncate_with_report(
            &crate::promptable::ToolOutputFull::new(text),
            self.session.config.budgets.observation,
            self.session.tokenizer.as_ref(),
        );
        self.session.pending_observation = Some(obs);
    }

    /// Same restore as `restore_green_on_park`, for an outcome that DOES
    /// carry its own message string (`Park::AskUser`, `Park::Failure`):
    /// restores the checkpoint and pushes the `restored_green` entry the
    /// same way, then returns `message` with the sentence appended (or
    /// `message` unchanged when no restore was needed).
    fn restore_green_and_append(
        &mut self,
        step: u64,
        entries: &mut Vec<LogEntry>,
        message: String,
    ) -> String {
        let Some((entry, sentence)) = self.session.maybe_restore_green(step) else {
            return message;
        };
        entries.push(entry);
        format!("{message} {sentence}")
    }

    /// Mixed-shape non-progress guard's park path (`NON_PROGRESS_CAP`'s own
    /// block, immediately below in `step`, and the repeated-tool-error
    /// guard, which reaches the same park path early via
    /// `CONTINUE_REPEAT_CAP` whenever the SAME tool error recurs that many
    /// times): joins `self.session.non_progress_shapes` (already carrying
    /// this step's own shape, pushed by the caller before calling this),
    /// resets both non-progress-streak fields, logs the `non_progress_cap`
    /// System entry with the joined shapes, and returns the
    /// `Park::AskUser` message built from `pending_observation` plus the
    /// standard non-progress sentence -- restoring the green checkpoint
    /// into the message the same way every other park path here does.
    /// `count` is the number of steps being reported (the cap that was
    /// just reached: `NON_PROGRESS_CAP` for the mixed-shape guard,
    /// `CONTINUE_REPEAT_CAP` for an early tool-error-repeat park), so the
    /// message always states how many steps actually preceded it rather
    /// than a constant that may not match `shapes`. Factored out so the
    /// two callers produce byte-identical log entries/message shapes
    /// rather than two near-duplicate copies of this formatting.
    fn park_non_progress_cap(
        &mut self,
        step: u64,
        entries: &mut Vec<LogEntry>,
        count: u32,
    ) -> Outcome {
        let shapes = self.session.non_progress_shapes.join(", ");
        self.session.non_progress_count = 0;
        self.session.non_progress_shapes.clear();

        entries.push(system_entry(
            step,
            "non_progress_cap",
            serde_json::json!({"shapes": shapes}),
        ));

        let base = self
            .session
            .pending_observation
            .as_ref()
            .map(|o| o.as_str().to_string())
            .unwrap_or_default();
        let message = format!(
            "{base}\n\nThe model has sent {count} steps in a row without progress ({shapes}). \
             Reply with what it should do next."
        );
        let message = self.restore_green_and_append(step, entries, message);
        Outcome::Parked(Park::AskUser(message))
    }

    /// Green checkpoint, for callers that park outside `Active::step` (the
    /// driver's autonomous step cap in `tui/src/driver.rs`: hitting the cap
    /// while `Outcome::Continue` keeps coming back is itself a park the
    /// driver manufactures, not one `step`/`execute` ever sees, so none of
    /// `restore_green_on_park`/`restore_green_and_append` run for it).
    /// Restores the checkpoint into the sandbox exactly as
    /// `restore_green_and_append` does (checkpoint exists and the last
    /// `test` result is not a pass), appends the `restored_green` System
    /// entry directly to the session log the same way `apply_user_patch`
    /// appends its own out-of-step entries (there is no `StepRecord` for
    /// this call to return one inside), and returns the restore sentence.
    /// `Ok(None)` when no restore was needed (no checkpoint, or the last
    /// `test` already passed).
    pub async fn restore_green_if_stale(&mut self) -> Result<Option<String>, SessionError> {
        let (cur, _version) = self.session.state().await?;
        let step = cur.harness.step;
        let Some((entry, sentence)) = self.session.maybe_restore_green(step) else {
            return Ok(None);
        };
        self.session.log.append(entry)?;
        Ok(Some(sentence))
    }

    pub async fn step(
        &mut self,
        ev: Event,
        sink: &mut dyn StepSink,
    ) -> Result<StepRecord, SessionError> {
        let mut entries: Vec<LogEntry> = Vec::new();

        // 1. Handle the incoming event.
        match ev {
            Event::User(text) => {
                self.session.push_user(text).await?;
            }
            Event::Cancel => {
                self.session.cancel.cancel();
                let (cur, _version) = self.session.state().await?;
                let step = cur.harness.step;
                entries.push(system_entry(step, "cancelled", serde_json::json!({})));
                // Spec item 4: cancellation is a terminal outcome for this
                // `step` call -- drain any maintainer task still in flight
                // from an earlier step before returning, so it is never
                // left to outlive the session.
                self.drain_pending_maintainer(sink).await;
                return Ok(StepRecord::new(entries, Outcome::Parked(Park::Cancelled)));
            }
            Event::Tick => {}
        }

        // Fresh cancellation token for this step; no prompt has been built yet,
        // so a step that ends before calling the model reports no token count.
        let token = self.session.cancel.fresh_token();
        self.session.last_prompt_tokens = None;
        self.session.last_usage = None;

        // 2. Load state + version; terminal check.
        let (cur, version) = self.session.state().await?;
        let step_at_start = cur.harness.step;

        if self.skill.is_terminal(&cur.agent) {
            entries.push(system_entry(step_at_start, "done", serde_json::json!({})));
            // Spec item 4: `Done` is terminal -- drain any maintainer task
            // still in flight before returning it.
            self.drain_pending_maintainer(sink).await;
            return Ok(StepRecord::new(entries, Outcome::Done));
        }

        // 3. Run augmenters.
        let obs_ref = self.session.pending_observation.as_ref();
        let mut snippets: Vec<Snippet> = Vec::new();
        for (augmenter, budget) in &self.session.augmenters {
            let budget = TokenBudget(budget.0);
            let mut got = augmenter
                .augment(&cur.agent, obs_ref, budget, self.session.tokenizer.as_ref())
                .await;
            snippets.append(&mut got);
        }
        // Augmentation is derived, never load-bearing: clamp it to the
        // section budget by dropping trailing snippets rather than failing
        // the step. Per-augmenter budgets bound each source; this bounds the sum.
        let aug_cap = cur.harness.budgets.augmentation;
        while !snippets.is_empty()
            && self
                .session
                .tokenizer
                .count(&Augmentation::from_snippets(snippets.clone()).render())
                > aug_cap
        {
            snippets.pop();
        }
        let augmentation = if snippets.is_empty() {
            None
        } else {
            Some(Augmentation::from_snippets(snippets))
        };

        // 4. Build the bounded prompt.
        let caps = self.session.model.capabilities();
        let delegates = self.skill.allowed_delegates();
        let system_prompt = SystemPrompt::for_harness(
            &caps,
            <K::Patch as PatchStrategy<K::Workspace>>::name(),
            <K::Patch as PatchStrategy<K::Workspace>>::describe(),
            Some(&self.schema),
            &delegates,
        );
        let tool_list = self.session.tools.list_for(&cur.harness.active_tools);
        let agent_state = AgentState::new(&cur.agent);
        let current_commitment = self.skill.commitment(&cur.agent).map(|c| {
            let reads_left = COMMITMENT_READ_CAP.saturating_sub(self.session.commitment_reads);
            (c, reads_left)
        });
        let harness_view = HarnessView::new(&cur.harness).with_commitment(current_commitment);

        // Working set (statie-spec.md "10. Working set"): derived fresh
        // from the log every step (C2: derived, never persisted), so it
        // always reflects what's actually in the log, not the working set
        // computed on some earlier step.
        // Item 19: `self.session.last_window_steps` (consulted by the
        // redundant-read guard, 7.8 below, to tell an evicted window from
        // one still visible to the model) is set once the working set
        // actually rendered into the prompt is known -- after the item 44
        // eviction loop below, which may re-render this at a smaller
        // budget than `cur.harness.budgets.working_set` if the full-budget
        // rendering didn't fit the prompt.
        let mut working_set = WorkingSet::from_log(
            self.session.log.as_ref(),
            self.session.tokenizer.as_ref(),
            cur.harness.budgets.working_set,
            &self.session.working_set,
            step_at_start,
        );

        let notes = self.skill.notes(&cur.agent).map(Notes::new);
        // Unit 26 (situational rules): localised, per-step guidance derived
        // from harness-owned state, rendered as its own `## Now` section
        // right after the harness view. `None` (switch off, or the skill
        // has nothing to say this step) omits the section entirely --
        // identical to today's prompt. Its token count is not logged
        // separately: unlike `notes_tokens` (`StepSink::maintainer_done`),
        // there is no per-section breakdown in the worker step's own
        // usage-style payload to fold it into -- only the maintainer
        // subsystem (a separate call) logs a section's token count on its
        // own. `BoundedPrompt::tokens()` (`self.session.last_prompt_tokens`,
        // set below) already includes this section's contribution to the
        // total.
        let situational = self.skill.situational(&cur);
        // Clamp defensively before handing the observation to the builder:
        // `truncate` already bounds fresh tool output, but harness-authored
        // messages (`rejection`, `tool_aborted`, ...) are never truncated at
        // construction time and a long one (e.g. a tool error carrying
        // stderr, or an unknown-tool name) could otherwise exceed the
        // section budget and fail the step. The builder's per-section check
        // stays strict as the real invariant; this is what keeps it true.
        let mut pending_obs = self.session.pending_observation.take().map(|obs| {
            obs.clamp(
                cur.harness.budgets.observation,
                self.session.tokenizer.as_ref(),
            )
        });

        // Assembles the full prompt from the fixed sections captured above
        // plus the two elastic ones (`ws`/`obs`), so the eviction loop below
        // can rebuild with a smaller working set or a truncated observation
        // without repeating the fixed-section wiring.
        let assemble = |ws: Option<&WorkingSet>, obs: Option<&Observation>| {
            let mut builder = PromptBuilder::new(
                self.session.tokenizer.as_ref(),
                cur.harness.budgets,
                version,
            )
            .system(&system_prompt)
            .spec(&self.spec)
            .tools(&tool_list)
            .agent(&agent_state)
            .notes(notes.as_ref(), cur.harness.notes_step)
            .harness(&harness_view)
            .situational(situational.as_deref())
            .working_set(ws);
            if let Some(aug) = &augmentation {
                builder = builder.augmentation(aug);
            }
            if let Some(obs) = obs {
                builder = builder.observation(obs);
            }
            builder.build()
        };

        // Item 44: the harness owns the working set (the one elastic
        // section) and must never fail a step on a budget it can meet by
        // evicting from it. `build()` failing over budget doesn't
        // necessarily mean the fixed sections are too big -- it can mean
        // the working set (sized before the other sections were known) was
        // rendered larger than the room actually left for it.
        //
        // Live evidence: qwen_json_long_b26, five of ten runs ended with
        // `step failed: prompt section System over budget: 14005..14986 >
        // 14000` after item 40 added ~800 tokens of tool descriptions to
        // the system block, and the session parked on an error it could
        // have met by shrinking the one elastic section. Retry with a
        // smaller working-set budget (spec item 1), then -- once the
        // working set is at zero -- a truncated observation (spec item 2),
        // before giving up and returning the error as a real configuration
        // failure (spec item 3: the fixed sections alone exceed `total`).
        let mut ws_budget = cur.harness.budgets.working_set;
        let mut result = assemble(working_set.as_ref(), pending_obs.as_ref());
        while let Err(err) = &result {
            // Only a working set that was actually rendered (spec item 1:
            // "rendered with a non-zero budget") can be shrunk further --
            // `None` means the section is disabled (`WorkingSetConfig`
            // with both counts zero) or the log had nothing to show, and
            // re-rendering at a smaller budget would still produce `None`.
            if working_set.is_none() || ws_budget == 0 {
                break;
            }
            let overage = err.tokens - err.limit;
            let new_budget = ws_budget.saturating_sub(overage + 64);
            entries.push(system_entry(
                step_at_start,
                "working_set_shrunk",
                serde_json::json!({
                    "from": ws_budget,
                    "to": new_budget,
                    "overage": overage,
                }),
            ));
            ws_budget = new_budget;
            working_set = WorkingSet::from_log(
                self.session.log.as_ref(),
                self.session.tokenizer.as_ref(),
                ws_budget,
                &self.session.working_set,
                step_at_start,
            );
            result = assemble(working_set.as_ref(), pending_obs.as_ref());
        }
        // Working set is at zero (or was never rendered) and the prompt
        // still overflows: truncate the observation section once (spec
        // item 2 -- unlike the working set, this is a single shrink, not a
        // repeat-while-overflowing loop), reusing `Observation::clamp`'s
        // `truncate_with_report`-style handling rather than duplicating it.
        if let Err(err) = &result
            && let Some(obs) = &pending_obs
        {
            {
                let obs_tokens = self.session.tokenizer.count(&obs.render());
                let overage = err.tokens - err.limit;
                let new_budget = obs_tokens.saturating_sub(overage + 64);
                if new_budget < obs_tokens {
                    entries.push(system_entry(
                        step_at_start,
                        "observation_shrunk",
                        serde_json::json!({
                            "from": obs_tokens,
                            "to": new_budget,
                            "overage": overage,
                        }),
                    ));
                    pending_obs = Some(
                        obs.clone()
                            .clamp(new_budget, self.session.tokenizer.as_ref()),
                    );
                    result = assemble(working_set.as_ref(), pending_obs.as_ref());
                }
            }
        }
        self.session.last_window_steps = working_set
            .as_ref()
            .map(|ws| ws.window_steps().clone())
            .unwrap_or_default();
        let prompt = result?;
        self.session.last_prompt_tokens = Some(prompt.tokens());
        self.session.last_prompt = Some((prompt.system().to_string(), prompt.user().to_string()));

        // 5. Call the model.
        let raw: RawOutput = match self
            .session
            .model
            .step(prompt, &self.schema, sink, token.clone())
            .await
        {
            Ok(raw) => raw,
            Err(crate::error::ModelError::Cancelled) => {
                // The model never answered; keep the observation for the next attempt.
                self.session.pending_observation = pending_obs;
                entries.push(system_entry(
                    step_at_start,
                    "cancelled",
                    serde_json::json!({}),
                ));
                // Spec item 4: cancellation is terminal for this `step`
                // call -- drain any maintainer task still in flight before
                // returning it.
                self.drain_pending_maintainer(sink).await;
                return Ok(StepRecord::new(entries, Outcome::Parked(Park::Cancelled)));
            }
            Err(e) => return Err(SessionError::Model(e)),
        };

        // Recorded immediately after the model call, before parsing, so a
        // rejected output still counts toward real cost per model call.
        self.session.last_usage = raw.usage;
        let finish_reason = raw.finish_reason.clone();

        if let Some(reasoning) = &raw.reasoning {
            entries.push(LogEntry {
                step: step_at_start,
                kind: LogKind::Reasoning,
                ts: chrono::Utc::now().to_rfc3339(),
                payload: serde_json::json!({"text": reasoning.as_ref()}),
                agent: None,
            });
        }

        // 6. Parse. The raw content is kept for the log so a rejection can be
        // diagnosed from the session log alone (it never re-enters a prompt).
        let raw_content = raw.content.clone();
        let parsed: ParsedOutput<K::Workspace> =
            match ParsedOutput::parse_with_fixes::<K::Patch>(raw) {
                Ok((p, fixes)) => {
                    if !fixes.is_empty() {
                        entries.push(system_entry(
                            step_at_start,
                            "normalized",
                            serde_json::json!({"fixes": fixes}),
                        ));
                    }
                    p
                }
                Err(e) => {
                    return Ok(self.reject(
                        entries,
                        step_at_start,
                        e.to_string(),
                        &raw_content,
                        finish_reason.as_deref(),
                    ));
                }
            };

        // `HarnessState::consecutive_non_question_asks`: read from the
        // model's own raw parsed action, before `Skill::validate` (right
        // below) can rewrite it -- a skill converting a non-question
        // `ask_user` into `Action::Continue` (or the early-park trigger
        // converting it into `think`) must not hide the shape this field
        // exists to count. Used when `next_harness` is built (8, below).
        let raw_non_question_ask = matches!(
            parsed.action(),
            Action::AskUser { message } if !message.contains('?')
        );

        // 7. Validate.
        let validated: ValidatedStep<K::Workspace> = match self.skill.validate(&cur, parsed) {
            Ok(v) => v,
            Err(e) => {
                return Ok(self.reject(entries, step_at_start, e.to_string(), &raw_content, None));
            }
        };

        // 7.5. A patch that changes the handoff target must be paired with
        // action `done` in the same step: the skill instructions say "set
        // next_skill and send done", but a tool call alongside a changed
        // next_skill would otherwise silently hand off on the *next* step
        // (before the model even sees the tool's result), looping forever
        // in the wrong skill. Reject so the model gets a corrective
        // observation and the retry cap applies.
        let cur_target = self.skill.handoff(&cur.agent);
        let next_target = self.skill.handoff(validated.patch().next());
        if !matches!(validated.action(), Action::Done) && next_target != cur_target {
            return Ok(self.reject(
                entries,
                step_at_start,
                format!(
                    "changing `next_skill` (to {next_target:?}) requires action `done`; keep \
                     working with a tool call, or send `done` together with `next_skill` to \
                     hand off"
                ),
                &raw_content,
                None,
            ));
        }

        // 7.6. Commitment slot: a decision made in one step binds the next.
        // Evaluated against the *post-patch* workspace so a patch that
        // changes `next` in the same step it reads takes effect
        // immediately (the read below is then judged against the new
        // commitment, not the stale one). No commitment set -> no rule
        // (today's behaviour); `done`/`ask_user`/`yield`/`delegate` are
        // unaffected -- only `Action::Tool` is gated.
        let commitment = self.skill.commitment(validated.patch().next());
        if commitment != self.session.last_commitment {
            self.session.commitment_reads = 0;
            if self.session.last_commitment.is_none() && commitment.is_some() {
                self.session.silent_reads = 0;
            }
            self.session.last_commitment = commitment.clone();
        }
        // Set when this step's tool call is a mutating call matching the
        // commitment: after commit+execute, the commitment is spent (the
        // observation gets a "commitment fulfilled" prefix, and if the
        // model's own patch didn't already clear `next`, the harness clears
        // it with an extra commit). Set to the note text when this step's
        // tool call is a mutating call that does *not* match: the
        // observation gets a "note:" suffix reminding the model `next` is
        // stale, but `next` is left alone.
        let mut commitment_fulfilled: Option<Commitment> = None;
        let mut commitment_mismatch_note: Option<String> = None;
        // Set (guards.commitment_read_cap = false only) when this step's
        // tool call is a read while a commitment is set: appended to the
        // observation below instead of the cap rejecting/clearing it.
        let mut commitment_pending_note: Option<String> = None;
        if let (Action::Tool { name, args }, Some(c)) = (validated.action(), &commitment) {
            let is_read_only = READ_ONLY_TOOLS.contains(&name.as_str());
            let is_mutating = MUTATING_TOOLS.contains(&name.as_str());
            if is_read_only {
                let targets_committed_path = match name.as_str() {
                    "filter_last_tool_call" => true,
                    "read_file" | "search" | "list_files" => {
                        c.path.is_some()
                            && args.get("path").and_then(Value::as_str) == c.path.as_deref()
                    }
                    _ => false,
                };
                if !self.session.guards.commitment_read_cap {
                    // Off: reads against the committed path neither count
                    // toward the cap nor clear the commitment. A short
                    // factual note is appended to this step's observation
                    // (below, once the tool has actually run) instead of a
                    // rejection.
                    commitment_pending_note = Some(format!(
                        "note: `next` is still pending -- `{}` {} ({}).",
                        c.tool,
                        c.path.as_deref().unwrap_or("(no path)"),
                        c.what
                    ));
                } else if targets_committed_path
                    && self.session.commitment_reads < COMMITMENT_READ_CAP
                {
                    self.session.commitment_reads += 1;
                } else {
                    let path_desc = c.path.as_deref().unwrap_or("(no path)");
                    return Ok(self.reject(
                        entries,
                        step_at_start,
                        format!(
                            "you committed to `{} {}` ({}); make that call now, or change \
                             `next` (and say why in `recent_summary`) -- you have used {} of \
                             {COMMITMENT_READ_CAP} reads on {}",
                            c.tool, path_desc, c.what, self.session.commitment_reads, path_desc
                        ),
                        &raw_content,
                        None,
                    ));
                }
            } else if is_mutating {
                // `shell`/`test` commitments match on the tool name alone:
                // a shell command has no `path` argument, so comparing
                // `args.path` against `c.path` for these two tools recorded
                // the call that honoured the commitment as a mismatch
                // whenever the model had also written a `path` onto the
                // commitment (e.g. copied from a `grep`/`search` call that
                // itself carried no `path`). `edit_file`/`write_file` keep
                // the path comparison: those calls do touch one named file,
                // and the commitment is only fulfilled by the one it named.
                let matches_commitment = name == &c.tool
                    && (c.tool == "shell"
                        || c.tool == "test"
                        || c.path.is_none()
                        || args.get("path").and_then(Value::as_str) == c.path.as_deref());
                if matches_commitment {
                    commitment_fulfilled = Some(c.clone());
                } else {
                    let path_desc = c.path.as_deref().unwrap_or("(no path)");
                    commitment_mismatch_note = Some(format!(
                        "note: `next` still says {} {} ({}); update it if that is no longer \
                         the plan.",
                        c.tool, path_desc, c.what
                    ));
                }
            }
        } else if let (Action::Tool { name, .. }, None) = (validated.action(), &commitment)
            && self.session.guards.silent_reads
            && READ_ONLY_TOOLS.contains(&name.as_str())
            && is_empty_patch(validated.patch().raw())
            && let Some(cap) = self.skill.silent_read_cap()
            && self.session.silent_reads >= cap
        {
            // Guard 2: reads require a commitment (skill-driven). Only
            // applies when no commitment is set -- the commitment-slot rule
            // above already gates reads while one is -- and only to a read
            // whose patch is empty: a read that records something (a file
            // note, a plan step, `recent_summary`) is never counted.
            let k = self.session.silent_reads;
            return Ok(self.reject(
                entries,
                step_at_start,
                format!(
                    "{k} read(s) in a row that wrote nothing to the workspace. Write what you \
                     learned in this patch (a file note, a plan step, `recent_summary`) or the \
                     edit you will make (`next` = \
                     {{\"tool\":\"edit_file\",\"path\":\"<file>\",\"what\":\"<one line>\"}}; \
                     `next.tool` is never read_file), then read on. To find the exact spot, \
                     `search` (pattern + path) beats paging"
                ),
                &raw_content,
                None,
            ));
        }

        // 7.7. Redundant-verification guard: an exact repeat of the most
        // recently run `shell`/`test` command, with nothing mutated since,
        // gives the same result. Rather than rejecting the whole step (and
        // discarding a patch that may carry real progress alongside the
        // redundant call), the step commits normally and the call itself is
        // skipped at execution time -- see `SkippedCall` and the "do NOT
        // run the tool" handling in `execute`.
        let mut skip: Option<SkippedCall> = None;
        if self.session.guards.redundant_verify
            && let Action::Tool { name, args } = validated.action()
            && (name == "shell" || name == "test")
            && let Some(cmd) = extract_command(args)
            && let Some(last) = &self.session.last_verify
            && last.tool == *name
            && last.cmd == cmd
            && !self.session.mutated_since_verify
        {
            skip = Some(SkippedCall {
                reason: format!(
                    "nothing has changed since you last ran `{cmd}` (it returned: {}); \
                     running it again gives the same result. Make the change first with \
                     edit_file or write_file, or run a different command",
                    last.summary
                ),
                cached: last.summary.clone(),
            });
        }

        // 7.8. Redundant-identical-read guard: an exact repeat of a
        // read-only call already executed since the last mutation gives the
        // same output already in the log. Same treatment as 7.7: commit the
        // step and skip the call, don't reject it. Runs for every skill,
        // before the commit, regardless of whether a commitment is set
        // (that is a separate, narrower rule above). `filter_last_tool_call`
        // is exempt: it re-reads a previous output by design, so an
        // identical call to it is not redundant.
        //
        // Special case for `read_file` only: if the previous identical
        // call's output header shows a truncated window (`showing a-b` with
        // `b < total`), the model almost certainly wanted the rest of the
        // file, not to be told its call was redundant -- a live run asked
        // for the same default (untruncated-looking) window twelve times in
        // a row and never once passed `offset`. Rather than skip, page
        // forward: rewrite the call's `offset` to `b + 1` (keeping the
        // model's own `limit`, or the tool default if it gave none) and let
        // it run normally (see `PagedRead` and its use after the commit
        // below, and the "do NOT skip" handling in `execute`). A window
        // that already reached the end of the file (`b == total`), or whose
        // header can't be parsed, falls through to the ordinary skip.
        // Item 19: when windows are on, a repeat read is only truly
        // redundant if the earlier call's window is still rendered in the
        // working set the model just saw (`last_window_steps`, set right
        // above the prompt was built this step). One that's been evicted
        // is no longer visible to the model at all, so skipping it would
        // hide real information behind a refusal the model can't act on --
        // let it run normally instead, and note the re-run with a
        // `reread_evicted` System entry (pushed after commit, alongside
        // `paged_read`) rather than `skipped_redundant`.
        //
        // `at_step` equal to the most recent own `ToolOutput` step
        // (`newest_tool_output_step`) is a special case, not an omission in
        // `last_window_steps`: that's the read whose output is what the
        // model just saw as THIS step's Observation section (`WorkingSet::
        // from_log`'s `newest_output_step` deliberately excludes it from
        // the windows -- it's not repeated there because the Observation
        // section already showed it). Visible-via-Observation is still
        // visible, so this is not an eviction. Compared by looking up the
        // log directly rather than assuming `at_step == step_at_start - 1`:
        // an `after_tool`/maintainer hook can commit its own harness-only
        // step in between, advancing `step_at_start` past the read's own
        // step without producing a newer `ToolOutput`.
        let mut reread_evicted: Option<(String, u64)> = None;
        let mut paged: Option<PagedRead> = None;
        if skip.is_none()
            && self.session.guards.redundant_reads
            && let Action::Tool { name, args } = validated.action()
            && name != "filter_last_tool_call"
            && READ_ONLY_TOOLS.contains(&name.as_str())
        {
            let key = read_call_key(name, args);
            if let Some(&at_step) = self.session.reads_since_mutation.get(&key) {
                let evicted = self.session.working_set.max_windows > 0
                    && Some(at_step) != self.newest_tool_output_step(step_at_start)
                    && !self.session.last_window_steps.contains(&at_step);
                if evicted {
                    reread_evicted = Some((name.clone(), at_step));
                } else if name == "read_file"
                    && let Some(header) = self.previous_read_file_header(at_step)
                    && !header.at_end()
                {
                    let to_offset = header.to + 1;
                    let path = args
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    paged = Some(PagedRead {
                        path,
                        from: (header.from, header.to),
                        original_args: args.clone(),
                        to_offset,
                    });
                } else {
                    // With the working set enabled (10. Working set), a window
                    // for this exact call is likely still visible above, so
                    // point there instead of "the log" (which the model can't
                    // see without `recall`).
                    let where_clause = if self.session.working_set.max_windows > 0 {
                        "that window is in your working set above"
                    } else {
                        "that output is in the log"
                    };
                    skip = Some(SkippedCall {
                        reason: format!(
                            "{REDUNDANT_READ_PREFIX} `{name} {args}` at step {at_step} and nothing has \
                             changed since; {where_clause}. Read a different range (pass \
                             offset/limit), search for what you need, or make the change you \
                             committed to"
                        ),
                        cached: String::new(),
                    });
                }
            }
        }

        // 8. Commit (B1 stale-version check first).
        let validation_notes = validated.notes().to_vec();
        let (patch, mut action, acks) = validated.into_parts();
        // 7.8's read-paging special case: rewrite the committed call's args
        // to the offset actually served, so everything downstream (the
        // logged Action entry, the store's patch log, the tool call itself,
        // and 7.8's own `reads_since_mutation` bookkeeping after execution)
        // keys on the range that was really run, not the model's repeated
        // request.
        if let (Some(p), Action::Tool { args, .. }) = (&paged, &mut action)
            && let Value::Object(map) = args
        {
            map.insert(
                "offset".to_string(),
                serde_json::Value::Number(p.to_offset.into()),
            );
            if !map.contains_key("limit") || map.get("limit") == Some(&Value::Null) {
                map.insert(
                    "limit".to_string(),
                    serde_json::Value::Number(DEFAULT_READ_FILE_LIMIT.into()),
                );
            }
        }
        if patch.version() != version {
            return Ok(self.reject(
                entries,
                step_at_start,
                "stale version".to_string(),
                &raw_content,
                None,
            ));
        }

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        for item in &mut next_harness.inbox {
            if acks.contains(&item.id) {
                item.acked = true;
            }
        }
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();
        // `HarnessState::consecutive_non_question_asks`: streak of the raw
        // shape captured above (7, before `validate`), regardless of what
        // the skill's `validate` rewrote the action into -- a fresh `?`-free
        // `ask_user` extends the streak from the previous commit; anything
        // else (a tool call, a real question, `done`, `yield`, a delegate)
        // resets it.
        next_harness.consecutive_non_question_asks = if raw_non_question_ask {
            cur.harness.consecutive_non_question_asks.saturating_add(1)
        } else {
            0
        };
        // `HarnessState::consecutive_continues`: streak of the FINAL action
        // being committed (this `action`, after `validate` -- and 7.8's
        // read-paging rewrite, above -- have both had their say), unlike
        // `consecutive_non_question_asks` above, which counts the raw
        // pre-`validate` shape. A committed `Action::Continue` extends the
        // previous streak; any other committed action resets it to 0 --
        // EXCEPT a `think` step (`is_think_action`, unit 47's carve-out,
        // 3a), which leaves it exactly as it was: unit 47's refusal-think
        // substitution (`skills/src/coding/mod.rs`'s `refusal_thinks`)
        // converts the SECOND consecutive refusal into a `think` call
        // instead of a `Continue`, and without this carve-out that `think`
        // step would itself reset `consecutive_continues` to 0, so the
        // model repeating the SAME refused action a third time would look
        // like a fresh first refusal rather than the third strike --
        // exactly the `error/think/error/think` forever bug `is_think_
        // action`'s own doc describes for `last_tool_error`/`last_skip`,
        // just for this counter instead. Mirrors `tool_error_repeat_count`/
        // `continue_repeat_count`/`non_progress_count`'s own `else if
        // !is_think_action(..)` carve-outs below.
        next_harness.consecutive_continues = if matches!(action, Action::Continue { .. }) {
            cur.harness.consecutive_continues.saturating_add(1)
        } else if is_think_action(&action) {
            cur.harness.consecutive_continues
        } else {
            0
        };

        let patch_raw = patch.raw().clone();
        let new_version = self
            .session
            .store
            .commit(patch, next_harness.clone(), &action, version)
            .await?;
        self.session.failures = 0;
        self.session.last_rejection_reason = None;

        let mut patch_payload = serde_json::json!({"version": new_version, "patch": patch_raw});
        if let Some(usage) = self.session.last_usage
            && let Value::Object(map) = &mut patch_payload
        {
            map.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or(Value::Null),
            );
        }
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: patch_payload,
            agent: None,
        });
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Action,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::to_value(&action).unwrap_or(serde_json::Value::Null),
            agent: None,
        });
        if let Some(p) = &paged {
            // The model's original (un-rewritten) args are kept here "for
            // the record" -- the Action entry just above already carries
            // the rewritten args that actually ran.
            entries.push(system_entry(
                next_harness.step,
                "paged_read",
                serde_json::json!({
                    "path": p.path,
                    "from": [p.from.0, p.from.1],
                    "to_offset": p.to_offset,
                    "args": p.original_args,
                }),
            ));
        }
        if let Some((name, at_step)) = &reread_evicted {
            // Item 19: this call is an exact repeat of an earlier read, but
            // that read's window has since been evicted from the working
            // set, so it ran again for real (no `skipped_redundant`,
            // ordinary `tool_output` below) -- note that for anyone reading
            // the log back.
            entries.push(system_entry(
                next_harness.step,
                "reread_evicted",
                serde_json::json!({"name": name, "at_step": at_step}),
            ));
        }

        // Kept for the context-maintenance call after `execute` (11, below);
        // `Executable::new` takes ownership of the original.
        let action_for_maintainer = action.clone();
        let executable = Executable::new(action);
        let patch_empty = is_empty_patch(&patch_raw);

        // 9. Execute.
        let mut outcome = self
            .execute(
                executable,
                next_harness.step,
                &mut entries,
                token,
                sink,
                skip,
                patch_empty,
                paged,
            )
            .await?;

        // 10. Commitment slot follow-up, after the tool actually ran (so the
        // "commitment fulfilled"/"commitment discharged" note sits in front
        // of the tool's own output, and the harness-authored clear commits
        // on top of whatever state the tool call's execution left, e.g. it
        // never races the model's own patch).
        // A committed call that failed (tool error) does not spend the
        // commitment: the edit did not happen, so `next` must stay set.
        //
        // A call 7.7/7.8 skipped (`shell`/`test` is in `MUTATING_TOOLS`, so
        // a skipped repeat can still match a `next` commitment even though
        // it never ran) is different: 7.7/7.8 only skip a call whose result
        // is already known (an exact repeat of the last verification, or of
        // an already-executed read since the last mutation), so if the
        // skipped call matches the commitment there is nothing left to wait
        // for -- the model committed to a call whose outcome is already in
        // the log. Leaving `next` set here deadlocked run 21 steps 19-27:
        // every following call got rejected with "you committed to ..."
        // because the one call that could fulfil it was never going to run
        // again. So a matching skipped call discharges the commitment the
        // same way a real fulfilment does (harness clears `next`, resets
        // `commitment_reads`), just with a different observation prefix
        // ("commitment discharged: ..." instead of "commitment fulfilled:
        // ...") since the call's own result never re-ran this step.
        let tool_errored = entries.iter().any(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("tool_error")
        });
        let tool_skipped = entries.iter().any(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")
        });
        if tool_errored {
            commitment_fulfilled = None;
        }
        let commitment_discharged_only = tool_skipped && commitment_fulfilled.is_some();
        if let Some(c) = commitment_fulfilled {
            if let Some(obs) = self.session.pending_observation.take() {
                let prefix = if commitment_discharged_only {
                    format!(
                        "commitment discharged: {} {} was already run and nothing has changed \
                         since (its result is above). Set `next` to a different change, or \
                         leave it empty.\n\n",
                        c.tool,
                        c.path.as_deref().unwrap_or("(no path)")
                    )
                } else {
                    format!(
                        "commitment fulfilled: {} {}. Set `next` to the following change, or \
                         leave it empty and run `test`.\n\n",
                        c.tool,
                        c.path.as_deref().unwrap_or("(no path)")
                    )
                };
                let text = format!("{prefix}{}", obs.as_str());
                let (new_obs, _overflow) = Observation::truncate_with_report(
                    &crate::promptable::ToolOutputFull::new(text),
                    self.session.config.budgets.observation,
                    self.session.tokenizer.as_ref(),
                );
                self.session.pending_observation = Some(new_obs);
            }

            let (cur_after, version_after) = self.session.state().await?;
            if self.skill.commitment(&cur_after.agent).is_some() {
                // The model's own patch left `next` set (it did not clear
                // it itself): the harness clears it with a small
                // harness-authored patch, the same way a delegate-absorb
                // patch commits (source: "harness", one extra version,
                // harness.step + 1).
                let clear_value = serde_json::json!({"next": null});
                let clear_patch: <K::Patch as PatchStrategy<K::Workspace>>::Patch =
                    serde_json::from_value(clear_value.clone()).map_err(|e| {
                        SessionError::Patch(crate::error::PatchError::Malformed(e.to_string()))
                    })?;
                let cleared_next = <K::Patch as PatchStrategy<K::Workspace>>::apply(
                    &cur_after.agent,
                    &clear_patch,
                )
                .map_err(SessionError::Patch)?;
                let checked =
                    crate::patch::CheckedPatch::new(clear_value, cleared_next, version_after);
                let validated_clear = crate::patch::ValidatedPatch::from_checked(checked);

                let mut harness_next = cur_after.harness.clone();
                harness_next.step += 1;

                let raw_for_log = validated_clear.raw().clone();
                let new_version = self
                    .session
                    .store
                    .commit(
                        validated_clear,
                        harness_next.clone(),
                        &Action::Yield,
                        version_after,
                    )
                    .await?;

                entries.push(LogEntry {
                    step: harness_next.step,
                    kind: LogKind::Patch,
                    ts: chrono::Utc::now().to_rfc3339(),
                    payload: serde_json::json!({
                        "version": new_version,
                        "patch": raw_for_log,
                        "source": "harness",
                    }),
                    agent: None,
                });
                self.session.last_commitment = None;
                self.session.commitment_reads = 0;
            }
        } else if let Some(note) = commitment_mismatch_note
            && let Some(obs) = self.session.pending_observation.take()
        {
            let text = format!("{}\n\n{note}", obs.as_str());
            let (new_obs, _overflow) = Observation::truncate_with_report(
                &crate::promptable::ToolOutputFull::new(text),
                self.session.config.budgets.observation,
                self.session.tokenizer.as_ref(),
            );
            self.session.pending_observation = Some(new_obs);
        } else if let Some(note) = commitment_pending_note
            && let Some(obs) = self.session.pending_observation.take()
        {
            // `guards.commitment_read_cap = false`: the read ran normally
            // (no cap, no clear); append a short factual note rather than
            // rejecting.
            let text = format!("{}\n\n{note}", obs.as_str());
            let (new_obs, _overflow) = Observation::truncate_with_report(
                &crate::promptable::ToolOutputFull::new(text),
                self.session.config.budgets.observation,
                self.session.tokenizer.as_ref(),
            );
            self.session.pending_observation = Some(new_obs);
        }

        // 10b. Validation notes: a skill's `validate` override (e.g. the
        // coding skills' `strip_malformed_next`, which drops a malformed
        // `next` from the raw patch rather than rejecting the whole step)
        // may record one or more harness-facing notes on the
        // `ValidatedStep` instead of an observation it has no access to.
        // Append each, in order, to the step's observation exactly the way
        // `commitment_mismatch_note` is above. When the action is not a
        // tool call (e.g. `ask_user`), `execute` never sets
        // `pending_observation`, so a note here would otherwise be lost --
        // log it as a `System` entry (`event: "note"`) instead.
        for note in validation_notes {
            if let Some(obs) = self.session.pending_observation.take() {
                let text = format!("{}\n\n{note}", obs.as_str());
                let (new_obs, _overflow) = Observation::truncate_with_report(
                    &crate::promptable::ToolOutputFull::new(text),
                    self.session.config.budgets.observation,
                    self.session.tokenizer.as_ref(),
                );
                self.session.pending_observation = Some(new_obs);
            } else {
                entries.push(system_entry(
                    next_harness.step,
                    "note",
                    serde_json::json!({"text": note}),
                ));
            }
        }

        // 11. Context maintenance (statie-spec.md "9. Context
        // maintenance"): after every committed `Tool` step (`Continue`,
        // whether the tool succeeded or errored -- both are committed
        // steps) and after `AskUser`/`Yield` parks. Never on a rejection
        // (handled by the early returns above, which never reach here) or
        // after `Done`/a handoff/a cancellation/the retry-cap `Failure`
        // park (none of those represent "a committed step whose next
        // observation is ready").
        //
        // Unit 43 (D17 addendum: the maintainer's call is 1.3 s of a 3.25 s
        // step): the call itself runs off the critical path. Right after a
        // step commits, this drains whatever call was spawned for the
        // PREVIOUS eligible step (awaiting it and committing its result,
        // exactly as `run_maintainer` always did synchronously) and only
        // then spawns this step's own call -- never awaited here. Net: at
        // most one maintainer call in flight, and every prompt sees notes
        // at most one step stale (the drain for step N's call happens at
        // the earliest opportunity after step N+1 commits, so step N+1's
        // own prompt -- already built before this point in its `step`
        // call -- never sees it; step N+2's prompt, built at the top of the
        // next `step` call, does).
        let run_maintainer = matches!(
            outcome,
            Outcome::Continue | Outcome::Parked(Park::AskUser(_)) | Outcome::Parked(Park::Yield)
        );
        // D29: build 27 showed the skip rule that used to live here (unit
        // 43's `short_fixer_b23`: withholding the spawn after a harness
        // `Continue`, a `tool_rejected`, or a `skipped_redundant` read)
        // starving the notes during gemma's refusal walks -- long runs of
        // exactly the steps the skip targeted. Unit 46 removes it: every
        // eligible step (`run_maintainer` true) drains whatever call is
        // pending and spawns the new one, regardless of the step's action
        // or entries. The overlap itself (at most one call in flight, the
        // drain-then-spawn ordering) is unchanged from unit 43.
        if run_maintainer {
            // Unit 43: this step's own committed state (the snapshot
            // `spawn_maintainer` needs -- `next_harness.step`, `cur.agent`,
            // `commitment`, `active_tools`) MUST be read before the drain
            // below runs. The drain's own `apply_trusted_patch` (a fresh
            // commit on top of THIS step's) advances the harness step again
            // and can change `notes`/`next` -- reading state after it would
            // hand `spawn_maintainer` the drain's own post-commit state
            // instead of this worker step's, corrupting `notes_step` and
            // every other captured field.
            let (cur_for_spawn, _version) = self.session.state().await?;
            let reasoning_text = entries.iter().find_map(|e| {
                (e.kind == LogKind::Reasoning)
                    .then(|| e.payload.get("text").and_then(Value::as_str))
                    .flatten()
            });
            self.drain_pending_maintainer(sink).await;
            self.spawn_maintainer(
                &cur_for_spawn,
                reasoning_text,
                &patch_raw,
                &action_for_maintainer,
            )
            .await;
            // Unit 48 (D24: per-model behaviour is a config choice, not a
            // code default): `maintainer_overlap == false` drains the call
            // just spawned right away -- same spawn, same drain, same log
            // entries, same `notes_step` -- so the next prompt sees THIS
            // step's notes instead of the previous eligible step's (the
            // synchronous behaviour of builds through 26, unchanged code
            // path, no second implementation). `pending_maintainer` is
            // always `None` again by the time this returns, exactly as it
            // was before unit 43 ever left a call in flight.
            if !self.session.maintainer_overlap {
                self.drain_pending_maintainer(sink).await;
            }
        } else {
            // Spec item 4: `Done`/a handoff/the retry-cap `Failure` park
            // reaches here too (this `else` covers every `outcome` not
            // matched by `run_maintainer` above) -- any maintainer task
            // still in flight from an earlier step must be awaited and
            // committed (or its error logged) before such a terminal
            // outcome is returned, so no task outlives the session and no
            // commit races the store. Never spawns a new one: none of
            // these outcomes represent a step whose reasoning is worth
            // distilling further (the same rule the comment above `run_
            // maintainer` already states).
            self.drain_pending_maintainer(sink).await;
        }

        // Repeated-`Continue`-note guard (`CONTINUE_REPEAT_CAP`): any step
        // whose committed action was NOT `Action::Continue` breaks the
        // streak, the same as a `Continue` step with a different note does
        // (handled inline in `execute`'s `Action::Continue` arm). The
        // park-and-reset the cap itself triggers also clears the streak
        // (see that arm), so this only ever fires for an ordinary,
        // non-`Continue` step in between -- EXCEPT a `think` step
        // (`is_think_action`, unit 47's carve-out, 3a): mirrors `tool_
        // error_repeat_count`/`skip_repeat_count`'s own `is_think_action`
        // exemption (see that function's doc) -- unit 47's refusal-think
        // substitution converts the SECOND consecutive refusal into a
        // `think` call, and without this carve-out that `think` step would
        // clear `continue_repeat_count`, so the SAME refused note
        // recurring a third time would never reach `CONTINUE_REPEAT_CAP`.
        if !matches!(action_for_maintainer, Action::Continue { .. })
            && !is_think_action(&action_for_maintainer)
        {
            self.session.last_continue_note = None;
            self.session.continue_repeat_count = 0;
        }

        // Repeated-skip guard (`CONTINUE_REPEAT_CAP`, shared with the
        // repeated-`Continue`-note guard above): a `skipped_redundant` step
        // (7.7/7.8's "do NOT run the tool" path in `execute`) is the same
        // kind of non-progress as a repeated `Continue` note, but wasn't
        // capped before -- a live run skipped the same call 51 times in one
        // session. Track consecutive occurrences of the SAME `(tool, call
        // key)` (see `read_call_key`); any other committed step (an
        // executed tool call, whether it succeeds or errors) resets the
        // streak, the same as a different skip would.
        if tool_skipped && let Action::Tool { name, args } = &action_for_maintainer {
            let key = (name.clone(), read_call_key(name, args));
            if self.session.last_skip_key.as_ref() == Some(&key) {
                self.session.skip_repeat_count += 1;
            } else {
                self.session.last_skip_key = Some(key.clone());
                self.session.skip_repeat_count = 1;
            }

            // `HarnessState::last_skip`: harness-owned mirror of the
            // `last_skip_key`/`skip_repeat_count` pair just updated above,
            // committed via `set_harness` (the same out-of-band commit
            // pattern `record_tool_call` uses) so a skill's `validate` --
            // which only ever sees committed state, never `Session`'s own
            // in-memory fields -- can read it (`implement_repeated_skip_
            // thinks`, `skills/src/coding/mod.rs`). Recorded at this step's
            // count BEFORE the cap-park reset below (if any): the skip that
            // just happened still gets its own `Some(count)` entry, exactly
            // mirroring `last_skip_key`'s own value for this step.
            let (cur_for_skip, _version) = self.session.state().await?;
            let mut harness_for_skip = cur_for_skip.harness;
            harness_for_skip.last_skip = Some(crate::version::SkippedRead {
                tool: name.clone(),
                key: key.1.clone(),
                count: self.session.skip_repeat_count,
                step: next_harness.step,
            });
            self.session.store.set_harness(harness_for_skip).await?;

            if self.session.skip_repeat_count >= CONTINUE_REPEAT_CAP {
                // Reset so a resumed session starts this streak fresh
                // rather than parking again on the very next step even if
                // the model's reply happens to repeat the same skip once
                // more -- mirrors the `Continue`-note guard's own reset.
                self.session.last_skip_key = None;
                self.session.skip_repeat_count = 0;

                // Prefer the human-readable command (`shell`/`test`) over
                // the raw call key (`read_file`/`search`/...) when one is
                // available -- mirrors the redundant-verification guard's
                // own rejection text.
                let described = extract_command(args).unwrap_or_else(|| key.1.clone());
                let base = self
                    .session
                    .pending_observation
                    .as_ref()
                    .map(|o| o.as_str().to_string())
                    .unwrap_or_default();
                let message = format!(
                    "{base}\n\nThe model has repeated the same skipped call \
                     {CONTINUE_REPEAT_CAP} times in a row ({name} {described}). Reply with what \
                     it should do next."
                );
                let message =
                    self.restore_green_and_append(next_harness.step, &mut entries, message);
                outcome = Outcome::Parked(Park::AskUser(message));
            }
        } else if !tool_skipped && !is_think_action(&action_for_maintainer) {
            self.session.last_skip_key = None;
            self.session.skip_repeat_count = 0;

            // `HarnessState::last_skip`: cleared the same step `last_skip_key`
            // is, mirroring that field's own reset rule -- any committed
            // step that is not itself a skip breaks the streak, EXCEPT a
            // `think` step (`is_think_action`, unit 23's correction): a
            // think between two identical skips must not reset the streak,
            // or the model can alternate skip/think forever without ever
            // reaching `CONTINUE_REPEAT_CAP`. Only touches the store when
            // there is something to clear, avoiding an extra round-trip on
            // the (overwhelmingly common) case where `last_skip` is already
            // `None`.
            let (cur_for_skip, _version) = self.session.state().await?;
            if cur_for_skip.harness.last_skip.is_some() {
                let mut harness_for_skip = cur_for_skip.harness;
                harness_for_skip.last_skip = None;
                self.session.store.set_harness(harness_for_skip).await?;
            }
        }

        // Repeated-tool-error guard (`CONTINUE_REPEAT_CAP`, the same
        // treatment the repeated-skip guard just above gets): a `tool_error`
        // step (the generic tool-execution error path in `execute`, or the
        // `think`/`try_fixes` interceptions' own invalid-argument/no-
        // failing-test tool errors -- any `tool_error` System entry this
        // step produced) is the same kind of non-progress as a repeated skip
        // or `Continue` note: the model keeps retrying a call whose result
        // cannot change without a different action. Unit 25: identity is
        // `(tool name, error_shape(reason))` -- the SHAPE of the reason, not
        // its exact text (`error_shape`, this module) -- read back from the
        // `tool_error` entry's own payload (not from `action_for_maintainer`'s
        // args, which for a rejected-arguments error may differ call to call
        // while the error itself repeats, e.g. `think` with a slightly
        // different empty question each time still reports the same
        // reason). Keying on shape rather than the exact reason means a
        // model that re-issues the same KIND of failing edit with a
        // different line number (or a different quoted snippet) is still
        // recognized as repeating itself. Any other committed step (a
        // successful tool call, a DIFFERENT-shape tool error, a skip, or a
        // `Continue` conversion) resets the streak, the same as a different
        // skip would for `last_skip_key`.
        let tool_error_entry = entries.iter().find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("tool_error")
        });
        // Captured as a plain `bool` (rather than reusing `tool_error_entry`
        // itself) for the stalled-path guard below, since that guard's own
        // `park_non_progress_cap` call needs `&mut entries` and
        // `tool_error_entry` borrows `entries` immutably for its lifetime.
        let had_tool_error = tool_error_entry.is_some();
        // Item 29: a rejected inactive-tool call (`tool_rejected` System
        // entry) joins this SAME streak, the same way a `tool_error` step
        // does -- deliberately a SEPARATE search from `tool_error_entry`
        // above (not a broadened predicate on it), since `tool_error_entry`
        // also feeds the stalled-path guard below (`had_tool_error`,
        // restricted to `EDIT_TOOLS`): a rejected `edit_file` call in a
        // skill that doesn't have it (`plan`, this unit's own worked
        // example) must not also count as an edit failure ON A PATH for
        // that unrelated guard. `tool_error_entry` takes priority when
        // somehow both are present this step (never happens in practice --
        // a rejected call returns before dispatch, so the same step cannot
        // also produce a real `tool_error` -- but `or_else` keeps the
        // ordering explicit rather than assumed).
        let tool_error_entry = tool_error_entry.or_else(|| {
            entries.iter().find(|e| {
                e.kind == LogKind::System
                    && e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected")
            })
        });
        if let Some(e) = tool_error_entry {
            let tool = e
                .payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let reason = e
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // Unit 39: a `tool_rejected` entry (never a real `tool_error`,
            // whose reason text never matches `is_not_a_tool_rejection`)
            // whose reason says the call named no usable tool at all joins
            // one shared class, `("<not a tool>", "not a tool")`, instead of
            // its own `(name, error_shape(reason))` key -- so a model that
            // alternates between an empty `name` and a bogus/path `name`
            // (bench `qwen_json_short_b23/rename_across_files/seed1`: 107
            // rejected calls alternating `name: ""` and
            // `name: "src/config.rs"`) still advances ONE streak instead of
            // splintering into per-name keys that individually never reach
            // `CONTINUE_REPEAT_CAP`.
            let is_rejected =
                e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected");
            let not_a_tool = is_rejected && is_not_a_tool_rejection(&reason);
            let shape = if not_a_tool {
                "not a tool".to_string()
            } else {
                error_shape(&reason)
            };
            let key = if not_a_tool {
                ("<not a tool>".to_string(), shape.clone())
            } else {
                (tool.clone(), shape.clone())
            };
            if self.session.last_tool_error_key.as_ref() == Some(&key) {
                self.session.tool_error_repeat_count += 1;
            } else {
                self.session.last_tool_error_key = Some(key.clone());
                self.session.tool_error_repeat_count = 1;
            }

            // `HarnessState::last_tool_error`: harness-owned mirror of the
            // `last_tool_error_key`/`tool_error_repeat_count` pair just
            // updated above (unit 23 part (b), reshaped by unit 25),
            // committed via `set_harness` the same out-of-band way
            // `HarnessState::last_skip` is just above, so a skill's
            // `validate` can see it (`implement_repeated_error_thinks`,
            // `skills/src/coding/mod.rs`). `args` comes from
            // `action_for_maintainer`, not the `tool_error` entry's own
            // payload (which carries only `name`/`reason`) -- every
            // `tool_error` entry is pushed from within a tool-call dispatch
            // path, so `action_for_maintainer` is always `Action::Tool {
            // name, args }` whenever `tool_error_entry` is `Some` here.
            // `reason` keeps the FULL text of THIS (the latest) error --
            // the `think` question quotes it verbatim -- while `shape` is
            // what the identity/streak comparison above actually uses.
            // Recorded at this step's count BEFORE the cap-park reset below
            // (if any), exactly mirroring `last_skip`'s own timing.
            let error_args = match &action_for_maintainer {
                Action::Tool { args, .. } => args.clone(),
                _ => Value::Null,
            };
            let (cur_for_error, _version) = self.session.state().await?;
            let mut harness_for_error = cur_for_error.harness;
            harness_for_error.last_tool_error = Some(crate::version::RepeatedToolError {
                tool: tool.clone(),
                args: error_args,
                reason,
                shape,
                count: self.session.tool_error_repeat_count,
                step: next_harness.step,
            });
            self.session.store.set_harness(harness_for_error).await?;

            if self.session.tool_error_repeat_count >= 2 {
                // Second (and every later) consecutive identical error:
                // prefix the observation so the model is told explicitly
                // that repeating the call will not change the outcome --
                // mirrors the loop guard's own prefix placement (prepended,
                // so it survives `truncate_with_report`'s budget clamp on a
                // long error message). Unit 39: for the "not a tool" class
                // the prefix names the LATEST `name` (verbatim, possibly
                // empty) instead of a tool, since the whole point of this
                // class is that no tool was actually named.
                if let Some(obs) = self.session.pending_observation.take() {
                    let prefix = if not_a_tool {
                        format!(
                            "[harness] This is the second call in a row that named no tool \
                             (`{tool}` is not one); a third parks the session. Put one of the \
                             active tools in `name`."
                        )
                    } else {
                        format!(
                            "[harness] This is the same `{tool}` error as the previous step; \
                             repeating the call will not change it."
                        )
                    };
                    let text = format!("{prefix}\n\n{}", obs.as_str());
                    let (new_obs, _overflow) = Observation::truncate_with_report(
                        &crate::promptable::ToolOutputFull::new(text),
                        self.session.config.budgets.observation,
                        self.session.tokenizer.as_ref(),
                    );
                    self.session.pending_observation = Some(new_obs);
                }
            }

            if self.session.tool_error_repeat_count >= CONTINUE_REPEAT_CAP {
                // Reset so a resumed session starts this streak fresh,
                // mirroring the repeated-skip guard's own cap reset.
                self.session.last_tool_error_key = None;
                self.session.tool_error_repeat_count = 0;

                // Parks through the mixed-shape non-progress guard's own
                // park path (see `park_non_progress_cap`) rather than a
                // bespoke message: this streak never touched
                // `non_progress_count`/`non_progress_shapes` (a `tool_error`
                // step is neither a `Continue` conversion nor a skip, so the
                // mixed-shape guard below would otherwise reset the streak
                // on this very step), so push this step's own shape first.
                // Unit 39: the "not a tool" class always pushes the fixed
                // `<not a tool>` label, never the latest (possibly empty, or
                // varying) `name` -- the whole park shape names the CLASS,
                // not any one of the names that made it up.
                let park_shape_name = if not_a_tool { "<not a tool>" } else { &tool };
                self.session
                    .non_progress_shapes
                    .push(format!("repeated tool error: {park_shape_name}"));
                outcome = self.park_non_progress_cap(
                    next_harness.step,
                    &mut entries,
                    CONTINUE_REPEAT_CAP,
                );
            }
        } else if !is_think_action(&action_for_maintainer) {
            self.session.last_tool_error_key = None;
            self.session.tool_error_repeat_count = 0;

            // `HarnessState::last_tool_error`: cleared the same step
            // `last_tool_error_key` is, mirroring that field's own reset
            // rule and `HarnessState::last_skip`'s own clearing just above
            // -- any committed step that is not itself a `tool_error`
            // breaks the streak, EXCEPT a `think` step (`is_think_action`,
            // unit 23's correction): a `think` between two identical errors
            // must not clear `last_tool_error`, or the model can alternate
            // error/think/error/think forever, since a cleared
            // `last_tool_error` makes the next identical error look like a
            // fresh streak (count 1) and `CONTINUE_REPEAT_CAP` never fires.
            // Only touches the store when there is something to clear,
            // avoiding an extra round-trip on the (overwhelmingly common)
            // case where `last_tool_error` is already `None`.
            let (cur_for_error, _version) = self.session.state().await?;
            if cur_for_error.harness.last_tool_error.is_some() {
                let mut harness_for_error = cur_for_error.harness;
                harness_for_error.last_tool_error = None;
                self.session.store.set_harness(harness_for_error).await?;
            }
        }

        // Stalled-path guard (unit 25 part 3, `HarnessState::last_stall`):
        // one streak spanning BOTH a refused re-read of a path (the
        // repeated-skip guard just above, `tool_skipped`) and a failed edit
        // on the SAME path (this step's own `tool_error_entry`, restricted
        // to `EDIT_TOOLS`), in either order -- a model that alternates
        // re-reading a file and failing to edit it is going back and forth
        // on that one path even though neither guard above ever sees the
        // SAME shape twice running (a skip then an edit error are different
        // shapes to each of those guards). This step is a "stall event on
        // path P" when either: `tool_skipped` is true and the skipped call
        // named `path`, or `tool_error_entry` is `Some` for a tool in
        // `EDIT_TOOLS` whose args name `path`. Any OTHER committed step --
        // including a skip/error with no `path` argument at all -- is
        // "executed" for this guard's purposes UNLESS it is a `think` step
        // (`is_think_action`, D19: "a `think` step keeps the stall count"),
        // which leaves the streak exactly as it was, mirroring the
        // `last_skip`/`last_tool_error` think exemption.
        let stall_path = if tool_skipped
            && let Action::Tool { name, args } = &action_for_maintainer
            && READ_ONLY_TOOLS.contains(&name.as_str())
        {
            args.get("path").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        }
        .or_else(|| {
            if !had_tool_error {
                return None;
            }
            match &action_for_maintainer {
                Action::Tool { name, args } if EDIT_TOOLS.contains(&name.as_str()) => {
                    args.get("path").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            }
        });

        if let Some(path) = stall_path {
            let same_path_no_break = self.session.last_stall_path.as_deref() == Some(path.as_str());
            if same_path_no_break {
                self.session.stall_count += 1;
            } else {
                self.session.last_stall_path = Some(path.clone());
                self.session.stall_count = 1;
            }

            let (cur_for_stall, _version) = self.session.state().await?;
            let mut harness_for_stall = cur_for_stall.harness;
            harness_for_stall.last_stall = Some(crate::version::Stall {
                path: path.clone(),
                count: self.session.stall_count,
                step: next_harness.step,
            });
            self.session.store.set_harness(harness_for_stall).await?;

            // From the second stall event on (`count >= 2`), the guard is
            // live -- but the actual `think` substitution happens in
            // `implement`'s `validate` (`implement_stall_thinks`,
            // `skills/src/coding/mod.rs`), reading `HarnessState::
            // last_stall` back the same way `implement_repeated_skip_
            // thinks`/`implement_repeated_error_thinks` read their own
            // harness-owned fields; core itself does not substitute
            // anything here, only records the streak and parks at the cap.
            if self.session.stall_count >= NON_PROGRESS_CAP {
                // Reset so a resumed session starts this streak fresh,
                // mirroring the repeated-skip/repeated-tool-error guards'
                // own cap resets.
                self.session.last_stall_path = None;
                self.session.stall_count = 0;

                if !matches!(outcome, Outcome::Parked(_)) {
                    self.session
                        .non_progress_shapes
                        .push(format!("stalled on {path}"));
                    outcome = self.park_non_progress_cap(
                        next_harness.step,
                        &mut entries,
                        NON_PROGRESS_CAP,
                    );
                }
            }
        } else if !is_think_action(&action_for_maintainer) {
            self.session.last_stall_path = None;
            self.session.stall_count = 0;

            // `HarnessState::last_stall`: cleared the same step
            // `last_stall_path` is, mirroring `last_skip`/`last_tool_error`'s
            // own clearing above -- any committed step that is not itself a
            // stall event breaks the streak, EXCEPT a `think` step
            // (`is_think_action`, D19). Only touches the store when there is
            // something to clear.
            let (cur_for_stall, _version) = self.session.state().await?;
            if cur_for_stall.harness.last_stall.is_some() {
                let mut harness_for_stall = cur_for_stall.harness;
                harness_for_stall.last_stall = None;
                self.session.store.set_harness(harness_for_stall).await?;
            }
        }

        // No-green cap (unit 27, `NO_GREEN_PARK_AT`): a failing `test` call
        // that has just brought `HarnessState::fails_since_green` to
        // `NO_GREEN_PARK_AT` parks the run through the mixed-shape
        // non-progress guard's own path, the same way the repeated-
        // tool-error and stalled-path guards above do -- this run has
        // failed `test` that many times since its last green checkpoint (in
        // this activation) without ever landing a pass, regardless of what
        // shape each individual failing step took. Only evaluated when this
        // step hasn't already parked (mirrors the stalled-path guard's own
        // `!matches!(outcome, Outcome::Parked(_))` check just above): a
        // park is itself progress in the sense that matters here.
        if let Action::Tool { name, .. } = &action_for_maintainer
            && name == "test"
            && !matches!(outcome, Outcome::Parked(_))
        {
            let (cur_for_no_green, _version) = self.session.state().await?;
            if cur_for_no_green.harness.fails_since_green >= NO_GREEN_PARK_AT {
                self.session
                    .non_progress_shapes
                    .push(format!("no green in {NO_GREEN_PARK_AT} test runs"));
                outcome =
                    self.park_non_progress_cap(next_harness.step, &mut entries, NO_GREEN_PARK_AT);
            }
        }

        // Mixed-shape non-progress guard (`NON_PROGRESS_CAP`): unlike the
        // two guards just above, which only catch a streak of the SAME
        // shape recurring, this one catches ANY run of non-progress steps
        // regardless of shape -- a live run alternated a skipped repeat, an
        // empty `ask_user`, and a premature `done` for ~30 steps with
        // neither of those caps ever firing, since no single shape ever hit
        // three in a row. Only evaluated when this step didn't already park
        // (from the retry cap inside `execute`'s `Continue` arm, from the
        // repeated-skip guard just above, or from the model's own action):
        // a park is itself progress in the sense that matters here -- a
        // human/driver is now in the loop -- and the two guards above always
        // reset their own counters on the way to parking, so counting this
        // step again on top would double, not extend, the signal.
        if matches!(outcome, Outcome::Continue) {
            let is_non_progress =
                matches!(action_for_maintainer, Action::Continue { .. }) || tool_skipped;
            if is_non_progress {
                self.session.non_progress_count += 1;
                self.session
                    .non_progress_shapes
                    .push(non_progress_shape(tool_skipped, &action_for_maintainer));

                if self.session.non_progress_count >= NON_PROGRESS_CAP {
                    outcome = self.park_non_progress_cap(
                        next_harness.step,
                        &mut entries,
                        NON_PROGRESS_CAP,
                    );
                }
            } else if is_think_action(&action_for_maintainer) {
                // Unit 47's carve-out (3a), mirroring `continue_repeat_
                // count`/`tool_error_repeat_count`'s own `is_think_action`
                // exemption above: a `think` step is neither non-progress
                // itself nor does it clear a non-progress streak already in
                // progress -- the refusal-think substitution
                // (`skills/src/coding/mod.rs`'s `refusal_thinks`) converts
                // the SECOND consecutive refusal into a `think` call, and
                // without this carve-out that `think` step would clear
                // `non_progress_count`, resetting the mixed-shape streak
                // right alongside the identical-note one.
            } else {
                self.session.non_progress_count = 0;
                self.session.non_progress_shapes.clear();
            }
        } else {
            self.session.non_progress_count = 0;
            self.session.non_progress_shapes.clear();
        }

        Ok(StepRecord::new(entries, outcome))
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &mut self,
        executable: Executable<Action>,
        step: u64,
        entries: &mut Vec<LogEntry>,
        token: CancellationToken,
        sink: &mut dyn StepSink,
        skip: Option<SkippedCall>,
        patch_empty: bool,
        paged: Option<PagedRead>,
    ) -> Result<Outcome, SessionError> {
        match executable.into_inner() {
            Action::Tool { name, args } => {
                // A missing/null `args` means "no arguments".
                let args = if args.is_null() {
                    Value::Object(Default::default())
                } else {
                    args
                };

                // 7.7/7.8 recognized this exact call as a redundant repeat:
                // the step already committed (patch + action + acks) above,
                // but the call itself is not re-run -- it would return
                // exactly what's already in the log. Log a `skipped_redundant`
                // System entry, build the observation from the guard's
                // reason (plus the cached prior result for 7.7), and run it
                // through the same loop-guard bookkeeping/note a real repeat
                // would get, so a third identical attempt still gets that
                // note. No `tool_output` entry, no tool call, and none of
                // the other execution-path bookkeeping below (read_streaks,
                // silent_reads, reads_since_mutation, last_verify) is
                // touched -- the guard's own state must not learn anything
                // new from a call that never ran.
                if let Some(SkippedCall { reason, cached }) = skip {
                    entries.push(system_entry(
                        step,
                        "skipped_redundant",
                        serde_json::json!({"name": name, "reason": reason}),
                    ));
                    let mut text = if cached.is_empty() {
                        reason.clone()
                    } else {
                        format!(
                            "{reason}\n\n(call not re-run; this is the earlier result) {cached}"
                        )
                    };

                    // Loop guard: same recurrence check as a real tool call,
                    // keyed on (name, args, text) -- see the success path
                    // below for the un-skipped version of this logic.
                    let current = (name.clone(), args.clone(), text.clone());
                    let prior_occurrences = self
                        .session
                        .recent_tool_calls
                        .iter()
                        .filter(|entry| **entry == current)
                        .count();
                    if prior_occurrences >= 2 {
                        let occurrence = prior_occurrences + 1;
                        text = format!(
                            "[harness] This is occurrence #{occurrence} of `{name}` with \
                             identical arguments and identical output within the last \
                             {TOOL_CALL_HISTORY} tool calls; nothing has changed. Record what \
                             you learned in the workspace and take a different action (if the \
                             work is finished, hand off with `next_skill` or set the terminal \
                             field).\n\n{text}"
                        );
                    }
                    self.session.recent_tool_calls.push_back(current);
                    if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                        self.session.recent_tool_calls.pop_front();
                    }

                    let (obs, _overflow) = Observation::truncate_with_report(
                        &crate::promptable::ToolOutputFull::new(text),
                        self.session.config.budgets.observation,
                        self.session.tokenizer.as_ref(),
                    );
                    self.session.pending_observation = Some(obs);
                    self.session.record_tool_call().await?;
                    return Ok(Outcome::Continue);
                }

                // Enforce the active tool set (the same list the prompt
                // showed the model via `self.skill.tools()`): a tool that is
                // registered in the full `ToolSet` but not active for this
                // skill must still be rejected, not silently run. `think`
                // is exempt (`thinker.listed`, `cli/src/config.rs`, item
                // 14): when `listed` is false it is left out of
                // `skill.tools()` so the model does not see it as a choice,
                // but the harness's own substitutions
                // (`implement_repeat_failure_thinks`'s repeated-failure
                // trigger, the early-park trigger) construct `Action::Tool
                // { name: "think", .. }` directly and must keep working
                // unchanged either way -- by the time this action reaches
                // `execute`, a harness substitution is indistinguishable
                // from the model choosing `think` itself, and it is always
                // specially dispatched via `execute_think` right below
                // regardless, never reaching the real tool registry.
                let active = self.skill.tools();
                if name != "think" && !active.iter().any(|a| a == &name) {
                    // Item 29: the rejection names the skill that actually
                    // has this tool, when one of the skills this skill may
                    // hand off to (`allowed_transitions()`) does
                    // (`Skill::skill_with_tool`) -- so the model is told
                    // where to go, not just what it can't do here. An empty
                    // `name` is its own case (the model sent no tool name
                    // at all, so there is no tool to point anywhere for);
                    // otherwise, an unknown name with no skill that has it
                    // falls back to the plain "not available" text.
                    let skill_with_tool = self.skill.skill_with_tool(&name);
                    let reason = if let Some(s) = &skill_with_tool {
                        format!(
                            "tool `{name}` is not available in skill `{}`; `{s}` has it. \
                             Finish this skill's job, then hand off: {{\"patch\": \
                             {{\"next_skill\": \"{s}\"}}, \"action\": {{\"type\": \"done\", \
                             \"name\": \"\", \"args\": {{}}}}, \"ack_inbox\": []}}",
                            self.skill.name()
                        )
                    } else if name.is_empty() {
                        format!(
                            "action.name is empty: put the tool's name in `name` and its \
                             arguments in `args`; available tools: {active:?}"
                        )
                    } else {
                        format!(
                            "tool `{name}` is not available in skill `{}`; available tools: \
                             {active:?}. There is no tool by that name; if `{name}` is a path, \
                             it belongs in `args`.",
                            self.skill.name()
                        )
                    };
                    entries.push(system_entry(
                        step,
                        "tool_rejected",
                        serde_json::json!({
                            "name": name,
                            "reason": reason,
                            "skill_with_tool": skill_with_tool,
                        }),
                    ));
                    self.session.pending_observation = Some(Observation::user_message(&reason));
                    return Ok(Outcome::Continue);
                }

                // `think` (statie-spec.md's `think` tool): the session
                // intercepts it here, before dispatch to the registry --
                // `statie_tools::Think::call` always errors, by design (see
                // that type's doc). Everything else about a tool step stays
                // generic (`tool_calls` increments via `record_tool_call`,
                // the identical-call guard applies, the observation goes
                // through the usual truncation) -- only the dispatch itself
                // differs.
                if name == "think" {
                    return self.execute_think(step, entries, args, token).await;
                }

                // `try_fixes` (BACKLOG.md item 4): the session intercepts
                // it here, exactly like `think` above -- `statie_tools::
                // TryFixes::call` always errors, by design (see that
                // type's doc).
                if name == "try_fixes" {
                    return self.execute_try_fixes(step, entries, args, token).await;
                }

                let Some(tool) = self.session.tools.get(&name) else {
                    entries.push(system_entry(
                        step,
                        "tool_error",
                        serde_json::json!({"name": name, "reason": "unknown tool"}),
                    ));
                    self.session.pending_observation =
                        Some(Observation::rejection(&format!("unknown tool `{name}`")));
                    return Ok(Outcome::Continue);
                };

                let ctx = crate::tool::ToolCtx {
                    root: self.session.config.root.clone(),
                    log: Arc::clone(&self.session.log),
                    cancel: token.clone(),
                };

                let result = tokio::select! {
                    res = tool.call(args.clone(), &ctx) => Some(res),
                    _ = token.cancelled() => None,
                };

                match result {
                    None => {
                        entries.push(system_entry(
                            step,
                            "cancelled",
                            serde_json::json!({"tool": name}),
                        ));
                        self.session.pending_observation = Some(Observation::tool_aborted(&name));
                        Ok(Outcome::Parked(Park::Cancelled))
                    }
                    Some(Ok(full)) => {
                        entries.push(LogEntry {
                            step,
                            kind: LogKind::ToolOutput,
                            ts: chrono::Utc::now().to_rfc3339(),
                            payload: serde_json::json!({
                                "name": name,
                                "output": full.as_ref(),
                                "truncated": false,
                            }),
                            agent: None,
                        });
                        // Empty output is still information; say so explicitly.
                        let mut text = if full.as_ref().trim().is_empty() {
                            format!("(tool `{name}` returned no output)")
                        } else {
                            full.as_ref().to_string()
                        };

                        // 7.8's read-paging special case: prefix the note
                        // naming the previous (repeated) window and the new
                        // one actually served. The new window's own range is
                        // read back from this call's own output header when
                        // it parses (the tool may have cut the window
                        // earlier than `to_offset + limit - 1`, e.g. its
                        // byte cap); falls back to the rewritten
                        // offset/limit when the header can't be parsed.
                        if let Some(p) = &paged {
                            let (c, d) = parse_read_header(full.as_ref())
                                .map(|h| (h.from, h.to))
                                .unwrap_or((
                                    p.to_offset,
                                    p.to_offset + DEFAULT_READ_FILE_LIMIT - 1,
                                ));
                            text = format!(
                                "note: you repeated the read of lines {}-{}; showing the next \
                                 window instead (lines {c}-{d}).\n\n{text}",
                                p.from.0, p.from.1
                            );
                        }

                        // Loop guard: identical (name, args, output) triple recurring within
                        // the recent-history window, whether consecutively or alternating
                        // (e.g. A/B/A/B).
                        let current = (name.clone(), args.clone(), full.as_ref().to_string());
                        let prior_occurrences = self
                            .session
                            .recent_tool_calls
                            .iter()
                            .filter(|entry| **entry == current)
                            .count();
                        if prior_occurrences >= 2 {
                            let occurrence = prior_occurrences + 1;
                            // Prepended, not appended: `truncate_with_report` clamps the
                            // observation to the budget and would otherwise cut this note
                            // off entirely for a long output (e.g. a 100-line `read_file`),
                            // leaving the model never seeing it.
                            text = format!(
                                "[harness] This is occurrence #{occurrence} of `{name}` with \
                                 identical arguments and identical output within the last \
                                 {TOOL_CALL_HISTORY} tool calls; nothing has changed. Record what \
                                 you learned in the workspace and take a different action (if the \
                                 work is finished, hand off with `next_skill` or set the terminal \
                                 field).\n\n{text}"
                            );
                        }
                        self.session.recent_tool_calls.push_back(current);
                        if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                            self.session.recent_tool_calls.pop_front();
                        }

                        // Redundant-verification guard bookkeeping: a
                        // successful `shell`/`test` call records what it
                        // ran (for comparison against the next call) and
                        // clears the mutated flag; a successful
                        // `write_file`/`edit_file` call sets the mutated
                        // flag so a subsequent repeat of the last verify
                        // command is no longer considered redundant.
                        if name == "shell" || name == "test" {
                            if let Some(cmd) = extract_command(&args) {
                                self.session.last_verify = Some(VerifyRun {
                                    tool: name.clone(),
                                    cmd,
                                    summary: summarize_verify_output(full.as_ref()),
                                });
                                self.session.mutated_since_verify = false;
                            }
                        } else if name == "write_file" || name == "edit_file" {
                            self.session.mutated_since_verify = true;
                            self.session.mutated_since_test = true;
                        }

                        // Green checkpoint (statie-spec.md "10. Working
                        // set", "Green checkpoint"): a `test` call updates
                        // `last_test_result`, and on a pass snapshots this
                        // activation's edited files, logs
                        // `green_checkpoint`, and prefixes the observation.
                        // On a fail, the same call also runs the
                        // repeated-failure guard, logging a
                        // `repeated_failure` entry and prefixing the
                        // observation once the same failure summary has
                        // recurred. Either prefix is prepended last (after
                        // the loop-guard/paging notes above) so it lands at
                        // the very front, exactly as the spec's prefix text
                        // requires.
                        if name == "test"
                            && let Some((entry, prefix)) =
                                self.session.note_test_result(step, full.as_ref())
                        {
                            entries.push(entry);
                            text = format!("{prefix}{text}");
                        }

                        // Guard 2 bookkeeping: a successfully executed
                        // read-only call with an empty patch increments the
                        // silent-read streak; the same call with a
                        // non-empty patch (it recorded something) resets it
                        // instead; any mutating call also resets it (mirrors
                        // the read-without-edit guard's reset rule).
                        if READ_ONLY_TOOLS.contains(&name.as_str()) {
                            if patch_empty {
                                self.session.silent_reads += 1;
                            } else {
                                self.session.silent_reads = 0;
                            }
                        } else if MUTATING_TOOLS.contains(&name.as_str()) {
                            self.session.silent_reads = 0;
                        }

                        // Redundant-identical-read guard bookkeeping: a
                        // successfully executed read-only call (except
                        // `filter_last_tool_call`, exempt by design) is
                        // recorded under its canonical key so an identical
                        // repeat can be rejected before it re-runs; any
                        // successfully executed mutating call clears the map
                        // outright, since a `shell`/`test` run may have
                        // changed anything a prior read saw.
                        if MUTATING_TOOLS.contains(&name.as_str()) {
                            self.session.reads_since_mutation.clear();
                        } else if READ_ONLY_TOOLS.contains(&name.as_str())
                            && name != "filter_last_tool_call"
                        {
                            self.session
                                .reads_since_mutation
                                .insert(read_call_key(&name, &args), step);
                            if let Some(p) = &paged {
                                // The call actually ran under the rewritten
                                // (paged) args, recorded under that key just
                                // above -- but the model's ORIGINAL
                                // (un-rewritten) request is the one it will
                                // send again on a further repeat (it has no
                                // reason to start guessing offsets itself).
                                // Point that key at this step too, so the
                                // next bare repeat's header lookup
                                // (`previous_read_file_header`) reads back
                                // the window this call actually served, not
                                // the stale one from before it was ever
                                // paged (e.g. the very first, un-paged read
                                // of this file).
                                let original_key = read_call_key(&name, &p.original_args);
                                self.session.reads_since_mutation.insert(original_key, step);
                            }
                        }

                        // Read-without-edit guard: a read-only tool call
                        // that brings a path's consecutive-read streak to
                        // the threshold gets a note prepended (after the
                        // loop-guard note above, if both fire); a mutating
                        // tool call resets every path's streak instead.
                        // `guards.read_guard = false` disables the rule
                        // outright: no note, no rejection.
                        if let Some((path, count)) = self
                            .session
                            .guards
                            .read_guard
                            .then(|| self.session.note_tool_call_for_read_guard(&name, &args))
                            .flatten()
                        {
                            entries.push(system_entry(
                                step,
                                "read_guard",
                                serde_json::json!({"path": path, "count": count}),
                            ));
                            text = format!(
                                "[harness] This is read #{count} of `{path}` with no edit in \
                                 between. Reading is not progress: make the change now with the \
                                 lines in front of you (edit_file anchored on one line you can \
                                 see), or write what you learned — the line numbers and the \
                                 exact anchor line — into the workspace so you stop \
                                 re-reading.\n\n{text}"
                            );
                        }

                        let (obs, overflow) = Observation::truncate_with_report(
                            &crate::promptable::ToolOutputFull::new(text),
                            self.session.config.budgets.observation,
                            self.session.tokenizer.as_ref(),
                        );
                        self.session.pending_observation = Some(obs);

                        // Persist a note in the harness view about whether
                        // this tool's output overflowed the observation
                        // budget, so the model is told persistently (not
                        // just in the one prompt where the truncated
                        // observation is shown).
                        let note = overflow.map(|o| {
                            format!(
                                "step {step}: `{name}` output overflowed ({} chars, ~{} tokens > \
                                 {} budget); only the head and tail were shown. Narrow the next \
                                 call or use `filter_last_tool_call`.",
                                o.total_chars,
                                o.total_tokens,
                                self.session.config.budgets.observation
                            )
                        });
                        let (cur, _version) = self.session.state().await?;
                        let mut harness = cur.harness;
                        harness.last_overflow = note;
                        // Green checkpoint (statie-spec.md "10. Working
                        // set", "Green checkpoint"): commit `green_step`
                        // alongside `last_overflow` so it's visible to
                        // `validate`/`rules` on the next step and survives a
                        // handoff, mirroring `self.session.last_test_result`
                        // (set by `note_test_result` above, unconditionally,
                        // for every `test` call): `Some(step)` on a pass,
                        // `None` on a fail. Left untouched for every other
                        // tool.
                        if name == "test" {
                            harness.green_step = if self.session.last_test_result == Some(true) {
                                Some(step)
                            } else {
                                None
                            };
                            // Mirrors `green_step`: recorded from the same
                            // `test` output (`full`, still in scope from
                            // `note_test_result`'s call above) so a filtered
                            // pass is visible to `review`'s `validate`
                            // (`skills/src/coding/mod.rs`) even though the
                            // filtered-out note only lives in the tool
                            // output text, not in `last_test_result`.
                            harness.green_filtered = self.session.last_test_result == Some(true)
                                && full
                                    .as_ref()
                                    .contains("test(s) were filtered out by this command");
                            // Mirrors `green_step`/`green_filtered`: the
                            // `cmd` this `test` call ran, as given in its
                            // args when present, or else parsed back out of
                            // the tool's own `command: <cmd>` first line
                            // (`detected_test_command`) when the call was
                            // auto-detected (`tools/src/test_tool.rs`'s
                            // `detect_test_command`) -- unit 23: this used to
                            // stay `None` on an auto-detected run, leaving
                            // `execute_try_fixes`'s precondition (`Some(cmd)`
                            // alongside `Some(failure)`) unmet even with a
                            // genuine failing test on record. Read by the
                            // `review` skill's `validate` to rerun the same
                            // command on a blind approval rather than a fresh
                            // auto-detected one.
                            harness.last_test_cmd = extract_command(&args)
                                .or_else(|| detected_test_command(full.as_ref()));
                            // `edits_since_test` (`core/src/version.rs`):
                            // every `test` call resets the counter, pass or
                            // fail -- this is the harness's own record of
                            // whether anything changed since the run the
                            // model is about to point at.
                            harness.edits_since_test = 0;
                            // `repeated_failure_step` (`core/src/
                            // version.rs`): `Some(step)` exactly when this
                            // call's failure was identical to the previous
                            // run's -- the same condition
                            // `note_test_result` used above to decide
                            // whether to log a `repeated_failure` entry
                            // (`repeated_failure_count >= 2`); `None` on a
                            // pass or on a fail with a fresh summary.
                            harness.repeated_failure_step = if self.session.last_test_result
                                == Some(false)
                                && self.session.repeated_failure_count >= 2
                            {
                                Some(step)
                            } else {
                                None
                            };
                            // `last_failure_summary` (`core/src/
                            // version.rs`): mirrors `last_test_failure`
                            // (this session's in-memory copy, set by
                            // `note_test_result` above), capped to 600
                            // characters so the harness-owned copy stays
                            // bounded -- `Some` on a fail, `None` on a pass.
                            harness.last_failure_summary = self
                                .session
                                .last_test_failure
                                .as_ref()
                                .map(|s| s.chars().take(600).collect::<String>());
                            // `fails_since_green` (unit 27, `core/src/
                            // version.rs`): incremented on every failing
                            // `test` call, reset to 0 on a pass -- the same
                            // `self.session.last_test_result` this block
                            // already reads to decide `green_step`.
                            if self.session.last_test_result == Some(true) {
                                harness.fails_since_green = 0;
                            } else {
                                harness.fails_since_green =
                                    harness.fails_since_green.saturating_add(1);
                            }
                        } else if matches!(
                            name.as_str(),
                            "edit_file" | "write_file" | "revert_file"
                        ) {
                            harness.edits_since_test = harness.edits_since_test.saturating_add(1);
                        }
                        // This call actually executed -- see
                        // `HarnessState::tool_calls`'s doc. Folded into this
                        // same `set_harness` commit rather than a separate
                        // `record_tool_call` call, avoiding a second
                        // round-trip.
                        harness.tool_calls += 1;
                        // Unit 26: `last_tool` records which one, for the
                        // situational-rules procedure graph. See its doc.
                        harness.last_tool = Some(name.clone());
                        self.session.store.set_harness(harness).await?;

                        // Milestone 10: `Skill::after_tool` (see
                        // `call_after_tool_hook`'s doc) -- called right
                        // after this call's execution and bookkeeping have
                        // committed, on the success path only (never on a
                        // tool error, never on a skipped-redundant call
                        // above; a paged read reaches here with its
                        // rewritten `args`, already in scope).
                        self.call_after_tool_hook(step, &name, &args, full.as_ref())
                            .await?;

                        Ok(Outcome::Continue)
                    }
                    Some(Err(e)) => {
                        entries.push(system_entry(
                            step,
                            "tool_error",
                            serde_json::json!({"name": name, "reason": e.to_string()}),
                        ));
                        let mut message =
                            if let crate::error::ToolError::InvalidArgs(ref detail) = e {
                                let schema_str = render_schema_compact(&tool.args_schema());
                                format!(
                                    "tool `{name}` rejected its arguments: {detail}. Expected \
                                     arguments (JSON schema): {schema_str}. Fix the arguments and \
                                     call `{name}` again."
                                )
                            } else {
                                format!("tool `{name}` failed: {e}")
                            };

                        // Loop guard, same as the success path: identical
                        // (name, args, error) recurring within the
                        // recent-history window. Errors were previously
                        // exempt, so a model retrying the same bad call
                        // never saw the "nothing has changed" note.
                        let current = (name.clone(), args.clone(), e.to_string());
                        let prior_occurrences = self
                            .session
                            .recent_tool_calls
                            .iter()
                            .filter(|entry| **entry == current)
                            .count();
                        if prior_occurrences >= 2 {
                            let occurrence = prior_occurrences + 1;
                            message = format!(
                                "[harness] This is occurrence #{occurrence} of `{name}` with \
                                 identical arguments and identical output within the last \
                                 {TOOL_CALL_HISTORY} tool calls; nothing has changed. Record what \
                                 you learned in the workspace and take a different action (if the \
                                 work is finished, hand off with `next_skill` or set the terminal \
                                 field).\n\n{message}"
                            );
                        }
                        self.session.recent_tool_calls.push_back(current);
                        if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                            self.session.recent_tool_calls.pop_front();
                        }

                        // Built the same way as a successful tool output
                        // (not `Observation::rejection`, which prepends
                        // "output rejected: "), so the loop-guard note stays
                        // at the very front of the observation, and so the
                        // message is clamped to the observation budget the
                        // same way a large tool output would be.
                        let (obs, _overflow) = Observation::truncate_with_report(
                            &crate::promptable::ToolOutputFull::new(message),
                            self.session.config.budgets.observation,
                            self.session.tokenizer.as_ref(),
                        );
                        self.session.pending_observation = Some(obs);
                        self.session.record_tool_call().await?;
                        Ok(Outcome::Continue)
                    }
                }
            }
            Action::AskUser { message } => {
                let message = self.restore_green_and_append(step, entries, message);
                Ok(Outcome::Parked(Park::AskUser(message)))
            }
            Action::Yield => {
                self.restore_green_on_park(step, entries);
                Ok(Outcome::Parked(Park::Yield))
            }
            Action::Continue { note } => {
                // Harness-internal: a `Skill::validate` override committed
                // the model's patch but replaced its action with `Continue`
                // (e.g. a premature `done`/empty `ask_user` in the coding
                // skills) rather than rejecting the whole step. No tool
                // runs, no commitment is spent (the commitment-slot/guard
                // bookkeeping above is all gated on `Action::Tool`, so none
                // of it ran for this step), and the patch is already
                // committed by the time `execute` is called. Log a
                // `continued` System entry with the note, and set it as the
                // next observation through the same truncation helper the
                // rest of the harness's own notes use.
                entries.push(system_entry(
                    step,
                    "continued",
                    serde_json::json!({"note": note}),
                ));

                // Repeated-note guard (`CONTINUE_REPEAT_CAP`): a skill's
                // `validate` override converting a doomed-to-repeat step
                // into `Continue` is the right call the first couple of
                // times, but nothing else bounds how many times in a row it
                // can fire -- the retry cap only applies to a rejected step,
                // never a committed `Continue` one. Track consecutive
                // occurrences of the SAME note text; a different note (or
                // any other outcome) resets the count.
                if self.session.last_continue_note.as_deref() == Some(note.as_str()) {
                    self.session.continue_repeat_count += 1;
                } else {
                    self.session.last_continue_note = Some(note.clone());
                    self.session.continue_repeat_count = 1;
                }

                let (obs, _overflow) = Observation::truncate_with_report(
                    &crate::promptable::ToolOutputFull::new(note.clone()),
                    self.session.config.budgets.observation,
                    self.session.tokenizer.as_ref(),
                );
                self.session.pending_observation = Some(obs);

                if self.session.continue_repeat_count >= CONTINUE_REPEAT_CAP {
                    // Reset so a resumed session (the park is answered with
                    // a user message, then stepped again) starts this
                    // streak fresh rather than parking again on the very
                    // next step even if the model's reply happens to repeat
                    // the same note once more.
                    self.session.last_continue_note = None;
                    self.session.continue_repeat_count = 0;

                    let message = format!(
                        "{note}\n\nThe model has sent the same non-progress step \
                         {CONTINUE_REPEAT_CAP} times in a row. Reply with what it should do \
                         next (for review: 'approve' or the findings to send back)."
                    );
                    let message = self.restore_green_and_append(step, entries, message);
                    return Ok(Outcome::Parked(Park::AskUser(message)));
                }

                Ok(Outcome::Continue)
            }
            Action::Done => {
                let (cur, _version) = self.session.state().await?;
                self.restore_green_on_park(step, entries);
                match self.skill.handoff(&cur.agent) {
                    Some(target) => Ok(Outcome::Handoff(target)),
                    None => Ok(Outcome::Done),
                }
            }
            Action::Delegate { skill: child, task } => {
                let Some(runner) = self.session.child_runner.clone() else {
                    entries.push(system_entry(
                        step,
                        "delegate_rejected",
                        serde_json::json!({"skill": child, "task": task}),
                    ));
                    self.session.pending_observation = Some(Observation::rejection(&format!(
                        "delegation to `{child}` is not available: configured children are []"
                    )));
                    return Ok(Outcome::Continue);
                };
                let known = runner.children();
                if !known.contains(&child) {
                    entries.push(system_entry(
                        step,
                        "delegate_rejected",
                        serde_json::json!({"skill": child, "task": task}),
                    ));
                    self.session.pending_observation = Some(Observation::rejection(&format!(
                        "delegation to `{child}` is not available: configured children are {known:?}"
                    )));
                    return Ok(Outcome::Continue);
                }

                self.session.delegations += 1;
                let n = self.session.delegations;
                let agent_path = format!("{}/{child}#{n}", self.skill.name());

                entries.push(system_entry(
                    step,
                    "delegate",
                    serde_json::json!({"child": child, "task": task, "agent": agent_path}),
                ));

                let req = ChildRequest {
                    child: child.clone(),
                    task: task.clone(),
                    agent_path: agent_path.clone(),
                    parent_step: step,
                    cancel: token.clone(),
                    inbox_seed: vec![ChildInboxItem { text: task.clone() }],
                };

                sink.child_started(&agent_path, &task);
                let report = runner.run(req, sink).await?;
                sink.child_finished(&agent_path, &report);

                // Absorb the report into the parent's workspace: harness-
                // authored, never model-authored, but still runs `rules`
                // and commits through the same RawPatch -> ... ->
                // ValidatedPatch pipeline a model patch does.
                let (cur, version) = self.session.state().await?;
                let mut next = cur.agent.clone();
                if let Err(e) = self.skill.absorb_report(&mut next, report.clone()) {
                    entries.push(system_entry(
                        step,
                        "delegate_absorb_rejected",
                        serde_json::json!({"child": child, "reason": e.to_string()}),
                    ));
                    self.session.pending_observation = Some(Observation::rejection(&format!(
                        "subagent `{child}` finished but its report could not be absorbed: {e}"
                    )));
                    return Ok(Outcome::Continue);
                }
                if let Err(e) = self.skill.rules(&cur, &next) {
                    entries.push(system_entry(
                        step,
                        "delegate_absorb_rejected",
                        serde_json::json!({"child": child, "reason": e.to_string()}),
                    ));
                    self.session.pending_observation = Some(Observation::rejection(&format!(
                        "subagent `{child}` finished but absorbing its report violated a rule: {e}"
                    )));
                    return Ok(Outcome::Continue);
                }

                let cur_value = serde_json::to_value(&cur.agent).unwrap_or(Value::Null);
                let next_value = serde_json::to_value(&next).unwrap_or(Value::Null);
                let patch_value = changed_top_level_keys(&cur_value, &next_value);

                let checked_next: K::Workspace =
                    match <K::Patch as PatchStrategy<K::Workspace>>::apply(
                        &cur.agent,
                        &serde_json::from_value(patch_value.clone()).map_err(|e| {
                            SessionError::Patch(crate::error::PatchError::Malformed(e.to_string()))
                        })?,
                    ) {
                        Ok(n) => n,
                        Err(e) => {
                            entries.push(system_entry(
                                step,
                                "delegate_absorb_rejected",
                                serde_json::json!({"child": child, "reason": e.to_string()}),
                            ));
                            self.session.pending_observation = Some(Observation::rejection(
                                &format!(
                                    "subagent `{child}` finished but the absorb patch failed to apply: {e}"
                                ),
                            ));
                            return Ok(Outcome::Continue);
                        }
                    };
                let checked = crate::patch::CheckedPatch::new(patch_value, checked_next, version);
                let validated = crate::patch::ValidatedPatch::from_checked(checked);

                let mut next_harness = cur.harness.clone();
                next_harness.step += 1;
                next_harness.active_tools = self.skill.tools();
                next_harness.activation = self.skill.name().to_string();

                let raw_for_log = validated.raw().clone();
                let new_version = self
                    .session
                    .store
                    .commit(
                        validated,
                        next_harness.clone(),
                        &Action::Delegate {
                            skill: child.clone(),
                            task: task.clone(),
                        },
                        version,
                    )
                    .await?;

                entries.push(LogEntry {
                    step: next_harness.step,
                    kind: LogKind::Patch,
                    ts: chrono::Utc::now().to_rfc3339(),
                    payload: serde_json::json!({
                        "version": new_version,
                        "patch": raw_for_log,
                        "source": "subagent",
                    }),
                    agent: None,
                });
                entries.push(LogEntry {
                    step: next_harness.step,
                    kind: LogKind::Action,
                    ts: chrono::Utc::now().to_rfc3339(),
                    payload: serde_json::json!({
                        "type": "delegate",
                        "skill": child,
                        "task": task,
                    }),
                    agent: None,
                });

                let outcome_label = match report.outcome {
                    ReportOutcome::Done => "done",
                    ReportOutcome::Blocked => "blocked",
                    ReportOutcome::Failed => "failed",
                    ReportOutcome::Capped => "capped",
                };
                let data_keys: Vec<&str> = report.data.keys().map(String::as_str).collect();
                let summary_text = format!(
                    "subagent {child} finished ({outcome_label}, {} steps): {}. Merged into \
                     reports[\"{child}\"]: findings ({}), files ({}), data keys {:?}.",
                    report.stats.steps,
                    report.summary,
                    report.findings.len(),
                    report.files.len(),
                    data_keys
                );
                let (obs, overflow) = Observation::truncate_with_report(
                    &crate::promptable::ToolOutputFull::new(summary_text),
                    self.session.config.budgets.observation,
                    self.session.tokenizer.as_ref(),
                );
                self.session.pending_observation = Some(obs);
                let note = overflow.map(|o| {
                    format!(
                        "step {step}: subagent `{child}` summary overflowed ({} chars, ~{} \
                         tokens > {} budget); only the head and tail were shown.",
                        o.total_chars, o.total_tokens, self.session.config.budgets.observation
                    )
                });
                if note.is_some() {
                    let mut harness = next_harness.clone();
                    harness.last_overflow = note;
                    self.session.store.set_harness(harness).await?;
                }

                if matches!(report.outcome, ReportOutcome::Capped) && token.is_cancelled() {
                    entries.push(system_entry(
                        next_harness.step,
                        "cancelled",
                        serde_json::json!({"child": child}),
                    ));
                    return Ok(Outcome::Parked(Park::Cancelled));
                }

                Ok(Outcome::Continue)
            }
        }
    }

    /// Runs a `think` call (statie-spec.md's `think` tool): the session's
    /// own handling for `Action::Tool { name: "think", .. }`, called from
    /// `execute` before dispatch to `self.session.tools` (`think` is
    /// registered there only so its schema/description render in the tool
    /// list and an unconfigured session still returns a schema for the
    /// empty-question error message below -- `statie_tools::Think::call`
    /// itself always errors).
    ///
    /// A missing/empty `question` is treated exactly like another tool's
    /// `ToolError::InvalidArgs`: same message shape, same loop-guard/
    /// truncation path, `record_tool_call` still increments (the call was
    /// recognized, just malformed) -- never a rejection. Otherwise: no
    /// thinker configured -> a fixed "not configured" tool output; a
    /// thinker configured -> its `think` call result (`Ok` -> "question:
    /// .../answer:\n...", `Err` -> "think failed: ..."), either way a
    /// successful tool-output observation, never a rejection. A `System`
    /// entry with `event: "think"` is always logged, and
    /// `HarnessState.last_think_step` is always set to this step.
    async fn execute_think(
        &mut self,
        step: u64,
        entries: &mut Vec<LogEntry>,
        args: Value,
        token: CancellationToken,
    ) -> Result<Outcome, SessionError> {
        let name = "think".to_string();
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if question.is_empty() {
            entries.push(system_entry(
                step,
                "tool_error",
                serde_json::json!({"name": name, "reason": "invalid tool arguments: question must not be empty"}),
            ));
            let schema_str = self
                .session
                .tools
                .get(&name)
                .map(|t| render_schema_compact(&t.args_schema()))
                .unwrap_or_else(|| "{}".to_string());
            let mut message = format!(
                "tool `think` rejected its arguments: question must not be empty. Expected \
                 arguments (JSON schema): {schema_str}. Fix the arguments and call `think` again."
            );

            let current = (
                name.clone(),
                args.clone(),
                "invalid tool arguments: question must not be empty".to_string(),
            );
            let prior_occurrences = self
                .session
                .recent_tool_calls
                .iter()
                .filter(|entry| **entry == current)
                .count();
            if prior_occurrences >= 2 {
                let occurrence = prior_occurrences + 1;
                message = format!(
                    "[harness] This is occurrence #{occurrence} of `{name}` with identical \
                     arguments and identical output within the last {TOOL_CALL_HISTORY} tool \
                     calls; nothing has changed. Record what you learned in the workspace and \
                     take a different action (if the work is finished, hand off with \
                     `next_skill` or set the terminal field).\n\n{message}"
                );
            }
            self.session.recent_tool_calls.push_back(current);
            if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                self.session.recent_tool_calls.pop_front();
            }

            let (obs, _overflow) = Observation::truncate_with_report(
                &crate::promptable::ToolOutputFull::new(message),
                self.session.config.budgets.observation,
                self.session.tokenizer.as_ref(),
            );
            self.session.pending_observation = Some(obs);
            self.session.record_tool_call().await?;
            return Ok(Outcome::Continue);
        }

        // Unit 11 ("options-and-verdict mode for `think`"): an optional
        // `mode` argument, `"trace"` (today's behaviour, the default when
        // `mode` is absent) or `"options"` (prepends
        // `OPTIONS_MODE_INSTRUCTION` to the question sent to the thinker).
        // Any other value is a tool error, same shape as the empty-question
        // path above (loop-guarded, truncated, never a rejection), whose
        // message names the two valid modes.
        let mode = args.get("mode").and_then(Value::as_str).unwrap_or("trace");
        if mode != "trace" && mode != "options" {
            let reason = format!(
                "invalid tool arguments: mode must be \"trace\" or \"options\", got {mode:?}"
            );
            entries.push(system_entry(
                step,
                "tool_error",
                serde_json::json!({"name": name, "reason": reason}),
            ));
            let schema_str = self
                .session
                .tools
                .get(&name)
                .map(|t| render_schema_compact(&t.args_schema()))
                .unwrap_or_else(|| "{}".to_string());
            let mut message = format!(
                "tool `think` rejected its arguments: mode must be \"trace\" or \"options\", got \
                 {mode:?}. Expected arguments (JSON schema): {schema_str}. Fix the arguments and \
                 call `think` again."
            );

            let current = (name.clone(), args.clone(), reason);
            let prior_occurrences = self
                .session
                .recent_tool_calls
                .iter()
                .filter(|entry| **entry == current)
                .count();
            if prior_occurrences >= 2 {
                let occurrence = prior_occurrences + 1;
                message = format!(
                    "[harness] This is occurrence #{occurrence} of `{name}` with identical \
                     arguments and identical output within the last {TOOL_CALL_HISTORY} tool \
                     calls; nothing has changed. Record what you learned in the workspace and \
                     take a different action (if the work is finished, hand off with \
                     `next_skill` or set the terminal field).\n\n{message}"
                );
            }
            self.session.recent_tool_calls.push_back(current);
            if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
                self.session.recent_tool_calls.pop_front();
            }

            let (obs, _overflow) = Observation::truncate_with_report(
                &crate::promptable::ToolOutputFull::new(message),
                self.session.config.budgets.observation,
                self.session.tokenizer.as_ref(),
            );
            self.session.pending_observation = Some(obs);
            self.session.record_tool_call().await?;
            return Ok(Outcome::Continue);
        }
        let sent_question = if mode == "options" {
            format!("{OPTIONS_MODE_INSTRUCTION}{question}")
        } else {
            question.to_string()
        };

        let thinker = self.session.thinker.clone();
        let (system, user) = self
            .session
            .last_prompt
            .clone()
            .unwrap_or_else(|| (String::new(), String::new()));

        let (text, configured, reasoning_chars, content_chars, answer_chars, prompt_tokens, error) =
            match thinker {
                None => (
                    "`think` is not configured: set a `thinker` section in the config (see \
                     README, Config). Decide from what you have."
                        .to_string(),
                    false,
                    0usize,
                    0usize,
                    0usize,
                    None::<u64>,
                    None::<String>,
                ),
                Some(thinker) => {
                    let input = ThinkInput {
                        system: &system,
                        user: &user,
                        question: &sent_question,
                    };
                    match thinker.think(input, token.clone()).await {
                        Ok(out) => {
                            let answer_chars = out.answer.chars().count();
                            let text = format!("question: {question}\nanswer:\n{}", out.answer);
                            (
                                text,
                                true,
                                out.reasoning_chars,
                                out.content_chars,
                                answer_chars,
                                out.usage.map(|u| u.prompt_tokens),
                                None,
                            )
                        }
                        Err(e) => (
                            format!("think failed: {e}"),
                            true,
                            0usize,
                            0usize,
                            0usize,
                            None,
                            Some(e.to_string()),
                        ),
                    }
                }
            };

        entries.push(system_entry(
            step,
            "think",
            serde_json::json!({
                "question": question,
                "configured": configured,
                "reasoning_chars": reasoning_chars,
                "content_chars": content_chars,
                "answer_chars": answer_chars,
                "prompt_tokens": prompt_tokens,
                "error": error,
            }),
        ));

        // `ToolOutput` entry, mirroring the generic tool success path (see
        // `execute`'s `Some(Ok(full)) => { ... }` arm): logged with the raw
        // output text, BEFORE the loop-guard prefix below is applied.
        // `recall`/`filter_last_tool_call` read the last `ToolOutput` entry,
        // so without this the model cannot filter or recall a long answer.
        entries.push(LogEntry {
            step,
            kind: LogKind::ToolOutput,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "name": name,
                "output": text,
                "truncated": false,
            }),
            agent: None,
        });

        // Loop guard, same as every other tool's success path: identical
        // (name, args, output) recurring within the recent-history window.
        // `raw_output` keeps the un-prefixed text for `after_tool` below,
        // which (like the generic path's `full`) must see the tool's actual
        // output, not the loop-guard note prepended for the prompt.
        let raw_output = text.clone();
        let current = (name.clone(), args.clone(), text.clone());
        let prior_occurrences = self
            .session
            .recent_tool_calls
            .iter()
            .filter(|entry| **entry == current)
            .count();
        let mut text = text;
        if prior_occurrences >= 2 {
            let occurrence = prior_occurrences + 1;
            text = format!(
                "[harness] This is occurrence #{occurrence} of `{name}` with identical \
                 arguments and identical output within the last {TOOL_CALL_HISTORY} tool calls; \
                 nothing has changed. Record what you learned in the workspace and take a \
                 different action (if the work is finished, hand off with `next_skill` or set \
                 the terminal field).\n\n{text}"
            );
        }
        self.session.recent_tool_calls.push_back(current);
        if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
            self.session.recent_tool_calls.pop_front();
        }

        let (obs, _overflow) = Observation::truncate_with_report(
            &crate::promptable::ToolOutputFull::new(text),
            self.session.config.budgets.observation,
            self.session.tokenizer.as_ref(),
        );
        self.session.pending_observation = Some(obs);

        // Mirrors `tool_calls`'s own commit site: this call executed
        // (configured or not), so it counts, and `last_think_step` records
        // when.
        let (cur, _version) = self.session.state().await?;
        let mut harness = cur.harness;
        harness.tool_calls += 1;
        harness.last_think_step = Some(step);
        // Unit 26: `last_tool` records which tool, for the
        // situational-rules procedure graph. See its doc.
        harness.last_tool = Some(name.clone());
        self.session.store.set_harness(harness).await?;

        // Milestone 10: `Skill::after_tool` (see `call_after_tool_hook`'s
        // doc) -- `think` counts as a successfully executed tool call for
        // this hook the same as any other (configured, unconfigured, and
        // backend-error outputs alike; only the empty-question invalid-args
        // path above never reaches here, matching the generic path never
        // calling the hook on a tool error).
        self.call_after_tool_hook(step, &name, &args, &raw_output)
            .await?;

        Ok(Outcome::Continue)
    }

    /// Runs a `try_fixes` call (BACKLOG.md item 4, "Hypotheses and
    /// verify"): the session's own handling for `Action::Tool { name:
    /// "try_fixes", .. }`, called from `execute` before dispatch to
    /// `self.session.tools`, the same as `execute_think` handles `think`
    /// (`try_fixes` is registered there only so its schema/description
    /// render -- `statie_tools::TryFixes::call` itself always errors).
    ///
    /// No fixer attached: the same unconfigured-observation shape
    /// `execute_think` produces without a thinker (see that function).
    /// No failing test on record (`cur.harness.last_failure_summary` is
    /// `None`, the harness-owned persisted mirror -- see that field's doc):
    /// a tool-error observation, the fixer never called. Otherwise builds a
    /// `FixInput` from the current step's own prompt (mirroring
    /// `execute_think`) and the normalised failure summary already kept by
    /// the repeated-failure bookkeeping, calls `fixer.propose`, then
    /// verifies each candidate in its own worktree via `candidate_runner`
    /// (snapshot once, each candidate tried and cleaned up regardless of
    /// outcome), applies the first passer, and falls back to a `think` call
    /// (when a thinker is attached) if none passed. `HarnessState.
    /// last_try_fixes_step` AND `last_think_step` are always set to this
    /// step (see `HarnessState::last_try_fixes_step`'s doc), so the
    /// once-per-streak repeated-failure gate is shared between the two
    /// tools.
    async fn execute_try_fixes(
        &mut self,
        step: u64,
        entries: &mut Vec<LogEntry>,
        args: Value,
        token: CancellationToken,
    ) -> Result<Outcome, SessionError> {
        let name = "try_fixes".to_string();
        let start = std::time::Instant::now();

        // No fixer attached: same shape as `think` with no thinker.
        let Some(fixer) = self.session.fixer.clone() else {
            return self
                .finish_try_fixes(
                    step,
                    entries,
                    name,
                    args,
                    "`try_fixes` is not configured: set a `fixer` section in the config (see \
                     README, Config). Decide from what you have."
                        .to_string(),
                    serde_json::json!({
                        "k": Value::Null,
                        "candidates": 0,
                        "winner": Value::Null,
                        "losers": Value::Array(vec![]),
                        "configured": false,
                        "wall_ms": start.elapsed().as_millis() as u64,
                    }),
                )
                .await;
        };

        // No failing test on record: no `last_test_cmd` at all, or the most
        // recent `test` call passed (`harness.last_failure_summary` is
        // `None` before any failing `test` call this activation, and
        // cleared back to `None` on a pass -- see that field's doc, and
        // `last_test_failure`'s doc for the in-memory copy it mirrors).
        // Read from the harness-owned, persisted mirror rather than the
        // session-local `last_test_failure` so this check (and the fixer
        // input built from it) survives a handoff/re-activation that resets
        // the session-local copy but not the harness state -- see
        // `last_test_failure`'s doc for why the two can diverge.
        let (cur, _version) = self.session.state().await?;
        let last_test_cmd = cur.harness.last_test_cmd.clone();
        let failure = cur.harness.last_failure_summary.clone();
        let (Some(cmd), Some(failure)) = (last_test_cmd, failure) else {
            entries.push(system_entry(
                step,
                "tool_error",
                serde_json::json!({
                    "name": name,
                    "reason": "no failing test to fix; run test first"
                }),
            ));
            let (obs, _overflow) = Observation::truncate_with_report(
                &crate::promptable::ToolOutputFull::new(
                    "try_fixes: no failing test to fix; run test first".to_string(),
                ),
                self.session.config.budgets.observation,
                self.session.tokenizer.as_ref(),
            );
            self.session.pending_observation = Some(obs);
            self.session.record_tool_call().await?;
            return Ok(Outcome::Continue);
        };

        let (system, user) = self
            .session
            .last_prompt
            .clone()
            .unwrap_or_else(|| (String::new(), String::new()));
        let k = args
            .get("k")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(self.session.default_k);

        let input = FixInput {
            system,
            user,
            failure,
            k,
        };

        let propose_result = fixer.propose(input, token.clone()).await;
        let candidates = match propose_result {
            Ok(out) => out.candidates,
            Err(crate::fixer::FixError::Empty) => {
                return self
                    .finish_try_fixes(
                        step,
                        entries,
                        name,
                        args,
                        "try_fixes: the proposer returned no candidates".to_string(),
                        serde_json::json!({
                            "k": k,
                            "candidates": 0,
                            "winner": Value::Null,
                            "losers": Value::Array(vec![]),
                            "configured": true,
                            "wall_ms": start.elapsed().as_millis() as u64,
                        }),
                    )
                    .await;
            }
            Err(e) => {
                return self
                    .finish_try_fixes(
                        step,
                        entries,
                        name,
                        args,
                        format!("try_fixes failed: {e}"),
                        serde_json::json!({
                            "k": k,
                            "candidates": 0,
                            "winner": Value::Null,
                            "losers": Value::Array(vec![]),
                            "configured": true,
                            "error": e.to_string(),
                            "wall_ms": start.elapsed().as_millis() as u64,
                        }),
                    )
                    .await;
            }
        };

        let verify_outcome = self
            .verify_candidates(&candidates, &cmd, token.clone())
            .await?;

        let mut winner_index: Option<usize> = None;
        let mut loser_descriptions: Vec<String> = Vec::new();
        let text = match verify_outcome {
            VerifyOutcome::Winner {
                index,
                edit,
                diff,
                cmd,
                losers,
            } => {
                winner_index = Some(index);
                let mut lines = vec![format!(
                    "try_fixes: {} candidates; candidate {} passed `{cmd}` and was applied",
                    candidates.len(),
                    index + 1,
                )];
                lines.push(format!("applied: {} — {}", edit.path, edit.rationale));
                let capped_diff: String = diff.chars().take(2000).collect();
                lines.push(capped_diff);
                if !losers.is_empty() {
                    lines.push("losers:".to_string());
                    for l in &losers {
                        loser_descriptions.push(l.line.clone());
                        lines.push(l.line.clone());
                    }
                }
                lines.join("\n")
            }
            VerifyOutcome::NoneWon { cmd, losers } => {
                let mut lines = vec![format!(
                    "try_fixes: {} candidates; none passed `{cmd}`",
                    candidates.len(),
                )];
                for l in &losers {
                    loser_descriptions.push(l.line.clone());
                    lines.push(l.line.clone());
                }
                let base = lines.join("\n");

                // Fall back to `think` when a thinker is attached, with the
                // losers in its question.
                if let Some(thinker) = self.session.thinker.clone() {
                    let loser_text = loser_descriptions.join("\n");
                    let question = format!(
                        "None of these candidate fixes passed {cmd}:\n{loser_text}\nWhat is \
                         wrong instead, and what is the one edit to make?"
                    );
                    let (system, user) = self
                        .session
                        .last_prompt
                        .clone()
                        .unwrap_or_else(|| (String::new(), String::new()));
                    let think_input = ThinkInput {
                        system: &system,
                        user: &user,
                        question: &question,
                    };
                    match thinker.think(think_input, token.clone()).await {
                        Ok(out) => format!("{base}\nthink:\n{}", out.answer),
                        Err(e) => format!("{base}\nthink:\nthink failed: {e}"),
                    }
                } else {
                    base
                }
            }
        };

        let losers_json: Vec<Value> = loser_descriptions
            .iter()
            .map(|l| serde_json::Value::String(l.clone()))
            .collect();
        let payload = serde_json::json!({
            "k": k,
            "candidates": candidates.len(),
            "winner": winner_index.map(|i| i + 1),
            "losers": losers_json,
            "configured": true,
            "wall_ms": start.elapsed().as_millis() as u64,
        });

        self.finish_try_fixes(step, entries, name, args, text, payload)
            .await
    }

    /// Shared tail of `execute_try_fixes`: logs the `try_fixes` System
    /// entry and the `ToolOutput` entry, runs the loop guard, commits
    /// `tool_calls`/`last_try_fixes_step`/`last_think_step`, and calls
    /// `call_after_tool_hook` -- mirroring `execute_think`'s own tail
    /// exactly (see that function's doc for why every one of these steps
    /// runs regardless of whether the call was configured, errored, or
    /// produced a real verdict).
    async fn finish_try_fixes(
        &mut self,
        step: u64,
        entries: &mut Vec<LogEntry>,
        name: String,
        args: Value,
        text: String,
        mut payload: Value,
    ) -> Result<Outcome, SessionError> {
        if let Value::Object(map) = &mut payload {
            map.insert("event".to_string(), Value::String("try_fixes".to_string()));
        }
        entries.push(LogEntry {
            step,
            kind: LogKind::System,
            ts: chrono::Utc::now().to_rfc3339(),
            payload,
            agent: None,
        });

        entries.push(LogEntry {
            step,
            kind: LogKind::ToolOutput,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "name": name,
                "output": text,
                "truncated": false,
            }),
            agent: None,
        });

        let raw_output = text.clone();
        let current = (name.clone(), args.clone(), text.clone());
        let prior_occurrences = self
            .session
            .recent_tool_calls
            .iter()
            .filter(|entry| **entry == current)
            .count();
        let mut text = text;
        if prior_occurrences >= 2 {
            let occurrence = prior_occurrences + 1;
            text = format!(
                "[harness] This is occurrence #{occurrence} of `{name}` with identical \
                 arguments and identical output within the last {TOOL_CALL_HISTORY} tool calls; \
                 nothing has changed. Record what you learned in the workspace and take a \
                 different action (if the work is finished, hand off with `next_skill` or set \
                 the terminal field).\n\n{text}"
            );
        }
        self.session.recent_tool_calls.push_back(current);
        if self.session.recent_tool_calls.len() > TOOL_CALL_HISTORY {
            self.session.recent_tool_calls.pop_front();
        }

        let (obs, _overflow) = Observation::truncate_with_report(
            &crate::promptable::ToolOutputFull::new(text),
            self.session.config.budgets.observation,
            self.session.tokenizer.as_ref(),
        );
        self.session.pending_observation = Some(obs);

        let (cur, _version) = self.session.state().await?;
        let mut harness = cur.harness;
        harness.tool_calls += 1;
        harness.last_try_fixes_step = Some(step);
        harness.last_think_step = Some(step);
        // Unit 26: `last_tool` records which tool, for the
        // situational-rules procedure graph. See its doc.
        harness.last_tool = Some(name.clone());
        self.session.store.set_harness(harness).await?;

        self.call_after_tool_hook(step, &name, &args, &raw_output)
            .await?;

        Ok(Outcome::Continue)
    }

    /// Verifies each candidate in its own worktree via `self.session.
    /// candidate_runner`, in order, stopping at the first pass. Every
    /// worktree created is removed before this returns, on every path
    /// (winner, loser, or a candidate-creation error), via an explicit
    /// cleanup loop rather than relying on `Drop` (the trait objects carry
    /// no such guarantee).
    ///
    /// A candidate whose creation itself errors (an ambiguous/missing
    /// anchor, an invalid path, or a backend/git failure) is a loser with
    /// that error text as its reason -- it is never run.
    async fn verify_candidates(
        &mut self,
        candidates: &[crate::fixer::CandidateEdit],
        cmd: &str,
        _token: CancellationToken,
    ) -> Result<VerifyOutcome, SessionError> {
        let cmd = cmd.to_string();

        // No runner attached: every candidate is a loser with the same
        // "not configured" reason, none tried.
        let Some(runner) = self.session.candidate_runner.clone() else {
            let losers = candidates
                .iter()
                .enumerate()
                .map(|(i, c)| Loser {
                    line: format!(
                        "{}. {} — {} -> try_fixes has no worktree runner configured",
                        i + 1,
                        c.path,
                        c.rationale
                    ),
                })
                .collect();
            return Ok(VerifyOutcome::NoneWon { cmd, losers });
        };

        let root = self.session.config.root.path().to_path_buf();
        let snap = match runner.snapshot(&root).await {
            Ok(s) => s,
            Err(e) => {
                let losers = candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Loser {
                        line: format!("{}. {} — {} -> {e}", i + 1, c.path, c.rationale),
                    })
                    .collect();
                return Ok(VerifyOutcome::NoneWon { cmd, losers });
            }
        };

        let mut losers: Vec<Loser> = Vec::new();
        let mut winner: Option<(usize, crate::fixer::CandidateEdit, String)> = None;

        for (i, c) in candidates.iter().enumerate() {
            if winner.is_some() {
                losers.push(Loser {
                    line: format!("{}. {} — {} -> not tried", i + 1, c.path, c.rationale),
                });
                continue;
            }

            let edit = RunnerEdit {
                path: c.path.clone(),
                old: c.old.clone(),
                new: c.new.clone(),
            };

            let cand = match runner.candidate(snap.as_ref(), &edit).await {
                Ok(cand) => cand,
                Err(e) => {
                    losers.push(Loser {
                        line: format!("{}. {} — {} -> {e}", i + 1, c.path, c.rationale),
                    });
                    continue;
                }
            };

            let run_result = runner
                .run(
                    cand.as_ref(),
                    &cmd,
                    self.session.try_fixes_timeout,
                    self.session.try_fixes_output_cap,
                )
                .await;

            match run_result {
                Ok(res) if res.passed() => {
                    let diff = runner.diff(cand.as_ref()).await.unwrap_or_default();
                    if let Err(e) = runner.apply_to_sandbox(cand.as_ref()).await {
                        losers.push(Loser {
                            line: format!(
                                "{}. {} — {} -> applying the winning diff failed: {e}",
                                i + 1,
                                c.path,
                                c.rationale
                            ),
                        });
                        let _ = runner.remove(cand).await;
                        continue;
                    }
                    winner = Some((i, c.clone(), diff));
                    let _ = runner.remove(cand).await;
                }
                Ok(res) => {
                    let reason = loser_reason(&res.stdout, &res.stderr, res.timed_out);
                    losers.push(Loser {
                        line: format!("{}. {} — {} -> {reason}", i + 1, c.path, c.rationale),
                    });
                    let _ = runner.remove(cand).await;
                }
                Err(e) => {
                    losers.push(Loser {
                        line: format!("{}. {} — {} -> {e}", i + 1, c.path, c.rationale),
                    });
                    let _ = runner.remove(cand).await;
                }
            }
        }

        match winner {
            Some((index, edit, diff)) => {
                // The sandbox changed: bump `edits_since_test` the same as
                // an ordinary `edit_file`/`write_file` call would (see
                // `execute`'s success path), so the model is told it must
                // run `test` to confirm rather than trusting the candidate
                // run alone.
                let (cur, _version) = self.session.state().await?;
                let mut harness = cur.harness;
                harness.edits_since_test = harness.edits_since_test.saturating_add(1);
                self.session.store.set_harness(harness).await?;

                Ok(VerifyOutcome::Winner {
                    index,
                    edit,
                    diff,
                    cmd,
                    losers,
                })
            }
            None => Ok(VerifyOutcome::NoneWon { cmd, losers }),
        }
    }

    fn reject(
        &mut self,
        mut entries: Vec<LogEntry>,
        step: u64,
        reason: String,
        raw_content: &str,
        finish_reason: Option<&str>,
    ) -> StepRecord {
        let reason = match finish_reason {
            Some("length") => {
                let completion_tokens = self
                    .session
                    .last_usage
                    .map(|u| u.completion_tokens)
                    .unwrap_or(0);
                format!(
                    "{reason}; the server cut the output off (done_reason=length after \
                     {completion_tokens} tokens). Write less per step: use edit_file for a \
                     small change instead of write_file for a whole file, or split the file \
                     across steps."
                )
            }
            Some(other) if other != "stop" => format!("{reason} (done_reason={other})"),
            _ => reason,
        };
        self.session.failures += 1;

        // Repeated-`Continue`-note guard (`CONTINUE_REPEAT_CAP`): a
        // rejection is not a repeat of a `Continue` note -- it's a
        // different kind of step entirely, and the step that triggered it
        // never reached `Action::Continue`'s handling in `execute` at all
        // (this function returns before that). Break the streak here too so
        // a rejection sandwiched between two identical `Continue` notes
        // doesn't let them count toward the cap as if they were consecutive.
        self.session.last_continue_note = None;
        self.session.continue_repeat_count = 0;

        // Same reset for the repeated-skip guard (see `last_skip_key`): a
        // rejection is not a repeat of a skip either, and never reaches the
        // skip handling in `execute`.
        self.session.last_skip_key = None;
        self.session.skip_repeat_count = 0;

        // Same reset for the repeated-tool-error guard (see
        // `last_tool_error_key`): a rejection is not a repeat of a tool
        // error either, and never reaches the tool-error handling in
        // `execute`.
        self.session.last_tool_error_key = None;
        self.session.tool_error_repeat_count = 0;

        // Same reset for the stalled-path guard (see `last_stall_path`): a
        // rejection is not a skip or an edit error landing on a path
        // either, and never reaches that handling in `execute`/`step`.
        self.session.last_stall_path = None;
        self.session.stall_count = 0;

        // Same reset for the mixed-shape non-progress guard
        // (`NON_PROGRESS_CAP`): a rejected step never committed, so it is
        // not itself one of the non-progress steps the cap counts, and it
        // should not let two non-progress steps on either side of it count
        // as consecutive.
        self.session.non_progress_count = 0;
        self.session.non_progress_shapes.clear();

        // Prefix the *observation* (never the logged/returned `reason`
        // itself, which tests assert on verbatim) with the attempt count,
        // and, from the second consecutive rejection with the same reason,
        // an explicit nudge -- a low-temperature model can otherwise send
        // byte-identical output every retry until it parks at the cap,
        // never having been told the retry changed nothing.
        let max_attempts = self.session.config.retries + 1;
        let repeated = self.session.last_rejection_reason.as_deref() == Some(reason.as_str());
        let mut observation_text = format!("attempt {} of {max_attempts}: ", self.session.failures);
        if repeated {
            observation_text.push_str(
                "Your previous output was rejected for exactly this reason; send something \
                 different. ",
            );
        }
        observation_text.push_str(&reason);
        self.session.pending_observation = Some(Observation::rejection(&observation_text));
        self.session.last_rejection_reason = Some(reason.clone());

        let content: String = raw_content.chars().take(16000).collect();
        let mut rejected_payload = serde_json::json!({"reason": reason, "content": content});
        if let Some(usage) = self.session.last_usage
            && let Value::Object(map) = &mut rejected_payload
        {
            map.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or(Value::Null),
            );
        }
        entries.push(system_entry(step, "rejected", rejected_payload));

        if self.session.failures > self.session.config.retries {
            self.session.failures = 0;
            self.session.last_rejection_reason = None;
            entries.push(system_entry(
                step,
                "failure",
                serde_json::json!({"reason": reason}),
            ));
            // Green checkpoint: the retry-cap park is one of the parking
            // outcomes that restores a stale checkpoint (statie-spec.md
            // "Green checkpoint" -- "step cap, failure, or `done`"). The
            // sentence is appended only to the *returned* `Park::Failure`
            // message, never to the `"failure"`/`"rejected"` entries just
            // logged above, whose `reason` field callers assert on
            // verbatim.
            let park_reason = self.restore_green_and_append(step, &mut entries, reason);
            StepRecord::new(entries, Outcome::Parked(Park::Failure(park_reason)))
        } else {
            StepRecord::new(entries, Outcome::Continue)
        }
    }

    /// User-authored patch (`/edit`): applies the strategy and skill rules,
    /// but skips the inbox-ack check (the action is always `Yield`).
    pub async fn user_patch(&mut self, patch: Value) -> Result<StepRecord, SessionError> {
        let (cur, version) = self.session.state().await?;

        let checked = validate_user_patch::<K::Workspace, K::Patch>(
            self.skill.as_ref(),
            &cur,
            crate::patch::RawPatch(patch),
            version,
        )
        .map_err(SessionError::Patch)?;
        let step = ValidatedStep::user(checked);
        let (validated, action, _acks) = step.into_parts();

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();

        let patch_raw = validated.raw().clone();
        let new_version = self
            .session
            .store
            .commit(validated, next_harness.clone(), &action, version)
            .await?;
        self.session.failures = 0;
        self.session.last_rejection_reason = None;

        let mut entries = Vec::new();
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "version": new_version,
                "patch": patch_raw,
                "source": "user",
            }),
            agent: None,
        });
        entries.push(LogEntry {
            step: next_harness.step,
            kind: LogKind::Action,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::to_value(&action).unwrap_or(serde_json::Value::Null),
            agent: None,
        });
        Ok(StepRecord::new(entries, Outcome::Parked(Park::Yield)))
    }

    /// Operator-authored patch (`/patch`): applied directly by the harness
    /// on behalf of the user, without going through the model at all. Unlike
    /// `user_patch` (`/edit`, which still flows through a `step`-shaped
    /// `ValidatedStep`/`Outcome`), this exists so an operator can correct
    /// harness state the model itself got wrong (e.g. a plan step marked
    /// done that wasn't, or reset to pending) without arguing with the
    /// model about it.
    ///
    /// Applies `raw` with the skill's `Patch` strategy, runs
    /// `skill.rules(&cur, &next)` -- a violation is rejected the same way a
    /// model-authored patch would be, so the user still can't break the
    /// skill's invariants -- then commits with `harness.step + 1`. Logs a
    /// `Patch` entry with `"source":"user"` and a `System` entry
    /// `{"event":"user_patch"}`, and sets `pending_observation` to a plain
    /// observation (`operator patched the workspace: keys [a, b]`) so the
    /// model's next prompt sees what changed without reading it as a
    /// rejection. Retry counters (`failures`, `last_rejection_reason`) are
    /// left untouched -- this isn't a model turn.
    pub async fn apply_user_patch(&mut self, raw: Value) -> Result<Version, SessionError> {
        let applied = self.apply_trusted_patch(raw, "user").await?;

        self.session.log.append(system_entry(
            applied.step,
            "user_patch",
            serde_json::json!({"keys": applied.keys}),
        ))?;

        self.session.pending_observation = Some(Observation::user_message(&format!(
            "operator patched the workspace: keys {:?}",
            applied.keys
        )));

        Ok(applied.version)
    }

    /// Applies `raw` with the skill's `Patch` strategy and `rules`, exactly
    /// as an operator patch (`/patch`) does, but without the `user_patch`
    /// system entry or observation an operator patch produces -- those are
    /// specific to `apply_user_patch`/the maintainer patch path in `step`,
    /// which each build their own on top of this. Shared by
    /// `apply_user_patch` (source `"user"`) and the context-maintenance
    /// patch in `step` (source `"maintainer"`): both are harness-trusted
    /// writers of harness-owned fields, so both skip the
    /// `Skill::harness_owned_fields` check that gates model patches (that
    /// check lives in `validate_default`, on the model path only;
    /// `validate_user_patch`, which this calls, never runs it).
    ///
    /// Logs a `Patch` entry with `"source": source` and returns the new
    /// version, the harness step it committed at, and the sorted top-level
    /// keys the patch actually changed.
    async fn apply_trusted_patch(
        &mut self,
        raw: Value,
        source: &str,
    ) -> Result<AppliedTrustedPatch, SessionError> {
        let (cur, version) = self.session.state().await?;

        let validated = validate_user_patch::<K::Workspace, K::Patch>(
            self.skill.as_ref(),
            &cur,
            crate::patch::RawPatch(raw),
            version,
        )
        .map_err(SessionError::Patch)?;

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();

        let cur_value = serde_json::to_value(&cur.agent).unwrap_or(Value::Null);
        let next_value = serde_json::to_value(validated.next()).unwrap_or(Value::Null);
        let changed = changed_top_level_keys(&cur_value, &next_value);
        let mut keys: Vec<String> = changed
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort_unstable();

        let patch_raw = validated.raw().clone();
        let new_version = self
            .session
            .store
            .commit(validated, next_harness.clone(), &Action::Yield, version)
            .await?;

        self.session.log.append(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "version": new_version,
                "patch": patch_raw,
                "source": source,
            }),
            agent: None,
        })?;

        Ok(AppliedTrustedPatch {
            version: new_version,
            step: next_harness.step,
            keys,
        })
    }

    /// Applies a [`Skill::after_tool`] patch (milestone 10): unlike
    /// `apply_trusted_patch`, which runs `raw` through the skill's
    /// `PatchStrategy` (JSON Merge Patch's keyed-array merge for the
    /// project default), each top-level key present in `raw` REPLACES that
    /// field's value outright -- no deep merge, no keyed-array merge by
    /// `id`/`path`. A harness hook always knows the field's whole new value
    /// (e.g. the coding skills' `seen`, an array keyed by `path` that must
    /// be able to shrink on eviction or drop an entry on `revert_file` --
    /// exactly what the keyed merge would refuse to do, since it only ever
    /// updates or appends entries, never removes one absent from the
    /// patch), so a plain top-level assign is both simpler and correct
    /// where the keyed merge is not.
    ///
    /// Still runs the skill's `rules` on the candidate next state, and
    /// commits/logs exactly like `apply_trusted_patch` (source `"harness"`,
    /// one extra version, a `Patch` log entry), except the log entry also
    /// carries `"reason": reason` so it is distinguishable from commitment
    /// clearing and other harness-authored patches.
    /// Milestone 10: `Skill::after_tool` -- called right after a tool call's
    /// execution and bookkeeping have committed, on the success path only
    /// (never on a tool error, never on a skipped-redundant call; a paged
    /// read reaches here with its rewritten `args`). Never fails the step: a
    /// patch error is logged as a `System` `note` and otherwise ignored.
    /// Writes straight to `self.session.log` (not the local `entries`
    /// buffer, which `step()` has already mostly finished accumulating by
    /// the time `execute`/`execute_think` run) -- the same convention
    /// `run_maintainer` and `apply_trusted_patch`'s other callers use for a
    /// harness-authored commit made after the step's own patch/action have
    /// already committed. Shared by the generic tool success path and
    /// `execute_think` so the two call sites cannot drift.
    async fn call_after_tool_hook(
        &mut self,
        step: u64,
        name: &str,
        args: &Value,
        output: &str,
    ) -> Result<(), SessionError> {
        let (after_cur, _after_version) = self.session.state().await?;
        if let Some(hook_patch) = self.skill.after_tool(
            &after_cur.agent,
            &after_cur.harness,
            step,
            name,
            args,
            output,
        ) && let Err(e) = self.apply_after_tool_patch(hook_patch, "after_tool").await
        {
            let _ = self.session.log.append(system_entry(
                step,
                "note",
                serde_json::json!({
                    "text": format!(
                        "after_tool patch for `{name}` could not be applied: {e}"
                    )
                }),
            ));
        }
        Ok(())
    }

    async fn apply_after_tool_patch(
        &mut self,
        raw: Value,
        reason: &str,
    ) -> Result<AppliedTrustedPatch, SessionError> {
        let (cur, version) = self.session.state().await?;

        let Value::Object(patch_map) = &raw else {
            return Err(SessionError::Patch(crate::error::PatchError::Malformed(
                "after_tool patch must be a JSON object".to_string(),
            )));
        };

        let mut next_value = serde_json::to_value(&cur.agent)
            .map_err(|e| SessionError::Patch(crate::error::PatchError::Malformed(e.to_string())))?;
        {
            let Value::Object(next_map) = &mut next_value else {
                return Err(SessionError::Patch(crate::error::PatchError::Malformed(
                    "workspace does not serialize to a JSON object".to_string(),
                )));
            };
            for (k, v) in patch_map {
                if v.is_null() {
                    next_map.remove(k);
                } else {
                    next_map.insert(k.clone(), v.clone());
                }
            }
        }
        let next: K::Workspace = serde_json::from_value(next_value)
            .map_err(|e| SessionError::Patch(crate::error::PatchError::Malformed(e.to_string())))?;

        self.skill.rules(&cur, &next).map_err(SessionError::Patch)?;

        let checked = crate::patch::CheckedPatch::new(raw, next, version);
        let validated = crate::patch::ValidatedPatch::from_checked(checked);

        let mut next_harness = cur.harness.clone();
        next_harness.step += 1;
        next_harness.active_tools = self.skill.tools();
        next_harness.activation = self.skill.name().to_string();

        let cur_value = serde_json::to_value(&cur.agent).unwrap_or(Value::Null);
        let next_value_for_keys = serde_json::to_value(validated.next()).unwrap_or(Value::Null);
        let changed = changed_top_level_keys(&cur_value, &next_value_for_keys);
        let mut keys: Vec<String> = changed
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort_unstable();

        let patch_raw = validated.raw().clone();
        let new_version = self
            .session
            .store
            .commit(validated, next_harness.clone(), &Action::Yield, version)
            .await?;

        self.session.log.append(LogEntry {
            step: next_harness.step,
            kind: LogKind::Patch,
            ts: chrono::Utc::now().to_rfc3339(),
            payload: serde_json::json!({
                "version": new_version,
                "patch": patch_raw,
                "source": "harness",
                "reason": reason,
            }),
            agent: None,
        })?;

        Ok(AppliedTrustedPatch {
            version: new_version,
            step: next_harness.step,
            keys,
        })
    }

    /// `Skill::on_activate`: called once right after this activation's
    /// state/harness have been committed and the `activate` System entry
    /// logged (i.e. right before `Session::activate_inner` hands the built
    /// `Active` back to its caller) -- covering every activation path
    /// (fresh activate, handoff, resume with `--skill` via
    /// `Session::activate`, and `activate_forced`), since all of them
    /// funnel through `activate_inner`.
    ///
    /// Applies the returned patch the same way an `after_tool` hook's is
    /// (`apply_after_tool_patch`, top-level field replacement, source
    /// `"harness"`), just with `reason: "on_activate"` so the two are
    /// distinguishable in the log. Never fails activation: an error
    /// applying the patch is logged as a `System` `note` and otherwise
    /// ignored, mirroring `after_tool`'s own error handling in `execute`.
    async fn run_on_activate(&mut self) -> Result<(), SessionError> {
        let (cur, _version) = self.session.state().await?;
        let Some(patch) = self.skill.on_activate(&cur.agent) else {
            return Ok(());
        };
        if let Err(e) = self.apply_after_tool_patch(patch, "on_activate").await {
            let step = cur.harness.step;
            let _ = self.session.log.append(system_entry(
                step,
                "note",
                serde_json::json!({
                    "text": format!("on_activate patch could not be applied: {e}")
                }),
            ));
        }
        Ok(())
    }

    /// Context maintenance (statie-spec.md "9. Context maintenance"), spawn
    /// half (unit 43, D17 addendum: the maintainer's call is 1.3 s of a
    /// 3.25 s step): builds this step's `MaintainInput` -- unchanged from
    /// before this unit, spec item 5 -- and hands it to `maintainer.maintain`
    /// as a `tokio::spawn`ed task, stored as `self.session.pending_maintainer`.
    /// Never awaited here: callers must call `drain_pending_maintainer`
    /// first (this step's caller in `step` always does, right before this),
    /// so there is at most one call in flight at a time (spec item 1).
    ///
    /// `cur` is THIS step's own just-committed state, read by the caller
    /// BEFORE it calls `drain_pending_maintainer` -- never re-read here via
    /// `self.session.state()`, since the drain's own `apply_trusted_patch`
    /// commits on top of it (bumping the harness step again and possibly
    /// changing `notes`/`next`), and reading afterward would hand this
    /// function the drain's post-commit state instead of this step's,
    /// corrupting `notes_step` and everything else captured below. An
    /// absent `Maintainer` leaves `pending_maintainer` untouched (`None`
    /// stays `None`) exactly the way the old synchronous `run_maintainer`
    /// returned early without logging anything for that case. Reports
    /// nothing to `sink`/the log itself: unlike the old synchronous
    /// version, spawning never fails in a way worth reporting this step --
    /// the spawned call's own success/failure is only known once
    /// `drain_pending_maintainer` awaits it, at which point THAT call
    /// reports it.
    async fn spawn_maintainer(
        &mut self,
        cur: &CommittedState<K::Workspace>,
        reasoning: Option<&str>,
        patch: &Value,
        action: &Action,
    ) {
        let Some(maintainer) = self.session.maintainer.clone() else {
            return;
        };

        let step = cur.harness.step;

        let observation_text = self
            .session
            .pending_observation
            .as_ref()
            .map(Observation::as_str)
            .unwrap_or("")
            .to_string();
        let agent = serde_json::to_value(&cur.agent).unwrap_or(Value::Null);
        let commitment = self.skill.commitment(&cur.agent);
        let notes = self.skill.notes(&cur.agent).unwrap_or_default();
        let budget = cur.harness.budgets.notes;
        let active_tools = cur.harness.active_tools.clone();

        // Recorded in the `maintainer` System entry the drain logs, so a
        // run can show whether the maintainer ever actually saw any
        // reasoning: the maintainer's own log entry does not otherwise say.
        let reasoning_chars = reasoning.map(str::len).unwrap_or(0);
        let observation_chars = observation_text.len();

        let owned = MaintainInputOwned {
            reasoning: reasoning.map(str::to_string),
            patch: patch.clone(),
            action: action.clone(),
            observation: observation_text,
            agent,
            commitment,
            notes,
            budget,
            active_tools,
        };

        let handle = tokio::spawn(async move { maintainer.maintain(owned.as_input()).await });

        self.session.pending_maintainer = Some(PendingMaintainer {
            step,
            budget,
            reasoning_chars,
            observation_chars,
            handle,
        });
    }

    /// Context maintenance (statie-spec.md "9. Context maintenance"), drain
    /// half (unit 43): awaits whatever call `spawn_maintainer` most recently
    /// started (a no-op when `pending_maintainer` is `None`) and commits its
    /// result exactly as the old synchronous `run_maintainer` always did --
    /// same log entries, same `maintainer`/`maintainer_error` system events,
    /// same `next` handling (the inactive-tool/malformed-`next`/
    /// commitment-is-set checks all re-read the CURRENT state via
    /// `self.skill`, not a snapshot from spawn time, so a commitment or
    /// activation change in the step(s) between spawn and drain is honoured
    /// -- spawn time only froze what the maintainer itself was GIVEN, per
    /// spec item 5). Also commits `HarnessState.notes_step` alongside the
    /// notes patch, set to the step the drained call was spawned for.
    ///
    /// A maintainer error (backend down, over budget after one retry, or
    /// its patch rejected twice) logs `maintainer_error`, keeps the
    /// previous notes, and never changes the step outcome -- callers must
    /// not propagate this into the step's own `Result`.
    async fn drain_pending_maintainer(&mut self, sink: &mut dyn StepSink) {
        let Some(pending) = self.session.pending_maintainer.take() else {
            return;
        };
        let PendingMaintainer {
            step,
            budget,
            reasoning_chars,
            observation_chars,
            handle,
        } = pending;

        let out = match handle.await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                let reason = e.to_string();
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                sink.maintainer_failed(&reason);
                return;
            }
            Err(join_err) => {
                let reason = join_err.to_string();
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                sink.maintainer_failed(&reason);
                return;
            }
        };
        let mut out = out;

        // Unit 45: live bench evidence -- qwen_json_short_b26: 51 of 310
        // maintainer calls discarded for notes over budget; qwen_json_long_b24:
        // 170 of 368; most overshoots 3 to 60 tokens over 400. Discarding the
        // whole result over a few tokens threw away a good-enough summary
        // (and any `next` it carried) along with it, so cut instead of
        // dropping: `cut_notes_to_budget` returns the text unchanged when
        // it already fits, so `notes_tokens` below is always the
        // post-cut (in-budget) count.
        let notes_tokens_before = self.session.tokenizer.count(&out.notes);
        let (cut_notes, notes_tokens) = crate::maintainer::cut_notes_to_budget(
            &out.notes,
            budget,
            self.session.tokenizer.as_ref(),
        );
        if notes_tokens_before > budget {
            let _ = self.session.log.append(system_entry(
                step,
                "maintainer_notes_cut",
                serde_json::json!({"from": notes_tokens_before, "to": notes_tokens}),
            ));
        }
        out.notes = cut_notes;

        let (cur, _version) = match self.session.state().await {
            Ok(s) => s,
            Err(e) => {
                let reason = e.to_string();
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                sink.maintainer_failed(&reason);
                return;
            }
        };
        let commitment = self.skill.commitment(&cur.agent);
        let active_tools = cur.harness.active_tools.clone();

        // A proposed `next` whose tool isn't active for the CURRENT
        // activation is never a real commitment (statie-spec.md "9. Context
        // maintenance"): the worker has no way to satisfy it. Drop it and
        // log `maintainer_error`, but still apply the notes below.
        if let Some(c) = &out.next
            && !active_tools.iter().any(|t| t == &c.tool)
        {
            let reason = format!("proposed next uses inactive tool `{}`", c.tool);
            let _ = self.session.log.append(system_entry(
                step,
                "maintainer_error",
                serde_json::json!({"reason": reason}),
            ));
            out.next = None;
        }

        // A proposed `next` that is malformed in the same ways the coding
        // skills' `strip_malformed_next` (skills/src/coding/mod.rs) checks
        // a worker-written `next` is never a real commitment either: a
        // commitment must be a mutating call (`MUTATING_TOOLS`), since the
        // session's commitment read cap can only ever be cleared by one --
        // capping reads against a commitment no call can fulfil (e.g.
        // `next.tool = "read_file"`) rejects the model to the retry cap.
        // Checked in the same order as `strip_malformed_next`, first match
        // wins. Drop and log `maintainer_error`, but still apply the notes
        // below.
        if let Some(c) = &out.next {
            let reason = if !MUTATING_TOOLS.contains(&c.tool.as_str()) {
                Some(format!(
                    "proposed next is not a mutating call: `{}`",
                    c.tool
                ))
            } else if c.what.trim().is_empty() {
                Some("proposed next has an empty `what`".to_string())
            } else if matches!(c.tool.as_str(), "edit_file" | "write_file")
                && c.path.as_deref().map(str::trim).unwrap_or("").is_empty()
            {
                Some(format!("proposed next `{}` has no `path`", c.tool))
            } else {
                None
            };

            if let Some(reason) = reason {
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                out.next = None;
            }
        }

        // `next` is applied only when the workspace's own commitment is
        // still `None` as of THIS (drain) time -- a maintainer-proposed
        // commitment never overrides one the worker wrote itself, including
        // one the worker set in a step after this call was spawned.
        let propose_next = commitment.is_none() && out.next.is_some();

        let mut patch_value = serde_json::json!({"notes": out.notes});
        if propose_next && let Value::Object(map) = &mut patch_value {
            map.insert(
                "next".to_string(),
                serde_json::to_value(&out.next).unwrap_or(Value::Null),
            );
        }

        match self
            .apply_trusted_patch(patch_value.clone(), "maintainer")
            .await
        {
            Ok(applied) => {
                self.set_notes_step(step).await;
                let _ = self.session.log.append(system_entry(
                    applied.step,
                    "maintainer",
                    serde_json::json!({
                        "notes_tokens": notes_tokens,
                        "next_set": propose_next,
                        "reasoning_chars": reasoning_chars,
                        "observation_chars": observation_chars,
                    }),
                ));
                sink.maintainer_done(notes_tokens, propose_next);
            }
            Err(e) if propose_next => {
                // Retry once with `next` omitted: a bad `next` shouldn't
                // cost the notes update.
                let reason = e.to_string();
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                let retry_value = serde_json::json!({"notes": out.notes});
                match self.apply_trusted_patch(retry_value, "maintainer").await {
                    Ok(applied) => {
                        self.set_notes_step(step).await;
                        let _ = self.session.log.append(system_entry(
                            applied.step,
                            "maintainer",
                            serde_json::json!({
                                "notes_tokens": notes_tokens,
                                "next_set": false,
                                "reasoning_chars": reasoning_chars,
                                "observation_chars": observation_chars,
                            }),
                        ));
                        sink.maintainer_done(notes_tokens, false);
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        let _ = self.session.log.append(system_entry(
                            step,
                            "maintainer_error",
                            serde_json::json!({"reason": reason}),
                        ));
                        sink.maintainer_failed(&reason);
                    }
                }
            }
            Err(e) => {
                let reason = e.to_string();
                let _ = self.session.log.append(system_entry(
                    step,
                    "maintainer_error",
                    serde_json::json!({"reason": reason}),
                ));
                sink.maintainer_failed(&reason);
            }
        }
    }

    /// Sets `HarnessState.notes_step` to `step` via the same out-of-band
    /// `set_harness` commit pattern `last_skip`/`last_tool_error`/
    /// `last_stall` use (unit 43): called right after `apply_trusted_patch`
    /// commits the drained maintainer's notes, so the harness state a
    /// prompt reads afterward carries which step those notes summarise.
    /// Never fails the drain: a `state()`/`set_harness` error here is
    /// swallowed the same way those other call sites don't propagate one
    /// (this is bookkeeping on top of an already-committed notes patch, not
    /// the patch itself).
    async fn set_notes_step(&mut self, step: u64) {
        if let Ok((cur, _version)) = self.session.state().await {
            let mut harness = cur.harness;
            harness.notes_step = Some(step);
            let _ = self.session.store.set_harness(harness).await;
        }
    }

    /// Replays the patch log up to `to` using this skill's patch strategy.
    pub async fn replay(&self, to: Version) -> Result<CommittedState<K::Workspace>, SessionError> {
        let apply = apply_fn::<K::Workspace, K::Patch>();
        Ok(self.session.store.replay(to, &apply).await?)
    }

    pub fn session(&mut self) -> &mut Session<K::Workspace> {
        self.session
    }

    /// Token count of the most recently built prompt, if any.
    pub fn last_prompt_tokens(&self) -> Option<usize> {
        self.session.last_prompt_tokens
    }

    /// Server-reported usage from the most recent model call, if any.
    /// `None` when the step made no model call (e.g. terminal/handoff), or
    /// when the backend reported none.
    pub fn last_usage(&self) -> Option<Usage> {
        self.session.last_usage
    }

    pub fn skill_name(&self) -> &str {
        self.skill.name()
    }
}

fn system_entry(step: u64, event: &str, mut payload: Value) -> LogEntry {
    if let Value::Object(map) = &mut payload {
        map.insert("event".to_string(), Value::String(event.to_string()));
    }
    LogEntry {
        step,
        kind: LogKind::System,
        ts: chrono::Utc::now().to_rfc3339(),
        payload,
        agent: None,
    }
}

/// Compact-JSON rendering of a tool's args schema for a rejection message.
/// Above 1200 chars, falls back to just the `properties` keys and the
/// `required` list, so a large schema doesn't blow out the observation
/// with detail the model doesn't need to fix a rejected call.
fn render_schema_compact(schema: &Schema) -> String {
    let full = serde_json::to_string(schema.as_value()).unwrap_or_else(|_| "{}".to_string());
    if full.len() <= 1200 {
        return full;
    }
    let obj = schema.as_value().as_object();
    let properties: Vec<&String> = obj
        .and_then(|o| o.get("properties"))
        .and_then(Value::as_object)
        .map(|p| p.keys().collect())
        .unwrap_or_default();
    let required: Vec<&Value> = obj
        .and_then(|o| o.get("required"))
        .and_then(Value::as_array)
        .map(|r| r.iter().collect())
        .unwrap_or_default();
    serde_json::json!({"properties": properties, "required": required}).to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_command, normalize_failure_summary, read_call_key, summarize_verify_output,
    };

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        assert_eq!(normalize_command("  echo hi  "), "echo hi");
    }

    #[test]
    fn cuts_at_first_pipe() {
        assert_eq!(normalize_command("echo hi | cat"), "echo hi");
        assert_eq!(normalize_command("echo hi|cat|grep x"), "echo hi");
    }

    #[test]
    fn strips_trailing_redirect() {
        assert_eq!(normalize_command("cargo test 2>&1"), "cargo test");
    }

    #[test]
    fn collapses_internal_whitespace() {
        assert_eq!(normalize_command("echo   hi\tthere"), "echo hi there");
    }

    #[test]
    fn combines_all_normalizations() {
        assert_eq!(
            normalize_command("  cargo   test  -p  foo   2>&1  | tee out.log  "),
            "cargo test -p foo"
        );
    }

    #[test]
    fn empty_and_whitespace_only_normalize_to_empty() {
        assert_eq!(normalize_command(""), "");
        assert_eq!(normalize_command("   "), "");
    }

    #[test]
    fn different_commands_are_not_equal_after_normalization() {
        assert_ne!(normalize_command("echo hi"), normalize_command("echo bye"));
    }

    #[test]
    fn summarize_skips_shell_framing_lines() {
        let out = "exit: 0\n--- stdout ---\nhi\n--- stderr ---\n";
        assert_eq!(summarize_verify_output(out), "hi");
    }

    #[test]
    fn summarize_includes_failure_lines() {
        let out = "command: cargo test\nresult: fail (exit 1)\n--- failure summary ---\ntest \
                    foo::bar ... FAILED\nerror[E0382]: use of moved value\nthread 'main' \
                    panicked at src/main.rs";
        let summary = summarize_verify_output(out);
        // The `command:` line repeats what the rejection already quotes;
        // the summary starts at the result line instead.
        assert!(!summary.contains("command: cargo test"), "{summary}");
        assert!(summary.starts_with("result: fail (exit 1)"), "{summary}");
        assert!(summary.contains("FAILED"));
        assert!(summary.contains("error[E0382]"));
        assert!(summary.contains("panicked"));
    }

    #[test]
    fn summarize_truncates_to_300_chars() {
        let long_line = "x".repeat(500);
        let summary = summarize_verify_output(&long_line);
        assert_eq!(summary.chars().count(), 300);
    }

    #[test]
    fn normalize_failure_summary_ignores_thread_ids_and_timings() {
        let run = |tid: &str, secs: &str| {
            format!(
                "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                 test result: ok. 0 passed; 0 failed; finished in {secs}\n\
                 test two_items ... FAILED\n\
                 thread 'two_items' ({tid}) panicked at tests/total.rs:6:5:\n\
                 assertion `left == right` failed\n  left: 4491\n right: 4492\n--- stdout ---\n"
            )
        };
        let a = normalize_failure_summary(&run("12100280", "0.00s"));
        let b = normalize_failure_summary(&run("12100417", "0.01s"));
        assert_eq!(
            a, b,
            "two runs of the same failing test must normalize equal"
        );
        assert!(
            a.contains("left: 4491"),
            "the assertion itself must survive: {a}"
        );
        assert!(!a.contains("12100280"), "thread id must be scrubbed: {a}");
        assert!(a.contains("finished in Xs"), "timing must be scrubbed: {a}");
        // A different assertion is still a different failure.
        let c = normalize_failure_summary(&run("1", "0.00s").replace("4491", "4490"));
        assert_ne!(a, c);
    }

    #[test]
    fn normalize_failure_summary_stops_at_the_next_marker_section() {
        let output = "command: cargo test\nresult: fail (exit 1)\n--- failure summary ---\n\
                       unexpected closing delimiter: `}`\nsrc/lib.rs:42\n--- stderr ---\n\
                       noisy raw compiler output here\n";
        assert_eq!(
            normalize_failure_summary(output),
            "unexpected closing delimiter: `}`\nsrc/lib.rs:42"
        );
    }

    #[test]
    fn normalize_failure_summary_without_marker_drops_the_command_line() {
        let output = "command: cargo test\nresult: fail (exit 1)\nsome raw output with no \
                       failure-summary marker\nmore output";
        assert_eq!(
            normalize_failure_summary(output),
            "result: fail (exit 1)\nsome raw output with no failure-summary marker\nmore output"
        );
    }

    #[test]
    fn read_call_key_is_stable_for_identical_calls() {
        let a = serde_json::json!({"path": "a.rs", "offset": 1, "limit": 5});
        let b = serde_json::json!({"path": "a.rs", "offset": 1, "limit": 5});
        assert_eq!(
            read_call_key("read_file", &a),
            read_call_key("read_file", &b)
        );
    }

    #[test]
    fn read_call_key_ignores_object_key_order() {
        let a = serde_json::json!({"path": "a.rs", "offset": 1, "limit": 5});
        let b = serde_json::json!({"limit": 5, "offset": 1, "path": "a.rs"});
        assert_eq!(
            read_call_key("read_file", &a),
            read_call_key("read_file", &b)
        );
    }

    #[test]
    fn read_call_key_differs_on_different_args() {
        let a = serde_json::json!({"path": "a.rs", "offset": 1, "limit": 5});
        let b = serde_json::json!({"path": "a.rs", "offset": 2, "limit": 5});
        assert_ne!(
            read_call_key("read_file", &a),
            read_call_key("read_file", &b)
        );
    }

    #[test]
    fn read_call_key_differs_on_different_tool_name() {
        let args = serde_json::json!({"path": "a.rs"});
        assert_ne!(
            read_call_key("read_file", &args),
            read_call_key("search", &args)
        );
    }
}
