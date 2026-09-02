# SKILL.state Coding Harness — Implementation Plan

A mutable-state, interactive coding harness for local LLMs, written in Rust. Based on the SKILL.state architecture (Badhe, Tiwari, Chung — arXiv:2608.26263, EMNLP 2026), adapted for an interactive TUI workflow rather than batch skill execution.

## 1. Goals and non-goals

**Goals**

- Bounded prompt at every step: immutable spec + current structured state + latest observation (+ optional augmentation). No transcript growth.
- Interactive, adaptive session: user messages are just another observation source; the model chooses per step whether to continue autonomously or yield to the user.
- Type-system-enforced invariants where practical (patch validation, prompt gating, immutability, ownership of state regions).
- Pluggable: state storage, patch strategy (state mapping), augmentation (RAG), and model backend are all traits.
- Full session history is persisted for the user and *searchable by the model via tools*, but never appended to the prompt.

**Non-goals**

- Graph-based orchestration (LangGraph-style). Phase routing collapses into state + activation swaps.
- Multi-pane TUI. One history pane, one input line, slash commands for inspection.
- Cloud backends as a priority. Local inference on the user's own hardware is the primary target (Gemma 4 via Ollama first); the `Model` trait exists so other local servers, and eventually hosted APIs, are cheap to add later.

## 2. Core model

One loop, driven by events, not by a "run until terminal" condition.

```
Event ──► build prompt (spec, state, obs, aug) ──► Model
                                                    │
                     ┌──────────────────────────────┘
                     ▼
        RawOutput { reasoning, content }
                     │
      reasoning ──► SessionLog (user-visible, never in prompt)
      content   ──► parse ──► RawPatch + RawAction
                                   │
                          validate ──► ValidatedPatch<S>
                                   │
                    commit (patch + action, atomic) ──► StateStore
                                   │
                    execute action ──► Observation ──► next step / wait
```

Per step the model emits one JSON object `{ patch, action }`. Reasoning arrives on a separate channel (native thinking) or is stripped from a tagged block on backends without it.

### Events and actions

```rust
enum Event { User(UserInput), ToolDone(ToolId, Observation), Cancel, Tick }

enum Action {
    Tool(ToolCall),     // execute, feed observation back, step again
    AskUser(String),    // park until next User event
    Yield,              // park until next User event
    Done,               // activation complete
}
```

`Tool` re-enters the step loop immediately (autonomous mode). `AskUser`/`Yield` park the session. The model picks per step; there is no harness-level "autonomous vs interactive" mode switch.

## 3. Type-level invariants

Invariants are tiered by how much the compiler actually guarantees. Tier A means a violating program does not build. Tier B means the check is at runtime but the types make it impossible to skip. Tier C is convention the types support but cannot enforce.

### Tier A — compile-time

| # | Invariant | Mechanism |
|---|-----------|-----------|
| A1 | Reasoning never enters the prompt | Sealed `Promptable` trait; `Reasoning` is `LogEntry` only |
| A2 | Only validated patches commit | Typestate `RawPatch → ValidatedPatch<S>`; `StateStore::commit` accepts only `ValidatedPatch<S>` |
| A3 | A patch commits at most once; an action executes at most once | `ValidatedPatch<S>` and `Executable<Action>` are `!Clone` and consumed by `commit` / `execute` (linearity) |
| A4 | An action cannot execute before its patch commits | `execute` takes `Executable<Action>`, which only `commit` returns |
| A5 | Prompt is bounded | `BoundedPrompt` constructible only via the token-budgeted builder; `Model::step` takes `BoundedPrompt` |
| A6 | Model can only write its own state region | `Committed<H, S>`; `ValidatedPatch<S>` targets `agent` only |
| A7 | Observations are the only history channel into the prompt | Private `Observation` constructor; produced only by the tool executor (from `Executable<Action>`) or the harness rejection path |
| A8 | Only an active activation can step | `Session::activate::<K>()` is the sole constructor of `Active<K>`, which owns `.step()` |
| A9 | Tools only touch the session root | `SandboxPath` newtype, constructible only by joining and canonicalizing against the root; tool APIs take `SandboxPath`, never `Path` |
| A10 | Decoder schema and parser target are the same type | Schema passed to `Model::step` is `schemars`-derived from the exact `{patch, action}` type the parser deserializes into; there is no hand-written schema |

### Tier B — runtime checks the types make unforgettable

| # | Invariant | Mechanism |
|---|-----------|-----------|
| B1 | Patch validated against version N commits against version N | `BoundedPrompt` carries the `Version` it was built from; `RawOutput` inherits it; `validate` stamps it on `ValidatedPatch`; `commit` compares. The compare is runtime, the threading prevents mixing |
| B2 | Cross-region rules hold (e.g. no `Yield` with unacked inbox) | Witness tokens: validation returns a zero-sized `InboxAcked`; the `Yield` path requires it. See pattern below |
| B3 | Patch and action commit atomically | Single `commit(patch, action)` call inside a store transaction; `Cancel` before commit leaves no partial state. Tool side effects mid-execution are not covered (see §8) |
| B4 | Every step is logged | `step()` returns `#[must_use] StepRecord`; only `SessionLog::append` consumes it. Weak, but catches the common omission |

### Tier C — convention

| # | Invariant | Mechanism and caveat |
|---|-----------|----------------------|
| C1 | Spec is immutable per activation | `Arc<SkillSpec>`, no `&mut` accessors. Prevents mutation, not construction from arbitrary text |
| C2 | Augmentation is derived, never persisted by the harness | Separate `Augmentation` section, recomputed each step, no path into `StateStore`. The model may legitimately copy a snippet into `recent_summary` via its patch; that is the intended channel and not typeable |

### Witness-token pattern

For any rule of the form "X must have happened before Y," have the code that checks X return a zero-sized witness type with a private constructor, and make Y take that witness as a parameter. The check stays at runtime, but forgetting it becomes a compile error. `InboxAcked`, `SchemaChecked`, and `WithinBudget` are the first uses; new rules should be promoted this way rather than added as bare `if` checks in the loop.

### Caveat: `String` laundering

The `Promptable` gate stops accidental leaks, not deliberate ones: any `String`-accepting constructor of a `Promptable` type (`SkillSpec::new` in particular) can launder text past it. That is accepted. `Reasoning` keeps normal ergonomics (`Display`, `Serialize`, `AsRef<str>`) so the TUI, exporters, and extensions stay easy to write; constructors that are naturally typed (`Observation` from `truncate`, `Snippet` from an `Augmenter`) stay typed. Anything beyond that is a code-review concern, not a compiler one.

### Prompt gate

The invariant that matters is not "reasoning is never persisted" — it is "reasoning never reaches the model." Persisting everything to the session log is fine and desirable.

```rust
mod sealed { pub trait Promptable {} }
impl sealed::Promptable for Arc<SkillSpec> {}
impl sealed::Promptable for AgentState<S> {}
impl sealed::Promptable for Observation {}
impl sealed::Promptable for Augmentation {}

pub struct Reasoning(String);        // impl LogEntry; never Promptable
pub struct ToolOutputFull(String);   // logged whole; Observation is its truncated view
```

### State ownership

```rust
struct Committed<H, S> {
    harness: H,   // written by the harness only
    agent: S,     // written by the model via ValidatedPatch<S>
}
```

`H` holds: inbox of user messages (id, text, `acked: bool`), active tool set, budgets, current activation, step counter. Validation can enforce cross-region rules, e.g. *"cannot `Yield` with unacked inbox items"* — the rule that keeps the agent responsive rather than forgetful.

## 4. Core traits

```rust
pub trait Skill: Send + Sync {
    type Workspace: Serialize + DeserializeOwned + JsonSchema + Clone;
    type Action: DeserializeOwned + JsonSchema;
    type Patch: PatchStrategy<Self::Workspace>;
    fn spec(&self) -> Arc<SkillSpec>;
    fn validate(&self, cur: &Committed<H, Self::Workspace>, p: RawPatch)
        -> Result<ValidatedPatch<Self::Workspace>, PatchError>;
    fn is_terminal(&self, s: &Self::Workspace) -> bool;
}

/// Pluggable state mapping.
pub trait PatchStrategy<S> {
    fn apply(cur: &S, p: &ValidatedPatch<S>) -> Result<S, PatchError>;
}
// impls: Replace, JsonMergePatch (RFC 7386), JsonPatch (RFC 6902), Typed(enum Op)

/// Pluggable storage. Version-checked so a crashed step can't double-commit.
pub trait StateStore<S>: Send + Sync {
    async fn load(&self) -> Result<(S, Version)>;
    async fn commit(&self, p: ValidatedPatch<S>, expect: Version) -> Result<Version>;
}
// impls: InMemory, Sqlite (redb as alternative). Storing the patch log gives replay/rollback.

/// Pluggable augmentation. Composable; each with its own token budget.
pub trait Augmenter<S>: Send + Sync {
    async fn augment(&self, s: &S, obs: &Observation, budget: TokenBudget) -> Vec<Snippet>;
}
// impls: RepoBm25, SessionLogBm25, later Embeddings

/// Pluggable model backend.
pub struct Capabilities {
    pub native_thinking: bool,     // reasoning arrives in its own channel
    pub schema_constrained: bool,  // decoder enforces the JSON schema
    pub prefix_cache: bool,        // stable-first ordering pays off
}

pub trait Model: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn step(
        &self,
        prompt: BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,    // streaming reasoning + content deltas (TUI implements)
    ) -> Result<RawOutput, ModelError>;
}

pub struct RawOutput { pub reasoning: Option<Reasoning>, pub content: String }
// impls: OllamaModel, OpenAiCompatModel (vLLM, llama.cpp server, LM Studio), MockModel (tests)
```

The harness reads `capabilities()` once and adapts: no native thinking → request a `<thoughts>` block in the system prompt and strip it; no schema constraint → rely more on validation retries.

### Session and activations

The spec is immutable *per activation*, not per session. A session owns a persistent workspace and a sequence of skill activations.

```rust
struct Session<W> {
    store: Box<dyn StateStore<Committed<H, W>>>,
    log: SessionLog,
    model: Box<dyn Model>,
    augmenters: Vec<Box<dyn Augmenter<W>>>,
}

struct Active<'a, K: Skill> {
    session: &'a mut Session<K::Workspace>,
    spec: Arc<SkillSpec>,
}
// Session::activate::<K>() is the only constructor; only Active<K> has .step()
```

Activation switches (plan → implement → review → debug) are chosen by a small router skill or by the user via slash command. This replaces the phase-enum-inside-one-giant-state approach.

## 5. Session log and history recall

Two sinks, one gate:

- **SessionLog** — append-only JSONL, user-facing, permanent. Every entry: `step_id`, `kind` (user, reasoning, patch, action, tool_output, system), timestamp, payload. Indexed at write time (BM25 term set; embeddings later).
- **StateStore** — model-facing state only.

History is **pull, not push**:

- A `recall(query, kind_filter?)` tool searches the session log. Its result comes back as an `Observation`, truncated to the observation budget, through the same prompt gate as any other tool.
- Optionally, a `SessionLogBm25` augmenter injects a few relevant prior entries proactively.
- The system prompt tells the model history exists and is searchable; `step: N` in harness state reinforces it.
- Kind filtering ("only tool outputs", "only my patches") keeps returned observations small and on-topic.

The model answers "what did you just do?" from a bounded `recent_summary` field in agent state that it maintains itself — the harness caps its size, the model decides what's worth keeping.

## 6. TUI

Layout follows existing coding harnesses: one scrolling history pane, one input line. `ratatui` + `crossterm`.

- Reasoning renders inline as a collapsed, dimmed block while streaming (agent is visibly thinking).
- Slash commands are filtered views over the session log and state:
  - `/state` — current agent state (rendered JSON)
  - `/reasoning [step]` — expand reasoning for last or given step
  - `/patches [n]` — recent state diffs
  - `/log [kind]` — filter session log
  - `/tools` — active tool set
  - `/skill <name>` — switch activation
  - `/edit` — user-authored patch to agent state (goes through the same `ValidatedPatch` path)
- `Cancel` (Ctrl-C) aborts the in-flight step via `CancellationToken`; nothing commits.

The TUI implements `StepSink` to receive streaming deltas. It is the only consumer of the reasoning channel.

## 7. Prompt construction

Sections, in this order (stable first for KV prefix caching):

1. System prompt — harness contract: `{patch, action}` format, patch strategy in use, tool conventions, "history is searchable via `recall`", per-model quirks.
2. Skill spec — activation-specific instructions.
3. Tool list — schemas for the active tool set.
4. Agent state — `Committed.agent` as JSON.
5. Harness view — inbox (unacked items), step counter, budgets.
6. Augmentation — retrieved snippets, per-augmenter budgets.
7. Latest observation — truncated.

Each step sends system prompt + exactly one user message. No history array.

`BoundedPrompt::build()` tokenizes each section against a `TokenBudget` and returns `Err(OverBudget { section, .. })` rather than a prompt.

## 8. Validation and recovery

- Parse content → `RawPatch`, `RawAction`. Parse failure → error observation `"output rejected: <reason>"`, retry up to N. No reasoning carried across retries.
- `Skill::validate` checks schema, patch strategy constraints, and cross-region rules (inbox acks, allowed activation transitions).
- Rejected patch → error observation with the validator's message. The model sees *why* and tries again with the unchanged state.
- After N failures: park with `AskUser` and surface the failure in the TUI.
- `Cancel` during tool execution: the patch has already committed, so state is consistent, but the tool's side effects may be partial (half-written file, killed process). The executor records a `ToolAborted` observation so the next step sees it. Making writes transactional (write to temp, rename) is a per-tool concern, not a harness guarantee.

## 9. Model backends

### First target: Gemma 4 on Ollama

- Endpoint: `/api/chat`, `stream: true`, `think: true`, `format: <json schema for {patch, action}>`.
- Thinking arrives in its own field → maps directly to `Reasoning` → session log. No tag parsing.
- Use `format` with the full `schemars`-derived schema rather than native tool calling. Native tool calling assumes the messages + tool-role append pattern being replaced; one constrained JSON object per step keeps patch and action atomic.
- Still validate: `format` guarantees JSON syntax, not necessarily full schema conformance in all modes.
- Stable-first prompt ordering exploits Ollama's prefix cache.

### Others (later)

- `OpenAiCompatModel` for vLLM (guided_json), llama.cpp server (GBNF from schema), LM Studio.
- `MockModel` replays scripted outputs for deterministic tests without a GPU.

## 10. Milestones

| # | Milestone | Deliverable | Done when |
|---|-----------|-------------|-----------|
| 0 | Skeleton | Crate layout, core types, sealed `Promptable`, `Committed<H,S>`, `BoundedPrompt` builder | Compiles; unit tests for gate and budget |
| 1 | Core loop + TUI | Event loop, `OllamaModel`, `InMemory` store, `JsonMergePatch`, minimal TUI (history + input), collapsed reasoning, `/state` | Toy skill runs 50 steps with flat prompt size |
| 2 | Validation + recovery | `Skill::validate`, reject-as-observation, retry policy, `MockModel` test suite | Invalid outputs recover without state corruption |
| 3 | Persistence + replay | `SessionLog` JSONL with write-time indexing, `SqliteStore`, patch log, `replay(to: Version)`, `/log`, `/patches`, `/reasoning` | Session survives restart; replay reproduces state |
| 4 | Augmentation + recall | `Augmenter` trait, `RepoBm25`, `SessionLogBm25`, `recall` tool with kind filter, per-augmenter budgets | Model retrieves prior steps on demand |
| 5 | Activations | `Session`/`Active<K>` typestate, router skill, `/skill`, allowed-transition validation | Plan → implement → review flow works |
| 6 | Coding skill | Workspace = file manifest + task list + `recent_summary`; tools: read, write, shell, test, recall; sandboxed cwd; `/edit` | Completes a small real task end to end |
| 7 | Eval + second backend | Append-only baseline harness for comparison; `OpenAiCompatModel` | Token and success metrics on the same tasks |

## 11. Crate layout

```
skillstate/
├── core/        # types, traits, prompt gate, budget, validation, loop
├── store/       # InMemory, Sqlite
├── patch/       # Replace, JsonMergePatch, JsonPatch, Typed
├── augment/     # RepoBm25, SessionLogBm25
├── models/      # Ollama, OpenAiCompat, Mock
├── tools/       # read, write, shell, test, recall
├── skills/      # coding, router, toy
├── tui/         # ratatui frontend, StepSink impl, slash commands
└── cli/         # binary
```

## 12. Dependencies

`serde`, `serde_json`, `schemars`, `json-patch`, `tokio`, `tokio-util` (CancellationToken), `reqwest`, `minijinja` (prompt templates), `tokenizers` or `tiktoken-rs` (budget), `rusqlite` (or `redb`), `tantivy` or a small BM25 impl, `ratatui`, `crossterm`, `thiserror`, `tracing`.

## 13. Open questions

- Patch strategy default: merge-patch is most reliable for small models; does Gemma 4 at this size handle RFC 6902 or typed ops well enough to make them the default?
- Observation truncation policy: head/tail split vs. model-requested windows (`read_more(offset)` tool).
- Whether `recent_summary` should be model-owned only or harness-seeded with the last N action names.
- Whether inbox acks are explicit patch fields or inferred from the model referencing an inbox id in its action.
- Embedding backend for augmentation once BM25 hits its limit (Ollama `/api/embed` is the obvious first choice).
