# Standing rules for every brief (paste verbatim at the top of the agent prompt)

- Work in your own git worktree. First overlay the base tree:
  `rsync -a --exclude target --exclude .git --exclude .claude --exclude bench-runs <base>/ ./`
- Git: read-only. No add, commit, stash, checkout, reset, or worktree commands.
- Do not edit README.md, ARCHITECTURE.md, statie-spec.md, RELATED-WORK.md,
  BACKLOG.md, DECISIONS.md. Do not touch /private/tmp, /Users/keith/projects/statie-dogfood,
  bench-runs/, .claude/, or bench/ unless the unit says so. No `cargo install`.
- Do not call the model host (192.168.1.222:11434) for any reason.
- Verification in the foreground only: `cargo build -p <crate>` for the crates you
  touched and `cargo test -p <crate> --test it <module>::` for the tests you wrote or
  changed (integration tests live in `<crate>/tests/it/<module>.rs`, one
  binary per crate, DECISIONS.md D18; a new test file is a new `mod` line in
  `tests/it/main.rs`, never a new file directly under `tests/`). No workspace-wide test, clippy, or fmt runs; no background cargo; no
  polling loops. The arbiter runs the single workspace gate.
- Tests must assert the behaviour (exact strings, exact steps, exact counts), not
  `is_some()` / no-panic. Never loosen an existing assertion.
- Report: files changed, the exact commands run, and their exit codes.
