# Unit 40: tool descriptions state their forms and failure texts up front

## Background (read, do not restate)
statie's tool descriptions (`Tool::description`, `tools/src/*.rs`) are one sentence each;
the rules a call must follow reach the model only as a rejection after it breaks them
(e.g. `edit_file`'s "old text not found ... (whitespace matters)", "`edit_file` changes part of a
file: give `new` with one of ...", `write_file`'s "exists with N lines; write_file replaces the
whole file ...", "invalid tool arguments: missing field `path`"). OpenCode's edit tool
description (231 words) states the same class of rules and its exact failure texts before the
first call, and its batteries show fewer anchor misses. The descriptions are rendered in the
prompt's system block (`core/src/prompt.rs`, the Tools section), so longer text costs the
cached prefix only.

## Spec
1. Rewrite `description()` for `edit_file`, `read_file`, `write_file`, `search`, `list_files`,
   and `test` (`tools/src/test_tool.rs`). Each description, in this order: what the tool does
   (one sentence); the argument forms (every argument name, which are required, which are
   alternatives to each other); the one or two rules batteries show broken, stated as rules;
   the exact text the tool returns when the form is wrong, quoted. At most 130 words each.
   The failure texts must be the same strings the tool produces: lift each into a
   `pub const` in the tool's module (or a shared `tools/src/common.rs` const when two tools
   share it) and build both the error and the description from the const (D13), so they cannot
   drift. Keep the format strings' placeholders out of the description (quote the fixed part).
2. Specifically: `edit_file` names the three forms (`old`/`new`, `after`|`before`/`new`,
   `lines: "START-END"`/`new`), says `old` and anchors must be copied byte-exact from a
   `read_file` window including leading whitespace, says an anchor must match exactly one line,
   says `content` is not an argument (whole-file writes are `write_file`), and quotes the
   not-found text and the wrong-form text. `read_file` names `path`, `offset`, `limit`, says the
   output ends with the next offset, says a second read of an unchanged range is refused and
   quotes that refusal's fixed prefix. `write_file` names `path`, `content`, says it replaces the
   whole file, and quotes the large-file refusal's fixed part. `search` names `pattern`, `path`,
   says the pattern is a regular expression and the output form. `list_files` names `path`.
   `test` names `cmd` (optional) and says what a run with no `cmd` does.
3. No behaviour change in any tool: same arguments, same errors, same outputs. No change to
   any note or guard text outside `tools/`.

## Acceptance
- `tools/tests/it/<tool>.rs` for each of the six: a test that asserts the description contains
  each argument name and the exact const failure text(s) (`assert!(desc.contains(CONST))` with
  the const imported, plus `assert!(desc.split_whitespace().count() <= 130)`), and one that
  asserts the tool's actual error on a wrong call equals the const-built text it did before
  (exact string, using the existing error tests where they exist; do not weaken them).
- Every existing test that snapshots a description or a rendered Tools section (search
  `tests/` for "Edit a file in place", "Read a window of a text file", "Write a text file",
  "Search file contents", "Run the project's test suite") is updated to the exact new text.
- Existing green: `cargo test -p statie-tools --test it`, `cargo test -p statie-core --test it`,
  `cargo test -p statie-skills --test it`, `cargo test -p statie --bin statie`.

## Boundaries
`tools/src/*.rs`, `tools/tests/it/*.rs`, and any test in `core/tests/it`, `skills/tests/it`,
`cli/` that the compiler or a failing assertion names (updated to exact new text). No skills or
core source changes. No .md edits.
Allowed cargo: `cargo build -p statie-tools`, `cargo test -p statie-tools --test it`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie --bin statie`.
