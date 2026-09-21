# Unit 52: a path-named call with only a path is a read

## Background (read, do not restate)
`infer_empty_tool_name` (`skills/src/coding/mod.rs`, units 32 and 42) already handles a call
whose `name` looks like a path (`looks_like_path`): it inserts the name as `path` if absent and
asks `infer_tool_name(&args)` for the tool. `infer_tool_name`'s rule 5 (unit 42) names
`read_file` only for `path` plus `offset`/`limit`; a bare `{"path": ...}` returns `None` by
design ("unit 42 only claims the offset/limit shape actually seen in the bench"). The bench has
now seen the bare shape: Qwen sends `{"name": "src/error.rs", "args": {"path": "src/error.rs"}}`
-- 12 such calls in 4 of 10 runs on `qwen_json_long_b33`, 11 in 5 runs on `_b28`, 5 in 3 runs
on `qwen_json_short_b33` -- and two of build 33's four long-task parks are
`non_progress_cap: repeated tool error: <not a tool>` on exactly this shape, repeated until the
cap because the model never gets a tool out of it.

## Spec
1. In `infer_empty_tool_name`'s path-named branch only (the branch after `looks_like_path`
   passes, NOT the empty-name branch): when `infer_tool_name(&args)` returns `None` and the
   args object, after the `path` insertion, has `path` as its only key (a string), infer
   `read_file` with those args. The note is the existing `INFERRED_TOOL_NOTE_PREFIX` +
   "`read_file`." exactly as the branch already formats it. A path-named call whose args carry
   any other key and still infer nothing stays `None` (unchanged).
2. The empty-name branch and `infer_tool_name` itself are unchanged: an empty `name` with a
   bare `{"path"}` still infers nothing (it could be a `write_file` missing `content`).
3. Update `infer_empty_tool_name`'s doc comment: one paragraph on the bare path-named shape
   with the evidence above.

## Acceptance (exact strings; never loosen an existing assertion)
- `skills/src/coding/tests.rs`: (a) `name: "src/error.rs"`, `args: {"path": "src/error.rs"}`,
  `read_file` active: action becomes `read_file` with args exactly `{"path": "src/error.rs"}`,
  note exact; (b) `name: "src/error.rs"`, `args: {}`: same result, `path` inserted; (c) `name:
  "src/error.rs"`, `args: {"path": "src/error.rs", "cmd": "ls"}`: unchanged (`None`, action
  untouched); (d) empty `name`, `args: {"path": "src/error.rs"}`: unchanged (`None`);
  (e) `name: "src/error.rs"` when `read_file` is NOT in `active` (e.g. the `router`'s empty
  list): find how the branch treats a tool the skill does not have (read the existing code and
  tests for the path-named branch) and assert that behaviour exactly, unchanged.
- `skills/tests/it/` end-to-end: in `implement`, the model sends the bare path-named call; the
  committed action is `read_file` and the observation is the file's contents (use the existing
  file-tools fixture, e.g. the one `premature_done.rs` or `skip_think.rs` uses).
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/tests/it/*.rs`. No core,
tools, cli, bench/ or .md edits.
Report: files changed, every existing test whose expectations changed and why, commands run with
exit codes.
