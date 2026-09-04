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
5. Notes — the harness-owned `notes` field kept by the maintainer (section 9); omitted when no maintainer is configured.
6. Harness view — inbox (unacked items), step counter, budgets.
7. Working set — recent calls and the last few read windows, derived from the log (section 10).
8. Augmentation — retrieved snippets, per-augmenter budgets.
9. Latest observation — truncated.

Each step sends system prompt + exactly one user message. No history array.

`BoundedPrompt::build()` tokenizes each section against a `TokenBudget` and returns `Err(OverBudget { section, .. })` rather than a prompt.

## 8. Validation and recovery

- Parse content → `RawPatch`, `RawAction`. Parse failure → error observation `"output rejected: <reason>"`, retry up to N. No reasoning carried across retries.
- `Skill::validate` checks schema, patch strategy constraints, and cross-region rules (inbox acks, allowed activation transitions).
- Rejected patch → error observation with the validator's message. The model sees *why* and tries again with the unchanged state.
- After N failures: park with `AskUser` and surface the failure in the TUI.
- Non-progress is rejected by field-driven rules, not prompt hints: loop and read guards, the commitment slot (`next`), redundant verification and redundant reads, done-stays-done plan steps, and mutating-only commitments. Each came from a logged stall; the current list is in ARCHITECTURE.md under "Guards against non-progress".
- `Cancel` during tool execution: the patch has already committed, so state is consistent, but the tool's side effects may be partial (half-written file, killed process). The executor records a `ToolAborted` observation so the next step sees it. Making writes transactional (write to temp, rename) is a per-tool concern, not a harness guarantee.

## 9. Context maintenance

The paper discards the reasoning trace once the state update is validated
and trusts the model to have written whatever mattered into state. Its own
limitations section names the failure this causes: a correct later update
can depend on something that was observed or decided earlier and never
committed to state. Every stall in the dogfood logs is that failure. The
model decides in reasoning ("delete lines 768-770", "old = this block, new =
that block"), the reasoning is dropped, the next step has no home for the
decision, and the model reads "one more thing" instead of acting. The
commitment slot (`next`) and the guards in section 8 make non-progress fast
and visible; they do not preserve the decision.

Context maintenance keeps the bounded prompt and stops throwing the decision
away. After each committed step, before the worker sees its next prompt, a
separate model call distils what the reasoning contained into a bounded,
harness-owned field. The worker's context is not dropped every step; it is
pruned into the allocated size by a call whose only job is pruning.

This is not the paper's summary-capped baseline (0.52 against 0.94). That
baseline summarised the transcript as a substitute for state. Here the
structured state stays authoritative and the maintainer maintains one field
from material that would otherwise be lost.

### Trait

```rust
pub struct MaintainInput<'a> {
    pub reasoning: Option<&'a Reasoning>,   // this step's trace, as logged
    pub patch: &'a Value,                    // the raw patch the worker sent
    pub action: &'a Action,
    pub observation: &'a Observation,        // what the action produced
    pub agent: &'a Value,                    // committed agent state, post-step
    pub commitment: Option<&'a Commitment>,  // the open `next`, if any
    pub notes: &'a Notes,                    // previous notes
    pub budget: TokenBudget,                 // `budgets.notes`
}

pub struct MaintainOutput {
    pub notes: Notes,
    pub next: Option<Commitment>,            // a decision the worker made but did not write
}

#[async_trait]
pub trait Maintainer: Send + Sync {
    async fn maintain(&self, input: MaintainInput<'_>) -> Result<MaintainOutput, MaintainError>;
}
```

`Notes` is a `Promptable` type with a private constructor: only the
maintainer path produces one, and `BoundedPrompt` renders it. A
`ModelMaintainer<M: Model>` implementation drives any `Model` backend with
a fixed system prompt and schema-constrained output `{notes, next?}`. A
`MockMaintainer` returns scripted outputs for tests. `NoMaintainer` is the
default when config has no `maintainer` section, and behaves exactly as
today.

### Where it runs in the step

1. Worker step validated, committed, tool executed, observation produced
   (the existing loop, unchanged).
2. `Maintainer::maintain` is called with the inputs above. It runs after
   every committed `Tool` step and after `AskUser`/`Yield` parks; it is not
   called on rejections (nothing new was committed) or after `Done`.
3. The output is applied as a patch `{"notes": ..., "next": ...}` with
   `source: "maintainer"` through the same validation as an operator patch
   (`validate_user_patch`): skill rules apply, so a malformed `next` is
   rejected and logged, and the previous notes are kept. `next` from the
   maintainer is applied only when the workspace's `next` is `null`; it
   never overrides a commitment the worker wrote itself.
4. The next worker prompt renders `notes` as its own section between the
   agent state and the harness view, headed "notes (kept by the harness
   from your previous reasoning)".

A maintainer error (backend down, timeout, over budget after one retry) is
logged as `maintainer_error`, keeps the previous notes, and never blocks the
loop. The worker step is never delayed by a failed maintainer call beyond
the maintainer's own timeout.

### Ownership and budget

- `notes` is a harness-owned field on the coding workspace
  (`Skill::harness_owned_fields`, the same mechanism that protects
  `reports`): a worker patch that touches it is rejected with the field
  name. `recent_summary` stays model-owned; it is the worker's own voice
  and the two are rendered as such.
- `budgets.notes` (default 400 tokens) caps the field. The maintainer's
  prompt states the cap; an over-budget output is retried once with the
  cap restated, then the previous notes are kept.
- The maintainer's prompt contains no skill spec and no tool list, only the
  inputs above, so it stays around 2k tokens.

### The maintainer's prompt

The system prompt is fixed and states the priorities in order:

1. Quote only from the inputs. Never invent a line number, a path, an
   anchor, or a decision that is not in the reasoning or the observation.
2. Keep verbatim whatever the next mutating call needs: line numbers,
   anchor text, exact error text, the `old`/`new` of an edit that failed
   on an argument slip.
3. Keep decisions with their reasons, and the fact that a decision was
   reversed if it was.
4. Drop narration, restatements of the task, and anything already in the
   plan or the observation.
5. If the reasoning contains a decision to make a specific mutating call
   and `commitment` is null, emit it as `next` in the commitment shape.
6. Order by what is needed next; cut from the end to fit the budget.

### Config

```json
"maintainer": {
  "model": { "backend": "ollama", "model": "gemma4:26b", "think": false },
  "budget": 400,
  "timeout_secs": 120
}
```

`maintainer.model` has the same shape as `model` and defaults to it when
omitted, so the worker's model runs the maintenance call. On one GPU the
calls are serialized, so a step costs one extra call of about 2k prompt
tokens; a smaller model in this slot is the intended way to buy that back.

### Trade-offs

- Cost: one extra model call per step. Wall time per step roughly doubles
  on a single local GPU unless the maintainer is a smaller model.
- Risk: the maintainer hallucinates a decision the worker never made.
  Mitigations: the quote-only rule, `next` going through the commitment
  rules, and the worker being free to change `next` with a reason.
- What it does not fix: a wrong decision is preserved as faithfully as a
  right one. The guards in section 8 remain the answer to that.

### Done when

- The dogfood unit that produced this design (move the `owned_fields` loop
  out of the `ask_user` block in `core/src/skill.rs`) lands with no
  operator patches after a resume.
- An `edit_file` that fails on an argument slip is re-sent correctly on
  the next step, because the notes carried its `old` and `new` text.
- `eval` reports steps-to-first-edit and rejections per landed edit with
  and without the maintainer on the same task.

## 10. Working set (recent observations)

The paper's prompt carries one observation: the latest. In a coding
session that is too little. The model reads the failing test, then reads
the file to change, and by then the test is gone from the prompt; it
reads the test again, and the redundancy rule (section 8) refuses the
re-read of exactly what the harness discarded. Seven runs on one unit
produced no edit, not because the model could not write the edit but
because it never had both halves of the problem in front of it at once.
Gemma 4 does this work fine in a transcript harness, where the last few
tool outputs are still visible. The state is still the memory; the working
set is the desk.

### What it holds

- **Recent calls.** One line per executed call for the last
  `working_set.recent_calls` steps (default 6): `step N: <tool> <compact
  args> -> <first line of the result>`. Skipped and errored calls are
  included and marked.
- **Windows.** The last `working_set.max_windows` (default 4) distinct
  `read_file` and `search` outputs, keyed by tool plus canonical args
  (a later identical call replaces the earlier entry), newest first,
  rendered verbatim under a header naming the path, the line range, and
  the step. The output that is this step's observation is not repeated.
  A window on path P is dropped once a later `edit_file` or `write_file`
  touched P; the model re-reads what it changed.
- **Edit windows.** A successful `edit_file`/`write_file` output (the
  tool prints the edited region with line numbers) is itself a window on
  P, keyed by path, and it replaces every earlier window on P, read or
  edit. Run 24 landed six edits and failed ten on anchors the model
  invented for the arm next to its insertion point: after each edit the
  file's current text had left the prompt, so the next anchor came from
  memory. The edit window keeps the region the model just changed in
  front of it until it changes that path again.

### Budget and placement

`budgets.working_set` (default 6000) bounds the section. Windows are
dropped oldest-first as whole units to fit; a single window larger than
the budget is head-truncated with a marker. Recent calls are cut first
only when no window fits. The section renders after the harness view and
before augmentation, so the stable sections keep their prefix. With the
dogfood config (`num_ctx` 16384) the prompt lands near 10k tokens.

### Derivation

`WorkingSet::from_log(log, tokenizer, budget, config, current_step)` in
core builds it from `tool_output` and `action` entries in the session log
every step (C2: derived, never persisted). It is not an `Augmenter`:
augmentation is "never load-bearing" and clamped by
`budgets.augmentation`, while the working set is the model's view of what
it just read and has its own budget. `working_set.max_windows: 0`
disables the section and restores today's behaviour.

### Interplay with the guards

- Redundant reads: the skip message says the window is in the working
  set rather than "in the log".
- Silent reads: unchanged; a read that records nothing still counts.
- Read guard and loop guard: unchanged.

### Done when

- The shrunk dogfood unit (three missing match arms in
  `tui/src/headless.rs`, compiler naming them) lands with the test file
  and the target file visible together in one prompt.
- A prompt-size test shows the working-set section bounded by its budget
  across a scripted run that reads more windows than `max_windows`.

### After it works: the rule audit

Most of the non-progress rules in section 8 were written against a model
that saw one observation at a time. If the working set fixes the stalls,
some of those rules were treating a symptom of too little context as a
model defect, and they now cost steps for nothing (run 19: two rules
pointing opposite ways at a model that simply needed both files in view).
The audit peels them back one at a time and measures.

First-pass classification, by what each rule was written against:

| Rule | Origin | Likely cause | Audit |
|------|--------|--------------|-------|
| Redundant reads (skip) | model re-read what the prompt had dropped | context | peel first |
| Silent reads | 62 reads with empty patches, offsets cycling | context | peel |
| Read guard (4 per path, note) | paging one file window by window | context | peel |
| Redundant verification (skip) | test re-run because the result was gone | context, partly habit | peel to a note |
| Done stays done | plan reopened after resume; model could not see what it had done | context, partly plan-trust | peel after recent calls |
| Commitment slot read cap | decisions died with the reasoning | context; the maintainer now preserves them | relax to a note while `next` is set |
| Commitment must be mutating | definitional | structure | keep |
| Loop guard (note) | identical triples | cheap, note only | keep |
| Active tools, handoff on done | contract | structure | keep |
| Rejection attempt count | retry replay | structure | keep |
| Per-skill action grammar, required placeholders | grammar steering | format | keep |
| Tool ergonomics (anchors, defaults, summaries) | tool shape | format | keep |
| Maintainer | reasoning discarded | the mechanism itself | keep, measure with and without |

Method: every rule marked "peel" gets a switch in a `guards` config
section (`guards.redundant_reads`, `guards.silent_reads`,
`guards.read_guard`, `guards.redundant_verify`, `guards.done_stays_done`,
`guards.commitment_read_cap`; all default `true` at the time of the
audit; `commitment_read_cap` and then `done_stays_done` were turned off
by its results and the first bench battery). The audit runs the same
unit with the working set on, one rule off at a time in the order above,
and compares the eval columns (`rej`, `edits`, `first_edit`, `maint`) and
steps to completion. A rule stays off when the run is no worse without
it; a rule that is still earning rejections stays on and is re-labelled
"model". The result is the smallest rule set that still lands the unit,
which is the honest measure of how much of the earlier machinery was
compensation for a starved prompt.

Run 21 (the first with the working set) landed the first edit on the
unit and then spent its remaining steps against the rules, which
reorders the audit and adds two harness fixes that precede it:

- A committed call that the redundancy rules skip must still discharge
  the commitment. Otherwise the slot can never be fulfilled and every
  other call is rejected for not honouring it (run 21, steps 19-27).
- A `shell`/`test` commitment matches on the tool alone. The model
  wrote `next.path` on a grep, the grep carried no `path` argument, and
  the call that honoured the commitment was recorded as a mismatch.
- "Commitment must be mutating" stays definitional, but the response
  changes: a non-mutating `next` is dropped with a note, not rejected.
  The model uses the slot to name its next look (nine rejections in run
  21 for `next.tool = search`), and the look was the right call.
- Done-stays-done blocked legitimate rework three times after a failing
  test showed the "done" step was wrong, and its redirect ("work on the
  first open step: run the test") then steered the model into four
  redundant test runs. Peel it first, not last.

Runs 22-26, each on the same unit with everything above in:

- Every remaining `next` rejection became a drop-with-note (run 22:
  four rejections of a `next` with no `what`, retry cap, failure), and
  the maintainer's proposal goes through the same shape checks, with
  its output schema constraining `next.tool` to the four mutating
  tools (run 23: the maintainer proposed a read, and the read cap then
  rejected every call).
- Edit windows (section 10) after run 24, where six edits landed and
  ten failed on anchors written from memory.
- The commitment read cap is off for the audit runs: run 25 parked as
  a failure on seven rejections of reads of a path other than the
  committed one; run 26 with the cap off was no worse. The cap's rule
  rejects any read of another path while `next` is set, which is
  stricter than the table above describes and than the model can use.
- Run 26 then failed on four different rejections at one step: `done`
  with an open plan step, `done` to review, an empty `ask_user`, `done`
  again. The harness gains an internal `Action::Continue { note }`
  (never offered to the model): a premature `done` in implement or an
  empty `ask_user` commits its patch, runs nothing, and the next
  observation names the first open plan step. The pattern behind all
  of these: a rejection discards state progress and buys a retry; a
  commit with a note keeps the progress and buys a step.
- A finding, not a change: with `model.think: false` (deliberate since
  run 10, when thinking deferred the edit) the model emits no reasoning,
  so `MaintainInput.reasoning` has been empty in every maintainer run.
  The maintainer's notes come from the patch, action, and observation.
  Section 9's premise (decisions live in the reasoning) is untested in
  this configuration; testing it means a run with thinking on and the
  maintainer on.

Runs 27-28 moved the bottleneck from rules to tools: run 27 hit the
step cap with 3 rejections and 10 edits, its last forty steps one
skipped read repeated (a repeated identical `read_file` now pages
forward), and two whole-file rewrites from memory (`write_file` now
refuses an existing file over 40 lines). Run 28 landed the unit: the
verification passed at committed step 94 and the full suite at 105.
The model then spent sixty steps removing duplicate arms, deleting line
ranges with `sed -i`, and reached the cap with the file broken.

### Green checkpoint

The harness never lost a green state again after run 28:

- When the `test` tool reports `result: pass`, the session snapshots
  every file the activation has edited (paths from `edit_file` and
  `write_file` actions in the log, contents read from the sandbox) as
  the green checkpoint, logs `green_checkpoint {step, paths}`, and
  prefixes the observation: "verification passed at step N; the files
  you edited are checkpointed. Finish the plan's bookkeeping and hand
  off (`done` with `next_skill: review`); cleanup beyond the task is
  review's call."
- A later pass replaces the checkpoint.
- When the activation parks (step cap, failure, or `done`) and the most
  recent `test` result is not a pass, the checkpoint is restored to the
  sandbox, `restored_green {from_step, paths}` is logged, and the park
  message says so. The model's post-green edits are gone from disk; the
  log keeps them.
- The checkpoint lives in the session, not the store (C2), and is
  bounded by a size cap; oversize files are not checkpointed and the
  log entry says which.
- Green overrides plan bookkeeping (run 30): the harness state carries
  `green_step` while the latest `test` result is a pass. A `done` in
  implement with `green_step` set marks the open plan steps done and
  hands off to review instead of being converted to a `Continue`; the
  model that sends `done` after a pass is right, and the note that
  sent it back to "make the change" cost the run.
- Every park restores, including the driver's own: the autonomous step
  cap and the "step failed" backend-error path call
  `Active::restore_green_if_stale` before parking (run 30 ended on a
  runner crash with the file broken and the checkpoint unused).

Run 32 landed the unit end to end (router, plan, implement, review,
done; 30 committed steps, 5 rejections) with the same binary that run
31 spent 101 steps on without going green. Two consequences: the audit
runs (section 10, "After it works") compare against a baseline whose
run-to-run variance is larger than any one rule's effect, so a single
run per switch is a first pass, not a verdict; and review approved
without a tool call, so the harness state now carries
`tool_calls` per activation and review's `done: true` with none is a
`Continue` that asks it to look first.

Audit result (runs 38-43, one run per switch, README field notes for
the table): no switch separates from run-to-run variance. Rejections
are about one per run with every rule on; the steps go to edits that
miss their anchor and to review's send-back shape. The commitment read
cap stays off (run 25). The other five stay on by default, not because
they earned it but because turning them off did not buy anything
either; a second pass with three runs per switch is the way to settle
it, once the edit-anchor and review-shape units land. The audit's real
finding is about where the machinery went: from rules that reject to
tools that match what the model meant (anchor stripping, line-break
tolerant anchors, duplicate-insert refusal, paging on repeat) and to
conversions that keep the patch and steer (`Continue`).

Runs 45-47 (one binary, every rule on, read cap off) all reached green
with the whole suite passing; two then failed in review, where the
model sent the same blind approval dozens of times and the look-first
conversion had no cap. A conversion is a steer, not a wall: the same
`Continue` note three steps in a row (`CONTINUE_REPEAT_CAP`) parks the
session as a question carrying the note, and every conversion that
drops an action also drops any `next_skill` in the patch, since the
core rule rejects a skill change on a non-`done` action.

`shell` refuses `sed -i` and `perl -i` (edits by line number from
memory) and points at `edit_file`.

Runs 51-54, the first four of the three-per-switch pass, brought two
stalls of one shape: the edit had landed, the model sent `done` with
the plan step still open, the premature-`done` note said "make the
change", and the model made it again, seven duplicate-insert refusals
in a row and no test run. Three wording and one bookkeeping change,
none of them a switch: the premature-`done` note offers the
bookkeeping path first (mark the step done and run `test`, else make
the change); the duplicate-insert refusal says the edit is already
applied and to verify; and the session remembers the last failing
`test` summary, so from the second identical failure the observation
says the text has not changed and whether anything was edited in
between (a `repeated_failure` entry). Thirteen of the thirty-four runs
from 21 to 54 had three or more identical failures in a row; the
harness had nothing to say about it until now.

Run 59 ended in `done` on a tree that did not compile: implement handed
off right after its own failing `test`, and review ran the unfiltered
suite, saw the non-exhaustive match, and approved with no findings.
The harness state already records whether the latest `test` passed
(`green_step`), so both are conversions now: implement's `done` with
the plan complete and no passing run, and review's approval with no
passing run, each keep the patch and say what a passing run would take.
Three of the same in a row park as a question, so a review that will
not read the test output becomes a question to the user rather than a
false `done`.

Runs 66-68 on that binary ended three different ways and each is a
ticket. Fourteen of the fifteen model-written `ask_user` parks since
run 21 carried no question, only "I need to fix X; I will first read
Y": an `ask_user` with no `?` in it is now a `Continue` in every coding
skill, the same shape as the empty one. Review approved a filtered
pass with the filtered-test note in front of it, so the harness state
records whether the pass was filtered and review's approval on a
filtered pass is a `Continue` (implement keeps its green). And the
"clean up the mess" park, six runs so far, is a missing tool: once a
file is corrupted past what `edit_file` can undo, and a whole-file
rewrite is refused by design, `revert_file` restores the content from
before the session's first edit of that path (a backup taken by
`edit_file`/`write_file` on first modification, cleared on a fresh
start, kept on resume).

Runs 69-71 on that binary all left the whole suite green on disk, and
two of them still parked: review sent `done: true` with no tool call
three times each and the repeat cap did its job. The look-first note
has now failed on this model in four runs; run 71 shows what does
work, a real test observation followed by the steer. So a blind
approval (`done: true`, no findings, no tool call in this activation)
is no longer a note but a substitution: the harness replaces the
action with `test` on the last test command the session ran (the
harness state carries it), keeps the patch, and the tool output is
the next observation with a one-line note saying why. It fires once
per review activation; after it the approval is either legitimate
(an unfiltered pass) or meets the no-green and filtered rules.

Runs 72-74 on that binary: three for three done, whole suite green on
disk in each, 30 to 89 committed steps, zero rejections. That closes
section 10's "done when" for the working set and the audit: the same
model, budget, and context that could not land one edit in 52 steps
at run 21 land the unit in three of three runs, and the difference is
entirely rules that keep the patch and steer, tools that show the
model what it meant, and a harness that does the one look the model
will not.

The three-per-switch pass (runs 51-65, table in the README field
notes) settled the defaults: every switch stays on except the
commitment read cap. With silent reads off or the read guard off, no
run of six reached green against a same-binary baseline that went green
in five of six; done-stays-done off and redundant-verify off were
within that baseline's variance (two of three green each) and are the
candidates to peel with more runs; redundant reads off sat between.
The reading of this, held loosely because variance is still larger
than any single effect: the rules that make a read leave something in
state, or that demand an edit after reads, are the ones carrying a
26B model in 16k of context, and they are not symptoms of too little
context but the substitute for it.

### Bench: the task battery

Every rule from run 21 to run 74 was tuned against one unit in one
file. The audit's verdicts are verdicts about that file. Before the
next rule, the harness needs a battery: several tasks of different
shapes, several seeds each, run through the real binary, reported as
landing rate and context cost. The prime directive it measures is the
project's: manage a limited context through mutable state and get the
work done. It does not reward tidy state; it rewards landed changes at
a low peak prompt.

A task is a directory: `task.json` (the prompt a user would type, the
verify command, the files that must be byte-identical afterwards, a
timeout), `fixture/` (the sandbox as the model finds it, failing the
verify command), and `solution/` (an overlay that makes it pass; the
self-check applies it so a broken fixture fails the test suite, not a
bench run). The first seven, six tiny Rust crates and one Python
module: add a function, fix an off-by-one, fill match arms across
files, rename across files, add a struct field, create a module, add
a method. Each is a shape a live run failed on.

`statie bench` copies a fixture to a fresh sandbox, writes the base
config in with the sandbox as root and the seed in the model options,
runs `statie -p <prompt>` there with the timeout, runs the verify
command, checks the protected files (a model that edits the test to
pass has not landed), and reads everything else from the session log:
committed steps, outcome, rejections, tool errors, edits, green step,
conversions and skips, and the server-reported prompt tokens per
call (max, mean, last). One line per run as it finishes; then per
task: landed k/n, median steps, mean peak prompt, mean seconds, the
outcomes. `--json` for the record, `--out` to keep every run's log,
stdout, verify output, and config.

What it is for: a rule earns its default when it does not lower the
landing rate across the battery; a context change (budget, working
set size, num_ctx) is judged by landing rate against peak prompt; and
variance, which dominated every series so far, gets a number.

First two batteries (README field notes for the tables). Baseline,
thinking off: eleven of fourteen landed, peak prompt under 7.4k on a
16k window; three of the four misses were the model sending `done`
where it should run `test` until the repeat cap parked it, and the
fourth was done-stays-done rejecting a reopened "run the tests" step
twelve times to a retry-cap failure with the crate green on disk.
Thinking on: fourteen of fourteen, a third fewer steps, 2.4 times the
wall time, a fifth more peak prompt, and seven patch-shape rejections
that constrained decoding had held at zero, so the server appears not
to enforce the schema after a thinking block. Consequences: implement's
`done` without a passing test becomes the harness running the test
(the review substitution's twin), done-stays-done defaults off, and
the maintainer's log entry records how much reasoning it was given so
section 9's premise can finally be measured rather than assumed.

Third build (verify guard keyed by tool so a `shell` run never hides a
`test`, identical skips capped, the test substitution ahead of the
premature-`done` note, a consumed handoff cleared on activation):
fourteen of fourteen landed and done with thinking off, median twelve
steps, four rejections, eleven minutes; thinking on landed thirteen
with one false `done` from the plan skill, closed by making review
the only skill that can terminate. Across three builds the landing
rate went 11, 13, 14 of 14 with the model, budgets, and context fixed;
each step was a harness state the log did not show until a bench run
made it show.

Fourth build (review the only terminal skill, implement's `done: true`
stripped with a note, a mixed-shape non-progress cap, a missing
`action` converted to a note): fourteen of fourteen landed and done in
both arms. Thinking off had zero rejections, the first battery to;
thinking on had twenty, nineteen of them patch-shape errors that
constrained decoding holds at zero without thinking, all fixed on the
retry. The shape rule stands: the server enforces the schema only when
no thinking block precedes the answer, so with thinking on the
harness's own parse is the schema.

### Tools write durable state

The model is a poor author of its own state: file notes without line
numbers, empty patches after reads, re-reading from the top after a
resume. The rules that carried it (silent reads, the read guard) exist
to force state out of it. The complement is to have the tools write
the state themselves. `Skill::after_tool` lets a skill turn a
successful tool call into a harness-sourced patch; the coding
workspace's `seen` list records, per file, its length and the line
ranges read and edited with the step of the last change. The model
reads it before re-reading and cannot patch it. Eviction and clearing
are the hook's, not the model's. Whether it lowers steps or peak
prompt is a bench question, answered against the baseline above.

## 11. Model backends

### First target: Gemma 4 on Ollama

- Endpoint: `/api/chat`, `stream: true`, `think: true`, `format: <json schema for {patch, action}>`.
- Thinking arrives in its own field → maps directly to `Reasoning` → session log. No tag parsing.
- Use `format` with the full `schemars`-derived schema rather than native tool calling. Native tool calling assumes the messages + tool-role append pattern being replaced; one constrained JSON object per step keeps patch and action atomic.
- Still validate: `format` guarantees JSON syntax, not necessarily full schema conformance in all modes.
- Stable-first prompt ordering exploits Ollama's prefix cache.

### Others (later)

- `OpenAiCompatModel` for vLLM (guided_json), llama.cpp server (GBNF from schema), LM Studio.
- `MockModel` replays scripted outputs for deterministic tests without a GPU.

## 12. Milestones

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
| 8 | Real coding sessions | Tool tuning (windowed reads, anchor edits, test summaries), argument aliases, non-progress guards, commitment slot, operator patches (see ARCHITECTURE.md "Guards against non-progress") | The toy task lands in 8 steps with zero rejections; each dogfood stall has a rule |
| 9 | Guides + subagents | `Report`, `ChildRunner`, `Delegate` action, guides with TTL pins (design in ARCHITECTURE.md "Milestone 9") | A child skill runs on its own workspace and its report lands in the parent's state |
| 10 | Context maintenance | `Maintainer` trait, `ModelMaintainer`, harness-owned `notes`, maintainer-proposed `next`, `budgets.notes`, `maintainer` config, `MockMaintainer` tests, eval columns (section 9) | The dogfood unit lands after a resume with no operator patches; a failed edit is re-sent from notes |
| 11 | Working set | `WorkingSet` derived from the log, `Section::WorkingSet`, `budgets.working_set`, `working_set` config, prompt-size test (section 10) | The shrunk dogfood unit lands; the section stays within budget |
| 12 | Rule audit | `guards` config switches, one-rule-off runs on the same unit, results recorded in README field notes (section 10 "After it works") | The smallest rule set that lands the unit is known and is the default |

## 13. Crate layout

```
skillstate/
├── core/        # types, traits, prompt gate, budget, validation, loop, maintainer
├── store/       # InMemory, Sqlite
├── patch/       # Replace, JsonMergePatch, JsonPatch, Typed
├── augment/     # RepoBm25, SessionLogBm25
├── models/      # Ollama, OpenAiCompat, Mock
├── tools/       # read, write, shell, test, recall
├── skills/      # coding, router, toy
├── tui/         # ratatui frontend, StepSink impl, slash commands
└── cli/         # binary
```

## 14. Dependencies

`serde`, `serde_json`, `schemars`, `json-patch`, `tokio`, `tokio-util` (CancellationToken), `reqwest`, `minijinja` (prompt templates), `tokenizers` or `tiktoken-rs` (budget), `rusqlite` (or `redb`), `tantivy` or a small BM25 impl, `ratatui`, `crossterm`, `thiserror`, `tracing`.

## 15. Open questions

- Patch strategy default: merge-patch is most reliable for small models; does Gemma 4 at this size handle RFC 6902 or typed ops well enough to make them the default?
- Observation truncation policy: head/tail split vs. model-requested windows (`read_more(offset)` tool).
- ~~Whether `recent_summary` should be model-owned only or harness-seeded with the last N action names.~~ Resolved by section 9: `recent_summary` stays model-owned; the harness keeps its own `notes` through the maintainer.
- Whether inbox acks are explicit patch fields or inferred from the model referencing an inbox id in its action.
- Embedding backend for augmentation once BM25 hits its limit (Ollama `/api/embed` is the obvious first choice).
