# Unit 29: a rejected tool call names the skill that has the tool, and repeats count

## Spec
1. `core/src/skill.rs`: add to the `Skill` trait a default method
   `fn skill_with_tool(&self, tool: &str) -> Option<String> { None }`. In
   `skills/src/coding/mod.rs`, implement it on `RouterSkill`, `PlanSkill`, `ImplementSkill`,
   and `ReviewSkill` with one shared helper `coding_skill_with_tool(tool) -> Option<&'static str>`
   that returns the coding skill whose `tools()` contains the name: `implement` for
   `edit_file`, `write_file`, `revert_file`, `test`, `try_fixes`, `shell`, `probe`; `review`
   for tools only review has; `None` for names no skill has (including the empty string).
   Only skills in the caller's `allowed_transitions()` are named; otherwise `None`.
2. `core/src/session.rs`, the inactive-tool rejection (search `"tool_rejected"`). The
   observation text becomes, exactly:
   - when `skill_with_tool` returns `Some(s)`: "tool `{name}` is not available in skill `{cur}`; `{s}` has it. Finish this skill's job, then hand off: {\"patch\": {\"next_skill\": \"{s}\"}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": {}}, \"ack_inbox\": []}"
   - when the name is empty: "action.name is empty: put the tool's name in `name` and its arguments in `args`; available tools: {active:?}"
   - otherwise (unknown name, no skill has it): "tool `{name}` is not available in skill `{cur}`; available tools: {active:?}. There is no tool by that name; if `{name}` is a path, it belongs in `args`."
   The `tool_rejected` log entry gains `"skill_with_tool": <s or null>`.
3. A rejected inactive-tool call joins the identical-tool-error streak the same way a tool
   error does (`HarnessState.last_tool_error`, `RepeatedToolError`, item 22/25): tool = the
   rejected name (empty string allowed), args = the call's args, reason = the observation text
   above, shape = `error_shape(reason)`. The existing second-strike `think`
   (`implement_repeated_error_thinks`, implement only) and third-strike park
   (`park_non_progress_cap` with the existing "repeated tool error: {name}" shape) apply
   unchanged. In `plan` and `router`, where no `think` runs, the park at the third strike is
   the whole effect.

## Acceptance
- `core/tests/it/tool_rejected.rs` (new module): (a) a skill whose `skill_with_tool` returns
  `Some("implement")` gets the exact first observation text; (b) empty name gets the exact
  second text; (c) unknown name gets the exact third text; (d) three identical rejected calls
  park with `non_progress_cap` and the shape string; a different-args rejected call in between
  does not reset the count (mirror `error_think.rs`/`non_progress.rs` semantics exactly as they
  exist for tool errors: read the code, assert what it does).
- `skills/tests/it/rejected_handoff.rs` (new module): with `plan` active, a scripted `edit_file`
  call is rejected with the observation naming `implement` and the exact hand-off shape;
  `coding_skill_with_tool("edit_file") == Some("implement")`, `("recall") == None` if recall is
  shared, `("") == None`.
- Existing green: `core/tests/it` full run, `skills/tests/it` full run, `cargo test -p statie-skills --lib`.

## Boundaries
`core/src/skill.rs`, `core/src/session.rs`, `core/src/version.rs` only if a field is needed,
`skills/src/coding/mod.rs`, `skills/src/lib.rs` (re-export the helper), the new test modules and
their `main.rs` lines, fixture literals the compiler flags. No other files. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie-models --test it`.
