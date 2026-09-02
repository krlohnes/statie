# statie

statie is a mutable-state, interactive coding harness for local LLMs. Instead
of growing a transcript on every turn, it keeps one bounded prompt per step —
an immutable skill spec, the current structured workspace state, the latest
tool observation, and optional retrieval augmentation — and asks the model
to emit a single JSON patch plus an action (run a tool, ask the user, yield,
or finish). The model's own reasoning never re-enters the prompt; the full
session history is persisted to disk and searchable by the model through a
`recall` tool, but prompt size stays flat whether the session is 5 steps or
500.

The binary in this crate (`statie`) is the thing you actually run: it loads
a JSON config, wires together a model backend, a persistence store, a set of
tools, and retrieval augmenters into a `Session`, and drives it either
through the `statie-tui` terminal UI or headlessly for scripting and CI.
Nothing about which model server to talk to, where state lives, or how big
the prompt budget is is hardcoded anywhere in the binary — all of it comes
from the config file described below, the same way you'd configure any
other agentic coding harness.

## Quick start

```sh
cargo run -- config init      # writes ./.statie/config.json with the defaults
$EDITOR .statie/config.json   # at minimum, point base_url at your model server
cargo run -- check            # verifies the model backend is reachable and reports its capabilities
cargo run -- run              # starts a session (TUI by default)
```

`cargo run -- config init --global` writes to
`~/.config/statie/config.json` instead, so the config applies to every
project you run `statie` from unless a project-local config overrides it.

## Configuration

statie reads a single JSON config file. Resolution order, first hit wins:

1. `--config <path>`
2. `$STATIE_CONFIG`
3. `./.statie/config.json`
4. `~/.config/statie/config.json`
5. built-in defaults (no file at all)

Every field has a default, so a partial file — even `{}` — is valid; any
key not recognized (a typo like `"modle"`) is a hard error rather than being
silently ignored. `statie.example.json` at the repo root is the full default
config with every key spelled out; `statie config show` prints the config
statie would actually use, merged with whichever file it resolved.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `model.backend` | string | `"ollama"` | `"ollama"`, `"openai"`, or `"mock"`. `"openai"` talks to any OpenAI-compatible chat completions server (vLLM, llama.cpp server, LM Studio, ...). `"mock"` replays `model.mock_script` — used for tests and CI, never talks to the network. |
| `model.base_url` | string | `"http://localhost:11434"` | Base URL of the model server. Ollama default shown; for `"openai"` backend a typical value is `"http://localhost:8000/v1"`. |
| `model.model` | string | `"gemma4:26b"` | Model name/tag as the backend expects it. |
| `model.think` | bool | `true` | Ask Ollama for native `thinking` output (`"think": true` on `/api/chat`). Ignored by other backends. |
| `model.api_key` | string or null | `null` | Bearer token for `"openai"` backend. Falls back to `$STATIE_API_KEY` if unset. |
| `model.num_ctx` | integer | `32768` | Context window sent on every Ollama request. Pinned on purpose: Ollama reloads the model (and re-allocates its KV cache) whenever a request uses a different `num_ctx`, so keep this constant and match any other client of the same server. |
| `model.format` | string | `"json"` | Ollama `format` mode: `"schema"` (full JSON schema grammar), `"json"` (plain JSON mode) or `"none"`. Default is `json`: on Ollama 0.33 with gemma4 the full-schema grammar degraded output (the model collapsed to `yield`/`done` while its reasoning said to call a tool). Every output is validated against the schema regardless. |
| `model.schema_constrained` | bool or null | `null` | Whether the backend enforces the schema; `null` derives it from `format` (Ollama) or is `true` (OpenAI-compatible). When false the schema is also pasted into the system prompt. |
| `model.options` | object | `{}` | Passed through verbatim: Ollama's `options` object, or merged into the OpenAI-compatible request body (e.g. vLLM's `guided_json`). |
| `model.timeout_secs` | integer | `600` | Per-request timeout. |
| `model.mock_script` | array of `{reasoning?, content, prompt_tokens?, completion_tokens?}` | `[]` | Scripted turns for `backend: "mock"`. The last turn repeats once the script is exhausted. `prompt_tokens`/`completion_tokens` (both must be set together) script that turn's server-reported `Usage`, standing in for what a real backend's `usage`/`prompt_eval_count` would report. |
| `budget.total` | integer | `24000` | Token budget for the whole prompt. |
| `budget.observation` | integer | `3000` | Cap on the latest tool-observation section (head/tail truncated beyond this). |
| `budget.augmentation` | integer | `2000` | Combined cap across all augmenters. |
| `retries` | integer | `3` | Rejected model outputs tolerated before the session parks with a failure, asking the user to intervene. |
| `store.kind` | string | `"sqlite"` | `"memory"` or `"sqlite"`. |
| `store.path` | path | `.statie/session.db` | SQLite database path, resolved relative to `sandbox_root`. |
| `session_log` | path | `.statie/session.jsonl` | Append-only JSONL session log (searchable via BM25), resolved relative to `sandbox_root`. |
| `sandbox_root` | path | `"."` | Root all tool file access and the repo augmenter are confined to. |
| `augmenters.repo_bm25.enabled` | bool | `true` | BM25 retrieval over source files under `sandbox_root`. |
| `augmenters.repo_bm25.budget` | integer | `1200` | Token budget for this augmenter. |
| `augmenters.session_log_bm25.enabled` | bool | `true` | BM25 retrieval over the session log (tool output, user messages, system events — never reasoning). |
| `augmenters.session_log_bm25.budget` | integer | `800` | Token budget for this augmenter. |
| `tools.shell_timeout_secs` | integer | `120` | Default timeout for the `shell` and `test` tools. |
| `tools.allow_shell` | bool | `true` | Whether the `shell` and `test` tools are registered at all. |
| `tools.max_output_bytes` | integer | `200000` | Per-tool output cap before truncation. |
| `max_autonomous_steps` | integer | `200` | Safety cap on consecutive autonomous steps (`Outcome::Continue`) before a run stops on its own. |

Config files never contain a hardcoded model endpoint by design — everything
above is read from the file (or its defaults), and `check`/`run` build the
model backend purely from `model.*`.

## Commands

- `statie config init [--global] [--force]` — writes the default config to
  `./.statie/config.json` (or `~/.config/statie/config.json` with
  `--global`); refuses to overwrite an existing file unless `--force` is
  given.
- `statie config show` — prints the effective config (defaults merged with
  whichever file was resolved) as pretty JSON, and the path it came from
  (or `defaults` if none was found).
- `statie check` — builds the configured model backend and prints its
  `capabilities()` (native thinking, schema-constrained decoding, prefix
  caching). For the `ollama` backend it also queries `GET
  {base_url}/api/tags` and reports whether `model.model` is present on the
  server.
- `statie run [--skill <name>] [--headless] [--message <text>]... [--memory]`
  — builds a session for the chosen skill and runs it. `--skill` selects the
  starting activation: `router` (the default), `plan`, `implement`, and
  `review` all share the coding workspace and its tools (read, write,
  shell, test, recall — `statie_tools::default_tools`); `toy` runs the toy
  skill instead. `--message` seeds the session's inbox with one or more
  initial user messages, in order — for the coding skills, the first
  message is the task the router copies into the workspace. `--memory`
  forces an in-memory store and log — nothing touches disk, useful for
  tests and one-off scratch runs.
- `statie eval [--task <toy|coding>] [--steps N] [--json] [-v|--verbose]`
  — runs the same task through two harnesses built from the same config:
  **statie** (the config's own bounded budgets) and **baseline** (an
  append-only harness that re-injects the entire prior transcript into the
  prompt every step, via `TranscriptAugmenter`, with an effectively
  unbounded budget). Both run headlessly, in isolated temp sandboxes, and
  report `steps`, `success`, and two families of prompt-token stats as a
  table or (`--json`) a JSON object: `est_*` (`prompt_tokens`,
  `total_prompt_tokens`, `max_prompt_tokens`, `growth`, printed as
  `est_total`/`est_max`/`est_growth`) is the local `TiktokenTokenizer`
  (cl100k) estimate of prompt size; `srv_*` (`server_prompt_tokens`,
  `server_total_prompt_tokens`, `server_max_prompt_tokens`,
  `server_total_completion_tokens`, `server_growth`) is the backend's own
  reported token usage for each model call — Ollama's `prompt_eval_count`/
  `eval_count`, or an OpenAI-compatible server's `usage` object — present
  only when the backend reports it (the printed table shows `-` in the
  server columns otherwise, e.g. for the `mock` backend with no scripted
  usage). `srv_max` is the number to compare against `model.num_ctx`, since
  it comes from the model's own tokenizer rather than the local estimate.
  `--task toy` drives `ToySkill` up to `--steps` (default 10) with the
  `echo` tool; `--task coding` asks the router to create `hello.txt` with a
  fixed line of content and checks the sandbox for it. Exits non-zero only
  on an actual error — a harness failing the task is reported, not treated
  as a CLI failure. Pair with a `model.backend: "mock"` config for a fully
  offline, deterministic comparison.

### `--headless`

By default `run` launches the `statie-tui` terminal UI. `--headless` instead
drives the session loop to completion (or until it parks) without a
terminal UI, writing to stdout, and exits `0` when the run reaches `done` or
parks (e.g. on `AskUser`/`Yield`). It prints a final summary line:

```
steps=N outcome=<done|parked|...> prompt_tokens=[…]
```

This is the mode to use in CI or scripts — pair it with `--memory` and a
`model.backend: "mock"` config for fully offline, deterministic runs.

## Running against Ollama: field notes

Findings from live runs of gemma4:26b on Ollama 0.33.2, each of which is now
either a default or a config knob:

- **Pin `model.num_ctx`.** Ollama sizes the runner's KV cache from `num_ctx`
  and reloads the model whenever a request uses a different value. statie
  sends `model.num_ctx` on every request; keep it constant and equal to the
  server's `OLLAMA_CONTEXT_LENGTH` (a 262k server default allocates a cache
  several times larger than the weights and fails to load). `num_ctx` inside
  `model.options` is rejected at config load.
- **Prefer `model.format: "json"`.** With the full JSON-schema `format`, gemma4
  collapsed to `yield`/`done` while its reasoning said to call a tool; plain
  JSON mode produced the correct tool call from the identical prompt. Every
  output is validated against the schema regardless, and the schema is pasted
  into the system prompt when `schema_constrained` is false.
- **Lower the temperature for agentic use** via `model.options`, e.g.
  `{"temperature": 0.3}`; the gemma4 Modelfile defaults to 1.0.
- **Keep transcripts out of the sandbox.** `RepoBm25` indexes the sandbox
  root; a headless transcript written there is fed back to the model as
  "relevant snippets" (including its own past actions). Point `sandbox_root`
  at the repository and write logs elsewhere.
- **Loops on empty observations.** The executor renders empty tool output as
  `(tool X returned no output)`, treats `args: null` as `{}`, and flags the
  third identical call with identical output so the model is told nothing
  changed.
- **Trust the server's own token counts over the local estimate.** statie's
  prompt-size numbers (`est_*` in `eval`, `last_prompt_tokens()`) come from
  `TiktokenTokenizer`'s cl100k estimate, which is only an approximation of
  whatever tokenizer the model actually uses. On the `done: true` line,
  Ollama additionally reports `prompt_eval_count`/`eval_count`; with
  Ollama's prefix cache, `prompt_eval_count` was observed (0.33.2) to be the
  *full* prompt token count, not just the newly-evaluated suffix, so it is
  directly comparable to `model.num_ctx`. statie surfaces these as `srv_*`
  wherever `est_*` appears (`eval`'s output, the headless transcript, the
  session log's `Patch`/`rejected` entries) — compare `srv_max` against
  `num_ctx` to see how close a run got to overflowing the context window,
  since it reflects the real tokenizer rather than the cl100k stand-in.

Live result on the `eval --task coding` task (create a file end to end via
router → plan → implement → review): both harnesses succeeded in 11 steps;
statie's prompt peaked at 2906 tokens with 478 tokens of growth (workspace
state, not history) versus 3671 and 1173 for the append-only baseline.

## Architecture

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the binding contract between
crates (types, traits, the session loop algorithm, and the type-level
invariants), and [statie-spec.md](./statie-spec.md) for the original design
plan it refines.
