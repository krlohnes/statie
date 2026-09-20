# Unit 32: a tool call with no name is named by its arguments; an anchor miss names `lines`

## Spec
1. `skills/src/coding/mod.rs`: `pub fn infer_tool_name(args: &serde_json::Value) -> Option<&'static str>`:
   - `question` present (any other keys ignored) → `"think"`
   - `path` and `content` present, no `new` → `"write_file"`
   - `path` and `new` present, and exactly one of `old`/`after`/`before`/`lines` → `"edit_file"`
   - `pattern` present → `"search"`
   - otherwise `None`. Order as listed; first match wins.
   A trigger `infer_empty_tool_name(cur, out) -> Option<String>` run FIRST in `validate` of
   `PlanSkill`, `ImplementSkill`, and `ReviewSkill` (before `strip_harness_owned`): when the action
   is `Action::Tool { name, args }` with `name.trim().is_empty()` and `infer_tool_name(args)` is
   `Some(t)`, set the action's name to `t` (args unchanged) and return the note, exactly:
   `pub const INFERRED_TOOL_NOTE_PREFIX: &str = "The tool call had no name; from its arguments the harness ran "` followed by `` `{t}` ``
   and a period. Do not check whether `t` is active for the skill: the existing inactive-tool
   rejection (item 29) handles that with the right note. Re-export both from `skills/src/lib.rs`.
2. `tools/src/edit_file.rs`, the `CollapsedMatch::None` anchor error: the text becomes exactly
   "anchor line not found in {path}; re-read the file and copy the line(s) exactly (whitespace matters), or address the range with `lines: \"START-END\"` from the read window instead of copying it{hint}"
   (the `{hint}` suffix stays where it is). Update any test that pins the old text to the new
   exact text; do not loosen it.

## Acceptance
- `skills/tests/it/infer_tool.rs` (new module): with `implement` active, a scripted
  `{"type":"tool","name":"","args":{"question":"why?"}}` runs `think` (assert the thinker was
  called with that question, as `think_trigger.rs` does) and the step note is exactly the
  INFERRED text for `think`; `{"name":"","args":{"path":"a.rs","content":"x"}}` runs `write_file`
  (or hits its item 15 read-first substitution; assert whichever the code does, exactly);
  `{"name":"","args":{}}` stays empty and gets the item 29 empty-name rejection text; with `plan`
  active, `{"name":"","args":{"path":"a.rs","old":"a","new":"b"}}` becomes `edit_file` and is then
  rejected with the item 29 note naming `implement` (both notes present).
  Unit tests in `skills/src/coding/tests.rs` for `infer_tool_name` covering every arm and
  the `None` cases (`path` alone; `path`+`new` with two anchors; `cmd`).
- `tools/tests/it/edit_file.rs`: the anchor-miss test(s) updated to the exact new text.
- Existing green: `cargo test -p statie-skills --test it`, `cargo test -p statie-skills --lib`,
  `cargo test -p statie-tools --test it`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs`, `tools/src/edit_file.rs`,
the test modules named above and `skills/tests/it/main.rs`. No other files. No .md edits.
Allowed cargo: `cargo build -p statie-skills`, `cargo build -p statie-tools`,
`cargo test -p statie-skills --test it`, `cargo test -p statie-skills --lib`,
`cargo test -p statie-tools --test it`.
