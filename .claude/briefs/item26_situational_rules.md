# Unit 26: situational rules, the rules text follows the last action

## Why
Procedural Graphs (RELATED-WORK.md): localised guidance (the 2-hop neighbourhood
of the last action) beat injecting the whole procedure text by 9 points and 71%
fewer tokens; whole-text injection hurt. Our coding skills render their whole
rules block every step. This unit builds the localised form behind a switch so a
battery can compare the two.

## Constraint
`SkillSpec` is immutable per activation (C1, `core/src/skill.rs`). Situational
text therefore lives in a per-step prompt section, not in the spec.

## Spec
1. `core/src/skill.rs`: a new trait hook
   `fn situational(&self, _cur: &CommittedState<Self::Workspace>) -> Option<String> { None }`.
   `core/src/session.rs`: when building each step's prompt, call it and, if
   `Some`, render it as its own section headed `## Now` placed right after the
   harness view, truncated to the same budget rule as notes. Log its token count
   in the step's `usage`-style payload if there is an obvious place; otherwise skip.
2. `skills/src/coding/procedure.rs` (new): a static procedure graph for the
   coding skills. Nodes are situations derived from `cur.harness` and the
   workspace: `start` (no tool call yet this activation), `read`, `search`,
   `edit_ok`, `edit_error`, `test_pass`, `test_fail_compile`, `test_fail_assert`,
   `test_fail_other`, `think_done` (a `think` ran since the last tool call),
   `skipped` (`last_skip` set), `stalled` (`last_stall` set). Edges carry
   `condition`, `guidance`, `pitfalls` strings. The text is MOVED from the
   existing rules bullets of `ImplementSkill`, `PlanSkill`, and `ReviewSkill`:
   every bullet that is about "what to do after X" becomes an edge from X;
   bullets that hold in every situation stay in the spec. Keep the bullets'
   wording; do not invent new rules. `pub fn situation(cur) -> Node` and
   `pub fn render(node) -> String` (the node's outgoing edges and their targets'
   outgoing edges, 2 hops, as bullets, at most 10 lines).
3. Switch: `guards.situational_rules` (bool, default false) in `core/src/guards.rs`
   and `cli/src/config.rs`, threaded like `grounded_findings` into the three
   skills. Off: nothing changes anywhere (spec text identical to today, no `## Now`
   section). On: the spec's instructions are the universal bullets only and
   `situational()` returns `render(situation(cur))`.
4. The plan-time `think` options sentence, the `try_fixes` sentence, and the
   `probe` sentence are edges (from `test_fail_*` and `start`), not universal.

## Acceptance
- `skills/tests/it/situational.rs` (new module in `tests/it/main.rs`): with the
  switch on, (a) after a failing assertion `test` the prompt (via
  `MockModel::prompts()`) contains a `## Now` section that includes the
  failure-handling bullet and does not include the after-edit bullet;
  (b) after a successful `edit_file` it includes "run `test`" guidance and not
  the failure bullet; (c) the step-3 prompt is smaller in tokens than the same
  step with the switch off (build both sessions, same scripted turns; assert
  the on-prompt's token count is strictly lower). With the switch off (default),
  (d) no prompt contains `## Now` and the spec text equals today's verbatim
  (snapshot the current instructions string in the test before you move any
  bullet, and assert equality with the switch off).
- Unit tests in `procedure.rs`: `situation()` for each node from a hand-built
  `HarnessState`; `render()` returns at most 10 lines.
- Every existing test green with the switch off. Do not weaken any assertion.

## Boundaries
`core/src/skill.rs`, `core/src/session.rs`, `core/src/guards.rs`, `core/src/prompt.rs`
(only if a section type is needed), `cli/src/config.rs`, `cli/src/coding.rs`,
`skills/src/coding/{mod.rs,tests.rs}`, `skills/src/coding/procedure.rs` (new),
`skills/src/lib.rs`, `skills/tests/it/{main.rs,situational.rs}`.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it <module>::`,
`cargo test -p statie-core --test it <module>::`, `cargo test -p statie --bin statie config`.
