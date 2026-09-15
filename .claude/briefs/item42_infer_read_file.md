# Unit 42: a nameless call with `read_file`'s arguments is a `read_file` call

## Background (read, do not restate)
`skills/src/coding/mod.rs` `infer_tool_name(args)` (item 32) returns `think`, `write_file`,
`edit_file`, or `search` from the argument keys, and its caller applies it only when the call's
`name` is empty, adding `INFERRED_TOOL_NOTE_PREFIX` to the notes. Bench `qwen_json_long_b24`:
106 tool calls with no usable name; 72 had an empty `name` with `path`, `offset`, `limit`; 12 had
a file path as `name` with `{path}` args; 3 a path as `name` with `path`/`offset`/`limit`; 2 a path
as `name` with `old`/`new`/`path`. Six of ten runs parked on `repeated tool error: <not a tool>`.

## Spec
1. `infer_tool_name`: after the existing rules (order unchanged), if `args` has `path` and at
   least one of `offset`/`limit`, and none of `content`, `new`, `pattern`, return `read_file`.
   A bare `{path}` (or `{}`) still returns `None`.
2. The caller that applies the inference (find it by `INFERRED_TOOL_NOTE_PREFIX`) also runs
   when `name` is non-empty, is not one of the skill's active tools, and looks like a path:
   contains `/`, or ends with `.` followed by 1 to 4 ASCII alphanumerics. In that case, if `args`
   is an object without a `path` key, insert `"path": <name>` first, then infer. If inference
   returns a tool, the action's `name` becomes that tool and the note reads exactly
   `{INFERRED_TOOL_NOTE_PREFIX}`{tool}` (the name was `{original}`).` where `{original}` is the
   path that was in `name`. If inference returns `None`, the action is left as it was (the
   existing rejection handles it).
3. Names that are an active tool of the skill are never touched. Nothing else changes.

## Acceptance
- `skills/src/coding/tests.rs` (beside the existing `infer_tool_name` tests): `{"path":"a",
  "offset":10,"limit":50}` → `Some("read_file")`; `{"path":"a","limit":20}` → `Some("read_file")`;
  `{"path":"a"}` → `None`; `{"path":"a","offset":1,"new":"x","old":"y"}` → `Some("edit_file")`
  (existing rule wins); `{"path":"a","offset":1,"content":"x"}` → `Some("write_file")`.
- `skills/tests/it/` (the module testing item 32's inference; find by
  `INFERRED_TOOL_NOTE_PREFIX`): (a) an `implement` step with `name: "src/lib.rs"`, `args:
  {"offset": 1, "limit": 40}` runs `read_file` on `src/lib.rs` with the exact note; (b) `name:
  "src/lib.rs"`, `args: {"path": "src/lib.rs", "old": "a", "new": "b"}` runs `edit_file` with the
  exact note; (c) `name: "src/lib.rs"`, `args: {}` is rejected with today's exact not-available
  text (no inference, no note); (d) `name: "read_file"` with `{"path":"a","limit":5}` is
  untouched (no note). Assert the committed action's `name`/`args` and the notes exactly.
- Existing green: `cargo test -p statie-skills --test it`, `cargo test -p statie-skills --lib`,
  `cargo test -p statie --bin statie`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/tests/it/*.rs`, and any test
the compiler or a failing assertion names (updated to exact new text). No core or tools
changes. No .md edits.
Allowed cargo: `cargo build -p statie-skills`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie --bin statie`.
