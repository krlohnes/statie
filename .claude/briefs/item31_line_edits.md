# Unit 31: line-addressed edits, and whole-file content is routed to write_file

## Spec
1. `tools/src/edit_file.rs`: `EditFileArgs` gains `#[serde(default)] pub lines: Option<String>`
   ("START-END" or "N", 1-based, inclusive) and `#[serde(default)] pub content: Option<String>`
   (accepted only so the error below can name it; never applied). `new` becomes
   `#[serde(default)] pub new: Option<String>` so the shape errors below can be produced by
   the tool rather than by serde. Behaviour:
   - exactly one of `old`, `after`, `before`, `lines` set, and `new` present: as today for the
     first three; for `lines`, replace lines START..=END (END defaults to START) with `new`
     (no trailing-newline surprises: the replaced block keeps the file's newline convention),
     write atomically as the other forms do, and return the same success header shape
     `edited {path}: replaced lines {start}-{end}` plus the existing post-edit window.
     `lines` out of range → `ToolError::InvalidArgs("lines {start}-{end} is outside {path} ({n} lines)")`.
   - `content` set and none of the four addressing fields: `ToolError::InvalidArgs` with exactly:
     "`edit_file` changes part of a file: give `new` with one of `old` (exact text to replace), `after`/`before` (an anchor line), or `lines` (\"START-END\" as shown by read_file). To replace the whole file, call `write_file` with `path` and `content`."
   - `new` present and no addressing field: the same text.
   - more than one addressing field: "exactly one of `old`, `after`, `before`, `lines` must be set".
   - `new` missing: "`new` is required: the replacement text (with `old` or `lines`) or the text to insert (with `after`/`before`)".
   Update the tool `description()` to name the four forms in one sentence.
2. `skills/src/coding/mod.rs`, `ImplementSkill::validate`: a new trigger `edit_lines_needs_fresh_read`,
   run right after `write_file_auto_confirm` (item 15) and using the same per-file freshness
   condition it uses (`SeenFile.read_step` against `edited`; read that code and reuse the helper,
   do not duplicate the condition): an `edit_file` with `lines` on a file NOT read since its last
   edit is substituted with `read_file {path, offset: START, limit: END-START+1 (min 1)}` and the note
   `pub const EDIT_LINES_STALE_NOTE: &str = "`lines` addresses the file as last read, and {path} was edited since; the harness read that range again. Send the edit against the fresh window."`
   with `{path}` filled. Re-export from `skills/src/lib.rs`.

## Acceptance
- `tools/tests/it/edit_file.rs` (exists? add to it; else new module): replace a single line, a
  range, the first line, the last line (newline preserved), out-of-range error text, `content`
  error text, `new`-only error text, two-forms error text, missing-`new` error text. All exact.
- `skills/tests/it/edit_lines.rs` (new module): after a read then an edit then no re-read, an
  `edit_file {lines}` becomes the `read_file` of that range with the exact note; after a re-read
  the same edit runs. Copy the setup from the item 15 tests (`write_file` auto-confirm).
- Existing green: `cargo test -p statie-tools --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie-skills --lib`, `cargo test -p statie-core --test it`.

## Boundaries
`tools/src/edit_file.rs`, `skills/src/coding/mod.rs`, `skills/src/lib.rs`, the test modules
named above and their `main.rs` lines, any existing test that pins the old error text (update to
the new exact text). No other files. No .md edits.
Allowed cargo: `cargo build -p statie-tools`, `cargo build -p statie-skills`,
`cargo test -p statie-tools --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie-core --test it`.
