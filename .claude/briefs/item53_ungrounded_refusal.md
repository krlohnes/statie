# Unit 53: all-uncited findings are a refusal, not a rejection

## Background (read, do not restate)
`review_grounded_findings` (`skills/src/coding/mod.rs`, item 12) drops findings with no
`file:line` citation. When some survive, it rewrites the patch and returns a note. When NONE
survive it returns `Err(PatchError::RuleViolation("Dropped N finding(s) with no file:line
citation. ..."))`: a rejection, which discards the whole patch and burns one of the three
retries. Build 34 Qwen short: two runs (`python_method` seed 1, `rename_across_files` seed 1)
hit that rejection three times each and parked as `failure` with the crate green on disk; build
32 gemma `rename_across_files` seed 2 hit it twice. The model that sends uncited findings sends
them again when rejected; it needs the note in front of it with its patch kept, which is what
every other refusal in this module does (a harness `Action::Continue { note }`), and the second
strike then belongs to item 47's `refusal_thinks`, which review does not yet call.

## Spec
1. `review_grounded_findings`: the all-dropped case no longer returns `Err`. Instead: remove
   `findings` from the raw patch (the committed `findings` stay as they were), leave the rest
   of the patch, and return `Ok(Some(note))` with `pub const UNGROUNDED_FINDINGS_NOTE_PREFIX:
   &str = "Every finding was dropped for want of a file:line citation; none was recorded and
   the rest of your patch was kept. Name the line each finding is about, as `path:line: what
   is wrong`, and send them again. Dropped: "` followed by the same quoted, 60-char-capped,
   `; `-joined list the partial-drop note already builds. The function's signature may stay
   `Result<Option<String>, PatchError>` (nothing else returns `Err` today; keep it or simplify,
   your call, but say which).
2. In `ReviewSkill::validate`, when that all-dropped note fires, the action becomes
   `Action::Continue { note }` (a refusal) and `next_skill` is stripped from the raw patch
   (a send-back with no findings is not a handoff; same treatment `ask_user_empty_note` gives
   `next_skill`). Distinguish the all-dropped note from the partial-drop note without string
   matching: have the function return which case fired (an enum or a second bool), not the
   note text alone. The partial-drop case is unchanged: patch rewritten, action untouched.
3. Wire item 47's `refusal_thinks(cur, &mut out)` into `ReviewSkill::validate` at the end of
   the conversion chain, before `review_rounds_cap_note`, exactly as `ImplementSkill::validate`
   and `PlanSkill::validate` do (its note goes into the notes chain). No other change to
   `refusal_thinks`.

## Acceptance (exact strings; never loosen an existing assertion except the one named below)
- `skills/src/coding/tests.rs` unit tests: (a) all findings uncited: `Ok(Some(note))` with the
  exact prefix and list, `findings` absent from the raw patch, other patch keys kept, the
  returned case is the all-dropped one; (b) one cited, one not: the existing partial-drop
  behaviour and note, exact, case partial; (c) all cited: `Ok(None)`.
- `skills/tests/it/grounded_findings.rs`: `all_ungrounded_send_back_is_refused_and_state_is_
  unchanged` is rewritten to the new contract and renamed accordingly: the step commits as
  `continued` with the exact note, `findings` unchanged from before the step, the patch's
  other fields (whatever the fixture sends, e.g. `recent_summary`) applied, `next_skill`
  still `None`, and the retry count NOT consumed (assert the run is not parked after three
  such steps: the third is item 47's think or the cap, per its rules; assert the exact
  outcome you observe and say why). Add: two all-uncited send-backs in a row: the second
  commits as a `think` with `REFUSAL_QUESTION` filled with the exact refusal note.
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs` (export the
constant), `skills/tests/it/*.rs`. No core, tools, cli, bench/ or .md edits.
Report: files changed, exact constant, every existing test whose expectations changed and why,
commands run with exit codes.
