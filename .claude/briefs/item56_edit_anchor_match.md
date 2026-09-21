# Unit 56: `edit_file` locates `old` by its first and last lines when the verbatim text is not found

## Background (read, do not restate)
`tools/src/edit_file.rs`: `old` must match the file verbatim; otherwise the tool fails with
`OLD_NOT_FOUND_PREFIX` + path + `OLD_NOT_FOUND_SUFFIX`, plus the "closest match is near line N"
window. Qwen reproduces long `old` blocks from memory with small drift (a wrapped doc line, a
trailing space) and then loops: 17, 35 and 23 such errors on the Qwen long batteries of builds
33, 34 and 35, in 4 to 6 of 10 runs each; three of build 35's four long-task misses ended at the
`repeated tool error: edit_file` cap on this error, the stall `think` in between not helping.
In every case seen the block's first line and last line were right and the drift was inside.
`new` replaces the whole block, so `old`'s interior only serves to locate it.

## Spec
1. In `edit_file`'s `old`-anchored path (find where the verbatim search fails and the not-found
   error is built), before failing, try two tolerant matches in order:
   a. Whitespace-normalised: compare `old` and the file with each line right-trimmed and runs
      of spaces/tabs inside a line collapsed to one space. A unique match replaces the
      corresponding original span.
   b. Anchored: `old` has at least 3 lines; let `first` and `last` be its first and last
      non-empty lines, trimmed. Find every pair (i, j), i < j, where file line i trimmed equals
      `first`, file line j trimmed equals `last`, and `j - i + 1` is within `old`'s line count
      minus 3 to plus 3. Exactly one such pair -> replace file lines i..=j with `new`. Zero or
      more than one -> fall through to today's error.
   On a tolerant match the edit applies exactly as a verbatim edit does (same `seen` update,
   same success output) with one added line at the end of the tool's success output:
   `pub const OLD_MATCHED_LOOSELY_SUFFIX: &str = " (old matched after whitespace
   normalisation)"` for (a), or `pub const OLD_MATCHED_BY_ANCHORS_PREFIX: &str = " (old matched
   by its first and last lines at "` + `{start}-{end}` + `"; {n} interior line(s) differed from
   the file)"` for (b), where `n` is the count of interior lines of `old` not equal (trimmed)
   to the corresponding file lines. Keep those two consts `pub` for tests.
2. `lines`/`after`/`before` anchored edits are unchanged. The verbatim match still wins when
   it exists (no behaviour change for today's successful edits).
3. Update the tool description (`tools/src/edit_file.rs`, ≤ 130 words rule from item 40): one
   clause saying an `old` block that drifted inside is matched by its first and last lines.

## Acceptance (exact strings; never loosen an existing assertion)
- `tools/src/edit_file.rs` unit tests (or `tools/tests/it/`, follow the crate's convention):
  (a) verbatim match unchanged (existing tests); (b) `old` with a trailing space and a
  double-space inside matches by (a) and the output ends with the exact suffix; (c) a 6-line
  `old` whose line 3 differs from the file matches by (b) at the right span, the file after
  the edit is exact, the output ends with the exact anchors line with `n = 1`; (d) two
  candidate spans -> today's error, exact prefix and suffix; (e) `old` of 2 lines with drift ->
  today's error; (f) span length outside ±3 -> today's error.
- Existing green: `cargo test -p statie-tools`, `cargo clippy -p statie-tools --all-targets --
  -D warnings`, `cargo fmt -p statie-tools`, `cargo test -p statie-skills --test it` (unchanged).

## Boundaries
`tools/src/edit_file.rs`, `tools/src/lib.rs` (exports), the tools crate's tests. No core,
skills, cli, bench/ or .md edits.
Report: files changed, exact constants, every existing test whose expectations changed and why,
commands run with exit codes.
