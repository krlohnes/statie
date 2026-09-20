# Unit 12: grounded review findings

## Spec
In the `review` skill (`skills/src/coding/mod.rs`, `ReviewSkill`), a send-back
is `patch.done == true` with non-empty `findings`. Add grounding: each finding
must cite a location the harness can check.

1. A finding is *grounded* when it contains `path:line` where `path` is a file
   present in the sandbox (workspace `files` or the seen map) and `line` is
   within the file's current line count. Parse the first `<path>:<digits>`
   token; paths are relative to the sandbox root.
2. On send-back, ungrounded findings are dropped. If any were dropped, the
   review note says which ones and why, e.g.
   `Dropped 2 findings with no file:line citation. Every finding must name the line it is about.`
   If every finding is dropped, the send-back is refused: the patch is
   rejected with that note and the review continues (same shape as the other
   review refusals, e.g. `review_blind_approval_runs_test`).
3. Config switch `guards.grounded_findings` (bool, default true) in
   `cli/src/config.rs`, threaded the same way as `guards.decision_field`.
   Off restores current behaviour exactly.
4. Nothing changes for approvals (empty `findings`).

## Acceptance
- New integration test `skills/tests/grounded_findings.rs`: grounded finding
  passes through unchanged; ungrounded finding dropped with the exact note text;
  all-ungrounded send-back refused and the state is unchanged except the note;
  out-of-range line counts as ungrounded; switch off passes everything through.
- Unit tests for the parser next to `state_fields` tests in `skills/src/coding/tests.rs`.
- Existing review tests in `skills/tests/review_*.rs` unchanged and green.

## Boundaries
Only `skills/src/coding/{mod,tests}.rs`, `skills/tests/grounded_findings.rs`,
`cli/src/config.rs` (the switch), and the guard struct where `decision_field` lives.
