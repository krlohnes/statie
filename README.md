# statie

![statie](statie-code.png)

statie is a mutable-state, interactive coding harness for local LLMs, written
in Rust. It implements the SKILL.state architecture from
[*SKILL.state* (Badhe, Tiwari, Chung — arXiv:2608.26263, EMNLP 2026)](https://arxiv.org/abs/2608.26263),
adapted for an interactive terminal workflow instead of batch skill
execution.

The idea: instead of growing a transcript on every turn, the model gets one
bounded prompt per step — an immutable skill spec, the current structured
workspace state, the latest tool observation, and a little retrieval — and
answers with a single JSON patch to that state plus one action (call a tool,
ask the user, or finish). Its reasoning never re-enters the prompt. The full
history is persisted and searchable through a `recall` tool, but prompt size
stays flat whether the session is 5 steps or 500.

Everything about which model server to talk to, where state lives, and how
big the budgets are comes from a JSON config file, the way you'd configure
any other agentic coding harness. Nothing is hardcoded.

## Quick start

```sh
cargo install --path cli                 # installs `statie` into ~/.cargo/bin
statie config init --global              # writes ~/.config/statie/config.json
$EDITOR ~/.config/statie/config.json     # point model.base_url at your server
statie check                             # is the server reachable, is the model there?

cd ~/some/project
statie                                   # TUI, fresh session
statie "add a --json flag to the CLI"    # TUI, seeded with a first message
statie -p "add a --json flag to the CLI" # headless: run to completion, print, exit
statie -r                                # resume the session in ./.statie/
```

A working config for gemma4:26b on Ollama looks like this (the rest of the
keys take their defaults):

```json
{
  "model": {
    "backend": "ollama",
    "base_url": "http://192.168.1.222:11434",
    "model": "gemma4:26b",
    "num_ctx": 16384,
    "timeout_secs": 1200,
    "options": { "temperature": 0.3 }
  },
  "budget": { "total": 12000, "observation": 2000, "augmentation": 1000 }
}
```

`sandbox_root` defaults to the current directory, so run `statie` from the
project you want it to work on. Session state lives in `./.statie/`, which
writes its own `.gitignore`.

## Configuration

statie reads a single JSON config file. Resolution order, first hit wins:

1. `--config <path>`
2. `$STATIE_CONFIG`
3. `./.statie/config.json`
4. `$XDG_CONFIG_HOME/statie/config.json`, defaulting to
   `~/.config/statie/config.json` (a file in the platform config dir, e.g.
   `~/Library/Application Support/statie/config.json` on macOS, is honoured
   as a fallback)
5. built-in defaults (no file at all)

Every field has a default, so a partial file — even `{}` — is valid; an
unrecognized key (a typo like `"modle"`) is a hard error rather than being
silently ignored. `statie.example.json` at the repo root is the full default
config with every key spelled out; `statie config show` prints the config
statie would actually use and where it came from.

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

## Commands

- `statie [OPTIONS] [PROMPT]` — starts a session in the TUI. A positional
  `PROMPT` seeds the first message.
- `statie -p|--prompt <TEXT> [OPTIONS]` — headless mode (see below).
  Repeatable; each `-p` is one seeded message, in order, after the
  positional `PROMPT` if there is one.
- `statie config init [--global] [--force]` — writes the default config to
  `./.statie/config.json` (or `~/.config/statie/config.json` with
  `--global`); refuses to overwrite without `--force`. `.statie/` is
  self-ignoring; keep a copy elsewhere (e.g. `statie.example.json`) if you
  want the config in version control.
- `statie config show` — prints the effective config and its source.
- `statie check` — builds the model backend and prints its capabilities
  (native thinking, schema-constrained decoding, prefix caching). For Ollama
  it also asks the server whether `model.model` is present.
- `statie eval [--task toy|coding] [--steps N] [--json] [-v]` — see
  [Eval](#eval).

Session options, valid with or without `-p`:

- `-r, --resume` — continue the session in `<sandbox>/.statie/`. **Fresh is
  the default**: without `--resume`, an existing store and log are archived
  to `.statie/archive/<timestamp>/` first and a clean session starts. On
  resume the TUI starts in the skill that was active and replays the last
  40 log entries into the history pane.
- `--skill <name>` — the starting activation. `router` (default), `plan`,
  `implement`, and `review` share the coding workspace; `toy` runs the toy
  counter skill. Normally the router picks and the handoffs chain on their
  own. An explicit `--skill` wins over the stored activation on resume.
- `--memory` — in-memory store and log; nothing touches disk. Never archives
  or resumes.
- `--toy-target <n>` — target for the `toy` skill (default 10).

## In the TUI

The history pane shows each step: the model's reasoning collapsed to one
line, the patch, the action, and the tool output; the status bar shows the
skill, step, version, prompt tokens (estimated and server-reported), and a
spinner with elapsed seconds while the model is thinking. Steps on a 26B
local model take 20–60 s. On startup the banner prints the config path, the
model, the sandbox, and whether the session is fresh or resumed.

Type a message and press Enter to give the model its next turn. Slash
commands (type `/` for completion, Tab to accept):

| Command | Effect |
|---|---|
| `/state` | print the committed workspace |
| `/reasoning [step]` | expand the reasoning of a step (default: latest) |
| `/patches [n]` | last n patches (default 5) |
| `/log [kind] [n]` | last n log entries, optionally one kind: `user`, `patch`, `action`, `tool_output`, `system` |
| `/tools` | tools available to the current skill |
| `/skill <name>` | switch skill (must be an allowed transition); `/skill! <name>` forces it |
| `/edit <json>` | apply a merge patch to the workspace yourself |
| `/help`, `/quit` | |

Keys: ↑/↓ and PgUp/PgDn scroll history, Home/End jump, ctrl-u clears the
input, ctrl-l clears the pane, Esc clears the input, ctrl-c cancels the
in-flight step (and quits when idle).

When the session parks it says why and what to do: a question from the
model, a step failure (fix the config or server and send a message to
retry), or the autonomous step cap (`max_autonomous_steps` consecutive
steps without pausing; any message continues with a fresh counter).

## Tools

All tools are confined to `sandbox_root` and their output is capped at
`tools.max_output_bytes`, then truncated head/tail to `budget.observation`
for the prompt (the full output stays in the session log).

| Tool | What it does |
|---|---|
| `read_file` | a file, or a window of it (`offset`/`limit`), with line numbers |
| `write_file` | whole-file write, atomic; for new files or full rewrites |
| `edit_file` | replace exactly one anchored occurrence; whitespace-tolerant fallback; reports the closest line on a miss |
| `search` | regex over file contents, with `path`/`glob`/`max_results`; root-relative paths |
| `list_files` | recursive listing, skipping `.git`, `target`, `node_modules`, `__pycache__`, virtualenvs |
| `filter_last_tool_call` | regex over the full output of the previous tool call, for when it overflowed |
| `shell` | `sh -c` in the sandbox with a timeout; runs in its own process group and kills it on timeout, cancel, or lingering background children |
| `test` | the project's test command (auto-detected or explicit), reported as `result: pass` / `result: fail (exit N)` plus a failure summary |
| `recall` | search the session log (never the model's reasoning) |

The coding skills: `router` decides where to go, `plan` explores and writes
a step list, `implement` works the plan with `edit_file`/`write_file` and
runs `test`, `review` runs `test`, checks the changes, and either sends
findings back to `implement` or marks the task done.

## Headless mode

`-p` drives the session to completion without a terminal UI, printing every
step to stdout, and exits 0 when the run reaches `done` or parks. The last
line is a summary:

```
steps=N outcome=<done|parked|...> prompt_tokens=[…]
```

```sh
statie -p "create hello.txt containing hello statie" --memory
```

Pair `--memory` with a `model.backend: "mock"` config for offline,
deterministic runs in CI.

## Eval

`statie eval` runs the same task through two harnesses built from the same
config: **statie** (bounded budgets) and **baseline** (an append-only
harness that re-injects the whole transcript every step). Both run
headlessly in temp sandboxes and report `steps`, `success`, and two
families of prompt-token stats: `est_*` from the local cl100k estimate, and
`srv_*` from the backend's own usage counts (Ollama's `prompt_eval_count`/
`eval_count`, or an OpenAI-compatible `usage` object). `srv_max` is the
number to compare against `model.num_ctx`. `--task toy` drives the counter
skill for `--steps`; `--task coding` asks the router to create a file and
checks that it did. `--json` prints an object instead of a table; `-v`
prints both transcripts to stderr.

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
- **Whole-file writes are where small models corrupt code.** `edit_file`
  replaces exactly one anchored occurrence and refuses ambiguous or missing
  anchors; its description says outright that `new` replaces `old`, because
  the first live run "inserted" a function by anchoring on its neighbour and
  deleted the neighbour. `search` (regex, root-relative paths) lets the model
  find definitions without reading whole files.
- **The repo index must follow the model's edits.** `RepoBm25` used to index
  once at startup; after an edit, the augmentation section showed the old
  file next to a fresh `read_file` of the new one, and the reviewer looped
  between `test` and `read_file` trying to reconcile them. It now
  fingerprints the tree (mtime + length) on every `augment` call and
  re-indexes when anything changed.
- **Say how to hand off, not just that it's required.** The `done`
  rejection now names the skill's allowed `next_skill` targets; without that
  the model repeated `done` five times with `next_skill` still set to its
  own name. The loop guard also watches a window of the last 8 tool calls,
  so alternating A/B/A/B repeats are flagged, not only consecutive ones.
- **`test` reports a verdict first.** Its output starts with `result: pass`
  or `result: fail (exit N)` plus a failure summary, so the model reads one
  line instead of a traceback. `list_files`/`search`/`RepoBm25` skip
  `__pycache__` and virtualenvs alongside `.git`, `target`, `node_modules`.
- **Small models patch arrays by key, so let them.** To mark one plan step
  done, gemma4 sent `plan: [{"id": 2, "status": "in_progress"}]` and, under
  strict RFC 7386, the other five steps vanished; a rule that rejected the
  drop just made it retry the same shape until the cap. The merge strategy
  now treats arrays of objects sharing an `id` or `path` as keyed: entries in
  the patch update their match field by field, new keys append, omitted
  entries stay. Plain arrays (strings) are still replaced whole.
- **Normalize the action shapes the model actually emits.** `{"type":
  "shell", "args": ...}` (tool name as the type) is rewritten to a `tool`
  call and logged as a `normalized` system entry instead of being rejected;
  a `tool` action with no `name` gets a message that says what's missing.
- **Truncate to fit by construction, then tell the model what it lost.**
  Head/tail truncation budgeted 60% + 40% of the observation cap and then
  appended its marker, so a 57k-char `ls -R node_modules` came out 11 tokens
  over and the strict builder failed the step. `truncate` now reserves the
  marker's cost, verifies the rendered count, and shrinks until it fits.
  The marker itself is now an instruction with numbers ("57853 chars, ~14000
  tokens against a 2000-token budget; showing the first 1200 and last 800")
  and the ways to narrow. The harness view carries a `last_observation:`
  note until the next tool output fits, so the following step still knows.
  And `filter_last_tool_call` applies a regex to the full output kept in the
  session log, so the model can pull the lines it needs without re-running
  the command; there is deliberately no "show me offset N" mode.
- **Repair the last brace, not the content.** Writing a whole file through
  `write_file` means 1000+ tokens of escaped code inside one JSON string;
  gemma4 twice nested `ack_inbox` inside `action` and dropped the final `}`.
  Ollama's JSON mode did not stop it (a direct probe produced a balanced
  1593-token object with `done_reason: stop`). The parser now closes
  unclosed brackets at end of output and hoists a misplaced `ack_inbox`/
  `patch`, logging both as `normalized`; a cutoff inside a string is still
  rejected. The server's `done_reason`/`finish_reason` is recorded, and a
  `length` cutoff says so in the rejection with advice to write less per
  step. Rejected raw content is logged up to 16k chars so the next
  diagnosis doesn't lose the tail.
- **Background processes outlive the shell.** The model ran `npm run dev &`;
  the 120 s timeout killed `sh` but the Vite server kept port 3000 until it
  was killed by hand. `shell` now runs each command in its own process
  group, kills the group on timeout or cancel, and after the shell exits
  gives lingering readers 500 ms before killing the group and saying so.
- **Anchors differ by whitespace more than by content.** Five of fourteen
  `edit_file` calls in one session failed "not found" and each cost a
  `read_file` round trip. Exact match is tried first; on zero matches a
  whitespace-normalized window match is accepted when unique (with a
  note), and a miss reports the closest line.
- **The step cap is a pause, not a failure.** The parked message now says
  so, with the step and skill, and how to continue.
- **`yield` is a dead end for a coding agent.** The model paused with
  `yield` after one step, and the UI could only say "waiting for you: yield".
  The coding skills now reject `yield` outright (`ask_user` with a concrete
  question is the only way to pause), and the park message shows the model's
  own `recent_summary` plus how to resume.
- **A model error must not kill the UI.** Running without a reachable
  server used to exit the TUI on the first Enter. Step failures are now shown
  in red and the session parks; only store/log failures or an unknown skill
  are fatal. The startup banner prints the config path, model, and sandbox so
  a wrong setup is visible before typing.

With those in place, the toy task "add `multiply` to a two-function Python
module so `make test` passes" went from 32+ steps and a killed run to 8 steps
with zero rejections: router → plan → implement (baseline `test`, one
`edit_file`, `test`) → review → done.

Live result on the `eval --task coding` task (create a file end to end via
router → plan → implement → review): both harnesses succeeded in 11 steps;
statie's prompt peaked at 2906 tokens with 478 tokens of growth (workspace
state, not history) versus 3671 and 1173 for the append-only baseline.

## Architecture

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the binding contract between
crates (types, traits, the session loop algorithm, and the type-level
invariants) — including the design for the next milestone, on-demand
guides and subagents as state composition — and
[statie-spec.md](./statie-spec.md) for the original design plan it refines.
