# Unit 38: the thinker sends the model's configured `think` flag, not a hard-coded `true`

## Background (read, do not restate)
`models/src/thinker.rs` `ModelThinker` calls `OllamaModel::chat_free`/`chat_free_with` with
`think: true` at every reasoning call site (the unchunked call and the chunked loop's
reasoning chunks). `OllamaConfig` already carries `pub think: bool` (default `true`), which the
acting-model calls honour, and the Qwen bench configs set `"think": false` on the model
because Ollama answers `400 {"error":"\"qwen3-coder:latest\" does not support thinking"}` when
the flag is sent. The thinker ignores that field, so every `think` call in every Qwen battery
failed with that 400 (24 to 82 failures per battery, `bench-runs/qwen_json_short_b15..b19`).
The thinker is built in `cli/src/config.rs` from `ThinkerConfig.model` or, when that is `None`,
the session's `ModelConfig` (so the Qwen configs' `think: false` reaches the thinker's
`OllamaConfig` today; it is just never read).

## Spec
1. `models/src/ollama.rs`: `OllamaModel` gains `pub fn think(&self) -> bool` returning the
   configured `OllamaConfig::think`.
2. `models/src/thinker.rs`: every reasoning call that passes a literal `true` for `think`
   passes `self.model.think()` instead. Calls that deliberately pass `false` today (a
   summariser or carry-over call, if any) stay `false`. The doc comments on `ModelThinker`
   and any that say "`think: true`" are updated to say the flag follows the model's config.
   When the flag is `false` the reasoning arrives as ordinary `content`; the existing
   `ANSWER:` extraction and the chunked loop's carry-over must work on that path exactly as
   they do on the `thinking` path (find how the chunked loop reads the trace today: if it reads
   only the `thinking` field, it must read `content` when `think` is off, since the model then
   writes its reasoning there).
3. No config schema change: `ThinkerConfig` is untouched. No change to which model the
   thinker is built from.

## Acceptance
- `models/tests/it/thinker.rs` (using the existing mock Ollama server in `models/tests/it/`):
  (a) with `OllamaConfig { think: false, .. }` the request bodies of an unchunked `think` and of
  each chunk of a chunked `think` carry `"think": false` (assert the exact JSON value on every
  captured request); with `think: true` they carry `true` (assert the existing behaviour, exact).
  (b) with `think: false`, a mock reply whose reasoning and `ANSWER: <text>` are in `content`
  (no `thinking` field) yields `ThinkOutput` with the exact answer text and the exact
  reasoning-character count; same for the chunked loop over two chunks (exact answer, exact
  chunk count).
- `models/tests/it/ollama_model.rs`: `OllamaModel::think()` returns the configured value for
  both settings (exact).
- Existing green: `cargo test -p statie-models --test it`, `cargo test -p statie-models --lib`,
  `cargo test -p statie --bin statie`.

## Boundaries
`models/src/ollama.rs`, `models/src/thinker.rs`, `models/tests/it/thinker.rs`,
`models/tests/it/ollama_model.rs`, `models/tests/it/common/mod.rs` (only if the mock server
needs a request-capture helper it lacks). No other source files. No .md edits. No config files.
Allowed cargo: `cargo build -p statie-models`, `cargo test -p statie-models --test it`,
`cargo test -p statie-models --lib`, `cargo test -p statie --bin statie`.
