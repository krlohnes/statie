# statie — architecture and implementation contract

This document is the binding contract between the crates. It refines
`statie-spec.md` (the design plan) into concrete Rust APIs. When the two
disagree, this document wins; when this document is silent, the spec wins.

## Workspace layout

Cargo workspace, edition 2024, resolver 3. Directory → package name:

```
statie/
├── Cargo.toml          # [workspace] members below; shared [workspace.dependencies]
├── core/      statie-core     types, traits, prompt gate, budget, validation, loop, Session/Active
├── store/     statie-store    InMemoryStore, SqliteStore, JsonlLog
├── patch/     statie-patch    Replace, JsonMergePatch, JsonPatch, Typed
├── augment/   statie-augment  RepoBm25, SessionLogBm25
├── models/    statie-models   OllamaModel, OpenAiCompatModel, MockModel
├── tools/     statie-tools    read, write, edit, search, filter, shell, test, recall
├── skills/    statie-skills   toy, coding (plan/implement/review), router
├── tui/       statie-tui      ratatui frontend, StepSink impl, slash commands
└── cli/       statie          binary: config loading, wiring, `statie` commands
```

Dependency direction: everything depends on `statie-core`; `cli` depends on
everything; nothing depends on `cli` or `tui`. `tools` may depend on `store`
only through core traits (it must not).

All `async fn` in traits use `#[async_trait::async_trait]` so traits are
object-safe. Errors use `thiserror`. Logging uses `tracing`.

## Configuration (user requirement: nothing hardcoded)

The binary reads a JSON config. Resolution order, first hit wins:

1. `--config <path>`
2. `$STATIE_CONFIG`
3. `./.statie/config.json`
4. `~/.config/statie/config.json`
5. built-in defaults

Every field has a serde default so a partial file is valid. Shape:

```json
{
  "model": {
    "backend": "ollama",                    // "ollama" | "openai" | "mock"
    "base_url": "http://localhost:11434",   // ollama default; openai: "http://localhost:8000/v1"
    "model": "gemma4:26b",
    "think": true,
    "api_key": null,                        // openai-compat only; also $STATIE_API_KEY
    "num_ctx": 32768,                       // sent on every Ollama request; pinned (a change forces a model reload)
    "format": "json",                       // ollama `format`: "schema" | "json" (default) | "none"
    "schema_constrained": null,             // null = derived from format; false = also paste the schema into the system prompt
    "options": {},                          // passed through verbatim (ollama `options`, openai extra body)
    "timeout_secs": 600
  },
  "budget": {
    "total": 24000,       // whole prompt, tokens
    "observation": 3000,  // latest observation section cap
    "augmentation": 2000  // all augmenters combined
  },
  "retries": 3,           // rejected outputs before parking with AskUser
  "store": { "kind": "sqlite", "path": ".statie/session.db" },   // "memory" | "sqlite"
  "session_log": ".statie/session.jsonl",
  "sandbox_root": ".",
  "augmenters": { "repo_bm25": { "enabled": true, "budget": 1200 },
                  "session_log_bm25": { "enabled": true, "budget": 800 } }
}
```

`statie config init` writes this file with defaults; `statie config show`
prints the effective config. `statie.example.json` at the repo root mirrors it.

## Core types (`statie-core`)

### Versions and state

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version(pub u64);          // starts at 0 for the initial state

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Committed<H, S> { pub harness: H, pub agent: S }
pub type CommittedState<W> = Committed<HarnessState, W>;

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct HarnessState {
    pub inbox: Vec<InboxItem>,        // user messages; unacked ones render in the prompt
    pub active_tools: Vec<String>,    // tool names available to the current activation
    pub budgets: Budgets,
    pub activation: String,           // current skill name
    pub step: u64,                    // committed step counter
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem { pub id: u64, pub text: String, pub acked: bool }
```

`Committed.agent` is written only through `ValidatedPatch<S>`; `harness` is
written only by core's commit path (`pub(crate)` mutation).

### Prompt gate (A1)

```rust
mod sealed { pub trait Sealed {} }
pub trait Promptable: sealed::Sealed { fn render(&self) -> String; }
// impls (all in core): SystemPrompt, Arc<SkillSpec>, ToolList, AgentState<'_, S: Serialize>,
//                      HarnessView<'_>, Augmentation, Observation
pub struct Reasoning(String);        // Display, Serialize, Deserialize, AsRef<str>, Clone; NOT Promptable
pub struct ToolOutputFull(String);   // pub fn new(String); Display, AsRef<str>; NOT Promptable
```

`Observation` has no public constructor. It is created only by
`Observation::truncate(&ToolOutputFull, cap_tokens, &dyn Tokenizer)` and
`Observation::rejection(reason: &str)` / `Observation::tool_aborted(name)` /
`Observation::user_parked()` etc., all `pub(crate)`. It exposes
`as_str()`, `Serialize`, `Clone`, `Debug`. Truncation is head/tail: keep the
first ~60% and last ~40% of the cap with a `\n…[truncated N chars]…\n` marker.

### Budget and tokens

```rust
pub trait Tokenizer: Send + Sync { fn count(&self, s: &str) -> usize; }
pub struct HeuristicTokenizer;      // ceil(chars / 4); used in tests
pub struct TiktokenTokenizer;       // cl100k_base via tiktoken-rs; default at runtime
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Budgets { pub total: usize, pub observation: usize, pub augmentation: usize }
pub struct TokenBudget(pub usize);
```

### Bounded prompt (A5, B1)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Section { System, Spec, Tools, AgentState, HarnessView, Augmentation, Observation }

pub struct PromptBuilder<'t> { /* private */ }
impl<'t> PromptBuilder<'t> {
    pub fn new(tokenizer: &'t dyn Tokenizer, budgets: Budgets, version: Version) -> Self;
    pub fn system(self, s: &SystemPrompt) -> Self;
    pub fn spec(self, s: &Arc<SkillSpec>) -> Self;
    pub fn tools(self, t: &ToolList) -> Self;
    pub fn agent<S: Serialize>(self, a: &AgentState<'_, S>) -> Self;
    pub fn harness(self, h: &HarnessView<'_>) -> Self;
    pub fn augmentation(self, a: &Augmentation) -> Self;     // checked against budgets.augmentation
    pub fn observation(self, o: &Observation) -> Self;       // checked against budgets.observation
    pub fn build(self) -> Result<BoundedPrompt, OverBudget>;  // checks total
}
#[derive(Debug, thiserror::Error)]
#[error("prompt section {section:?} over budget: {tokens} > {limit}")]
pub struct OverBudget { pub section: Section, pub tokens: usize, pub limit: usize }

pub struct BoundedPrompt { /* private: system: String, user: String, version, tokens */ }
impl BoundedPrompt {
    pub fn system(&self) -> &str; pub fn user(&self) -> &str;
    pub fn version(&self) -> Version; pub fn tokens(&self) -> usize;
}
```

Section order is fixed (stable-first): System, Spec, Tools, AgentState,
HarnessView, Augmentation, Observation. `system()` is sections 1–3;
`user()` is sections 4–7. Each section is rendered with a `## <Section>`
heading. Exactly one system + one user message per step. No history array.

### Model output, schema (A10)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Tool { name: String, #[serde(default)] args: serde_json::Value },
    AskUser { message: String },
    Yield,
    Done,
}

/// The exact type the parser deserializes into and the schema is derived from.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepOutput<P> {
    pub patch: P,
    pub action: Action,
    /// Inbox ids the model has read and responded to. Yield/AskUser/Done
    /// are rejected while any inbox item is unacked.
    #[serde(default)] pub ack_inbox: Vec<u64>,
}

pub struct Schema(serde_json::Value);     // only constructor:
impl Schema { pub fn of<T: JsonSchema>() -> Schema; pub fn as_value(&self) -> &serde_json::Value; }

pub struct RawOutput { pub reasoning: Option<Reasoning>, pub content: String, version: Version }
impl RawOutput { pub fn new(prompt_version: Version /*from BoundedPrompt*/, reasoning, content) -> Self; pub fn version(&self)->Version }

pub struct Capabilities { pub native_thinking: bool, pub schema_constrained: bool, pub prefix_cache: bool }

pub trait StepSink: Send {
    fn reasoning_delta(&mut self, s: &str);
    fn content_delta(&mut self, s: &str);
}
pub struct NullSink;

#[async_trait]
pub trait Model: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn step(&self, prompt: BoundedPrompt, schema: &Schema, sink: &mut dyn StepSink,
                  cancel: CancellationToken) -> Result<RawOutput, ModelError>;
}
```

Backends without `native_thinking` are told (by the harness system prompt)
to open with a `<thoughts>…</thoughts>` block; the backend strips it into
`Reasoning` before returning. Backends without `schema_constrained` get the
schema pasted into the system prompt.

### Patch typestate (A2, A3, A6, B1)

```rust
pub struct RawPatch(pub serde_json::Value);    // parsed `patch` field, syntactically schema-shaped

pub trait PatchStrategy<S>: Send + Sync + 'static {
    /// The wire type of a patch. Its JsonSchema is what the model is constrained to.
    type Patch: Serialize + DeserializeOwned + JsonSchema + Send + Sync;
    fn name() -> &'static str;                        // e.g. "json-merge-patch (RFC 7386)"
    fn describe() -> &'static str;                    // one paragraph for the system prompt
    fn apply(cur: &S, patch: &Self::Patch) -> Result<S, PatchError>;
}

/// Zero-sized witnesses (private constructors, !Clone).
pub struct SchemaChecked(());  pub struct WithinBudget(());  pub struct InboxAcked(());

/// Patch after strategy application + schema check; `next` is the candidate agent state.
pub struct CheckedPatch<S> { /* raw: Value, next: S, version: Version, _w: SchemaChecked */ }
impl<S> CheckedPatch<S> { pub fn next(&self) -> &S; pub fn raw(&self) -> &serde_json::Value; }

/// Committable. Not Clone. Only `StateStore::commit` consumes it.
pub struct ValidatedPatch<S> { /* raw: Value, next: S, version: Version */ }
impl<S> ValidatedPatch<S> {
    pub fn version(&self) -> Version;      // version it was validated against
    pub fn next(&self) -> &S;              // new agent state (stores persist this)
    pub fn raw(&self) -> &serde_json::Value;// patch JSON (stores log this)
    pub fn into_parts(self) -> (serde_json::Value, S, Version);  // for stores
}
```

### Skill

```rust
pub struct SkillSpec { /* private */ }
impl SkillSpec { pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self;
                 pub fn name(&self) -> &str; pub fn instructions(&self) -> &str; }
// No &mut accessors (C1).

pub trait Skill: Send + Sync + 'static {
    type Workspace: Serialize + DeserializeOwned + JsonSchema + Clone + Send + Sync + 'static;
    type Patch: PatchStrategy<Self::Workspace>;
    fn name(&self) -> &str;
    fn spec(&self) -> Arc<SkillSpec>;
    fn initial_workspace(&self) -> Self::Workspace;
    fn tools(&self) -> Vec<String>;                     // active tool names
    fn allowed_transitions(&self) -> Vec<String> { vec![] }   // skill names this one may hand off to
    fn is_terminal(&self, s: &Self::Workspace) -> bool;
    /// Skill-specific rules on the candidate next state (e.g. recent_summary length cap).
    fn rules(&self, _cur: &CommittedState<Self::Workspace>, _next: &Self::Workspace) -> Result<(), PatchError> { Ok(()) }
    /// Full validation. Provided; skills override `rules`, not this.
    fn validate(&self, cur: &CommittedState<Self::Workspace>, out: ParsedOutput<Self::Workspace>)
        -> Result<ValidatedStep<Self::Workspace>, PatchError> { /* core::validate::validate_default */ }
}
```

`ParsedOutput<W>` is produced by core's parser from `RawOutput` (it holds the
`StepOutput<P::Patch>` re-serialized as `RawPatch` + `Action` + acks +
version). Default `validate`: apply `Self::Patch` (→ `CheckedPatch`), run
`rules`, check inbox acks (→ `InboxAcked` witness required by
`ValidatedStep::parking`), stamp version → `ValidatedStep<W>`.

```rust
pub struct ValidatedStep<W> { /* patch: ValidatedPatch<W>, action: Action, acks: Vec<u64> */ }
impl<W> ValidatedStep<W> {
    pub fn tool(patch: ValidatedPatch<W>, acks: Vec<u64>, name: String, args: Value) -> Self;
    pub fn parking(patch: ValidatedPatch<W>, acks: Vec<u64>, action: Action, _w: InboxAcked) -> Self; // panics/Err if action is Tool
}
pub fn check_inbox(cur: &HarnessState, acks: &[u64]) -> Result<InboxAcked, PatchError>; // all unacked ids ⊆ acks
```

### Store, log, augmenter, tool traits

```rust
#[async_trait]
pub trait StateStore<W>: Send + Sync {
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError>;
    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError>;  // version 0
    /// Atomic: persists patch.next() as agent, `harness` as harness, appends (patch.raw(), action) to the patch log.
    async fn commit(&self, patch: ValidatedPatch<W>, harness: HarnessState, action: &Action, expect: Version)
        -> Result<Version, StoreError>;   // Err(StoreError::VersionMismatch{expected, actual}) if expect != current
    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError>;
    /// Reapply the patch log from the initial snapshot up to `to` using `apply` (build one with `apply_fn::<W, P>()`).
    async fn replay(&self, to: Version, apply: ApplyFn<'_, W>) -> Result<CommittedState<W>, StoreError>;
    /// Overwrite the harness region only (used for inbox pushes and activation switches). Bumps no version.
    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggedPatch { pub version: Version, pub patch: Value, pub action: Action, pub harness: HarnessState }
pub type ApplyFn<'a, W> = &'a (dyn Fn(&W, &Value) -> Result<W, PatchError> + Sync);
pub fn apply_fn<W, P: PatchStrategy<W>>() -> impl Fn(&W, &Value) -> Result<W, PatchError> + Sync;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind { User, Reasoning, Patch, Action, ToolOutput, System }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry { pub step: u64, pub kind: LogKind, pub ts: String /* RFC3339 */, pub payload: Value }

pub trait SessionLog: Send + Sync {          // interior mutability; shared as Arc<dyn SessionLog>
    fn append(&self, e: LogEntry) -> Result<(), LogError>;
    fn entries(&self, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry>;   // newest last
    fn search(&self, query: &str, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry>;
    fn last_step(&self) -> u64;
}
pub struct MemoryLog;   // in core; substring search (BM25 lives in statie-store's JsonlLog)

#[must_use = "a step is not complete until its record is appended to the SessionLog"]
pub struct StepRecord { /* entries: Vec<LogEntry>, outcome: Outcome */ }
pub trait SessionLogExt { fn append_record(&self, r: StepRecord) -> Result<Outcome, LogError>; }  // blanket impl

#[async_trait]
pub trait Augmenter<W>: Send + Sync {
    fn name(&self) -> &str;
    async fn augment(&self, s: &W, obs: Option<&Observation>, budget: TokenBudget, tok: &dyn Tokenizer) -> Vec<Snippet>;
}
pub struct Snippet { pub source: String, pub text: String }   // pub fields; Augmentation::from_snippets is pub(crate)
pub struct Augmentation { /* Vec<Snippet> */ }

pub struct SandboxRoot(PathBuf);       // pub fn new(path) -> io::Result<Self> (canonicalizes)
pub struct SandboxPath(PathBuf);       // no public constructor
impl SandboxRoot { pub fn join(&self, rel: &str) -> Result<SandboxPath, SandboxError>; pub fn path(&self)->&Path }
impl SandboxPath { pub fn as_path(&self) -> &Path; pub fn relative(&self, root:&SandboxRoot) -> PathBuf }
// join: reject absolute paths & `..` escapes; canonicalize the deepest existing ancestor and require it to start with root.

pub struct ToolCtx { pub root: SandboxRoot, pub log: Arc<dyn SessionLog>, pub cancel: CancellationToken }
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn args_schema(&self) -> Schema;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError>;
}
pub struct ToolSet { /* HashMap<String, Arc<dyn Tool>> */ }   // register, get, names, list_for(&[String]) -> ToolList
```

### Loop, session, activation (A4, A7, A8, B3, B4)

```rust
pub enum Event { User(String), Tick, Cancel }

pub enum Outcome {
    Continue,                  // a tool ran; observation is pending; call step(Event::Tick) again
    Parked(Park),              // wait for the next User event
    Done,                      // activation terminal
    Handoff(String),           // router chose the named skill
}
pub enum Park { AskUser(String), Yield, Failure(String) /* retry cap hit */, Cancelled }

pub struct Executable<A> { /* private; returned only by Session::commit */ }

pub struct SessionConfig { pub budgets: Budgets, pub retries: u32, pub root: SandboxRoot }

/// Cloneable handle that cancels whichever `CancellationToken` is current
/// for the in-flight `step()` call. `Session` installs a fresh token at the
/// start of every `step`, so a handle obtained at any point still cancels
/// whichever step is running (or the next one, if none is).
pub struct CancelHandle(/* Arc<Mutex<CancellationToken>> */);
impl CancelHandle {
    pub fn cancel(&self);
}
impl Clone for CancelHandle {}

pub struct Session<W> {
    pub(crate) store: Box<dyn StateStore<W>>,
    pub(crate) log: Arc<dyn SessionLog>,
    pub(crate) model: Box<dyn Model>,
    pub(crate) augmenters: Vec<(Box<dyn Augmenter<W>>, TokenBudget)>,
    pub(crate) tools: ToolSet,
    pub(crate) tokenizer: Box<dyn Tokenizer>,
    pub(crate) config: SessionConfig,
    /* pending_observation: Option<Observation>, failures: u32, cancel: CancelHandle,
       last_activation: Option<(String /*name*/, Vec<String> /*allowed_transitions*/)> */
}
impl<W> Session<W> {
    pub fn new(store: Box<dyn StateStore<W>>, log: Arc<dyn SessionLog>, model: Box<dyn Model>,
               tools: ToolSet, tokenizer: Box<dyn Tokenizer>, config: SessionConfig) -> Self;
    pub fn with_augmenter(self, a: Box<dyn Augmenter<W>>, budget: TokenBudget) -> Self;
    /// Sole constructor of Active. Initializes the store if empty (or updates
    /// `activation`/`active_tools` if not). Errors with
    /// `SessionError::TransitionNotAllowed` if the session's remembered last
    /// activation forbids `skill` (checked against that skill's own
    /// `allowed_transitions()`, recorded at the time it was activated).
    pub async fn activate<K: Skill<Workspace = W> + ?Sized>(&mut self, skill: Arc<K>) -> Result<Active<'_, K>, SessionError>;
    /// Same as `activate`, but skips the transition-allowed check.
    pub async fn activate_forced<K: Skill<Workspace = W> + ?Sized>(&mut self, skill: Arc<K>) -> Result<Active<'_, K>, SessionError>;
    pub async fn state(&self) -> Result<(CommittedState<W>, Version), SessionError>;
    pub async fn push_user(&mut self, text: String) -> Result<(), SessionError>;   // inbox append + log User
    pub fn cancel_handle(&self) -> CancelHandle;                                   // Ctrl-C: handle.cancel(). Fresh token installed per step.
    pub fn log(&self) -> Arc<dyn SessionLog>;
    pub fn config(&self) -> &SessionConfig;
}
pub struct Active<'a, K: Skill + ?Sized> { /* session: &'a mut Session<K::Workspace>, skill: Arc<K>, spec: Arc<SkillSpec>, schema: Schema */ }
impl<'a, K: Skill + ?Sized> Active<'a, K> {
    pub async fn step(&mut self, ev: Event, sink: &mut dyn StepSink) -> Result<StepRecord, SessionError>;
    /// `/edit`: a user-authored patch, applied through the skill's patch
    /// strategy and `rules` (inbox acks are not required — the action is
    /// always `Yield`). Logs a Patch entry with an extra `"source":"user"` field.
    pub async fn user_patch(&mut self, patch: Value) -> Result<StepRecord, SessionError>;
    /// Reapplies the patch log from the initial snapshot up to `to`.
    pub async fn replay(&self, to: Version) -> Result<CommittedState<K::Workspace>, SessionError>;
    pub fn session(&mut self) -> &mut Session<K::Workspace>;
    pub fn last_prompt_tokens(&self) -> Option<usize>;
    pub fn skill_name(&self) -> &str;
}
```

`step` algorithm:

1. `Event::User(t)` → `push_user(t)`. `Event::Cancel` → cancel token, return `Parked(Cancelled)` record. `Tick` → nothing. A fresh `CancellationToken` is then installed into the session's `CancelHandle` for this step.
2. Load state + version. If `skill.handoff(agent)` is `Some(name)` → `Handoff(name)` without calling the model. Else if `skill.is_terminal(agent)` → `Done`.
3. Run augmenters (each with its own budget; total capped by `budgets.augmentation`). The Augmentation section is omitted entirely when no augmenter returns a snippet.
4. Build `BoundedPrompt` (sections above) with the current version. `OverBudget` → `SessionError::OverBudget` (not retried).
5. `model.step(prompt, &Schema::of::<StepOutput<<K::Patch as PatchStrategy<W>>::Patch>>(), sink, cancel)`. Cancelled → `Parked(Cancelled)`; nothing committed. Log reasoning (kind Reasoning) regardless, including on outputs that go on to be rejected.
6. Parse content → `ParsedOutput<W>`. On failure → pending observation `"output rejected: <reason>"`, failures += 1, log System `{"event":"rejected", "reason"}`; if failures > retries → reset failures, log System `{"event":"failure"}`, `Parked(Failure)`; else `Continue`. State unchanged.
7. `skill.validate(&cur, parsed)` → same rejection path on error, with the validator's message.
8. `commit`: assert `patch.version() == version` (B1) else reject as stale (`"stale version"`); build next `HarnessState` (acks applied, step+1, active_tools = skill.tools(), activation = skill.name()); `store.commit(...)`; wrap the action in `Executable::new` only here. failures = 0. Log Patch `{"version","patch"}` + Action entries.
9. `execute(Executable<Action>)`: `Tool` → look up in ToolSet (unknown tool → rejection observation, log System `tool_error`, `Continue`, not counted as a validation failure), run with cancel via `tokio::select!`; output logged whole as ToolOutput `{"name","output","truncated"}`; `Observation::truncate` becomes pending; `Continue`. A tool `Err` → log System `tool_error`, rejection observation, `Continue`. Cancel mid-tool → `Observation::tool_aborted` pending, log System `cancelled`, `Parked(Cancelled)`. `AskUser`/`Yield` → `Parked`. `Done` → checks `skill.handoff` again, then `Done`/`Handoff`.
10. Return `StepRecord { entries, outcome }` — caller must `log.append_record(rec)` to get the `Outcome`.

Retry counter resets on successful commit. Pending observation is cleared once it has been put into a prompt (taken via `Option::take`).

### System prompt contents

Rendered by `SystemPrompt::for_harness(caps: &Capabilities, strategy_name, strategy_desc, schema: Option<&Schema>)`:
output contract (`{patch, action, ack_inbox}` object, exactly one, no prose),
patch strategy description, action semantics, tool call convention,
"the session history is searchable with the `recall` tool; `step` in the
harness view counts committed steps", inbox/ack rule, `<thoughts>` request
when `!native_thinking`, schema text when `!schema_constrained`.

## Decisions on the spec's open questions

- Default patch strategy: `JsonMergePatch` (RFC 7386). Its `Patch` type is
  `MergePatch<S>` whose JsonSchema is `S`'s schema with `required` removed at
  every object level (so any subset is valid; `null` deletes).
- Observation truncation: head/tail split (60/40).
- `recent_summary`: model-owned; coding skill's `rules` caps it at 1200 chars.
- Inbox acks: explicit `ack_inbox: [ids]` on the step output.
- Embeddings: out of scope; BM25 only.

## Milestone 9 (design): guides and subagents

Goal: let one bounded agent call a second bounded agent for a self-contained
job and get back a small, typed result — without a messaging fabric, without
concurrent agents, and without the parent's prompt growing. Subagents are
**state composition**: a child's result is a value stored in the parent's
workspace, written by the harness, never by the model. Guides are the
procedural text a child (or, briefly, the parent) works from, loaded only
while needed.

Non-goals for this milestone: parallel children, child-to-child messaging,
children that talk to the user, Rust-defined child workspace types.

### Guides (on-demand procedure text)

A guide is a named text blob with a token cap. Guides live in config, not
code:

```json
"guides": {
  "ui-review": { "path": "guides/ui-review.md", "max_tokens": 600 },
  "rust-testing": { "text": "Run `cargo test` first ...", "max_tokens": 200 }
}
```

- Always-on cost is one line in the Harness View: `guides: ui-review,
  rust-testing (call \`guide\` to load one)`, omitted when no guides exist.
- `guide` tool, args `{ "name": string | null }`: pins the guide (or unpins
  with `null`). The pinned text renders in a new prompt section **Pinned**
  (after Spec, before Tools), budgeted by its own `max_tokens` and counted
  against `budgets.total`. A pin has a TTL of `guides.ttl_steps` (default 6)
  committed steps, refreshed by calling `guide` again; expiry is silent
  except for the `pinned:` line disappearing from the Harness View.
- Pin state is harness-owned: `HarnessState.pinned: Option<Pinned { name,
  expires_at_step }>`. Patches cannot touch it.
- A skill may declare a guide as permanently pinned for itself
  (`SkillSpec::with_guide(name)` in Rust, `"guide": "ui-review"` for a
  configured child skill). That is how a subagent carries its procedure
  without the parent ever seeing it.
- TUI: `/guide <name>` and `/guide` (unpin) send the same request through
  the driver; the Harness View line shows `pinned: ui-review (4 steps left)`.

### Report: the one shape a child returns

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub agent: String,                    // child skill name
    pub task: String,                     // the parent's delegation text, verbatim
    pub outcome: ReportOutcome,           // Done | Blocked | Failed | Capped
    pub summary: String,                  // <= 1200 chars; also the parent's observation
    pub findings: Vec<String>,            // empty = nothing to fix
    pub files: Vec<FileNote>,             // touched or judged relevant
    pub data: serde_json::Map<String, Value>, // skill-specific, schema-checked (below)
    pub stats: ReportStats,               // steps, server prompt/completion tokens, wall_ms
}
pub enum ReportOutcome { Done, Blocked, Failed, Capped }
```

Deliberately general: `findings` and `files` are first-class because the
coding rules and the TUI already understand them; everything else goes in
`data`. Nothing about a review, a UI check, or a research task is encoded in
the type — those differences live in the child's config and its `data`
schema.

**The child's workspace is the Report.** A configured child runs as
`Session<ReportWorkspace>` where `ReportWorkspace` is `Report` minus the
harness-filled fields (`agent`, `task`, `stats`) plus `recent_summary` and
`done`. Its rules: `done` requires non-empty `summary`, `outcome != Done`
requires non-empty `findings` or a `data.reason`, and `data` must validate
against the child's declared JSON schema (via the `jsonschema` crate;
validation errors are rejections with the schema path, like patch errors).
No new generics: every child is the same Rust type; the schema for `data` is
the only per-child variation.

### Configured child skills

```json
"subagents": {
  "ui-review": {
    "instructions": "Build the app, take a screenshot, and report ...",
    "tools": ["shell", "read_file", "search", "screenshot", "filter_last_tool_call"],
    "guide": "ui-review",
    "data_schema": { "type": "object", "properties": { "renders": {"type":"boolean"},
                     "issues": {"type":"array","items":{"type":"string"}} },
                     "required": ["renders"] },
    "budgets": { "total": 12000, "observation": 2000, "augmentation": 800 },
    "max_steps": 30
  }
}
```

A child skill is instructions + tools + optional guide + optional `data`
schema + budgets + a step cap. Rust-defined skills (`ToySkill`, the coding
four) are not delegation targets in this milestone; a child is always a
config stanza.

### The `delegate` action

```rust
pub enum Action { Tool{..}, AskUser{..}, Yield, Done,
                  Delegate { skill: String, task: String } }
```

Handled by the session in `execute`, not by a tool (tools have no model or
registry). Semantics:

1. Validation: the parent skill must list the child in
   `allowed_delegates()` (new provided `Skill` method, default empty; the
   coding skills list the configured children they may call). Depth: a
   child may not delegate (`max_depth = 1` in this milestone; the field
   exists so 2 can be enabled later). Unknown child or depth violation →
   rejection, not a failure.
2. The parent's patch is committed first (step N), exactly like a tool call.
   The delegation is logged as Action `{"type":"delegate","skill","task"}`.
3. The session builds the child: `Session<ReportWorkspace>` over a **child
   store** (`memory` for `--memory`, otherwise a second sqlite region keyed by
   agent path `implement/ui-review#3`), the **shared** log (entries carry
   `agent: "<path>"`; the parent's entries carry none; `recall` defaults to
   the caller's own path and accepts `agent: "*"`), the shared model, the
   shared tool set restricted to the child's list, the shared sandbox, the
   child's budgets, and a fresh `CancelHandle` chained to the parent's (parent
   cancel cancels the child). `Session.model` becomes `Arc<dyn Model>` to
   allow this.
4. The child runs to `Done`, `Parked(Failure)`, its step cap, or cancel, with
   `Event::User(task)` as its first event. `ask_user` and `yield` are
   rejected in children (`allows_yield() == false`, `allows_ask_user() ==
   false`, new provided method); a blocked child sets `outcome: Blocked` and
   says why in `summary`/`data.reason`. The TUI receives the child's stream
   through the same `StepSink`, tagged with the agent path, and renders it as
   one collapsible block: `▸ subagent ui-review · 12 steps · 3.1k tok`.
5. On return, the harness (not the model) writes the parent's
   `reports[<child name>] = Report` — parent workspaces that want delegation
   carry `#[serde(default)] pub reports: BTreeMap<String, Report>`, and the
   harness reaches it through a new provided `Skill` method
   `fn absorb_report(&self, ws: &mut W, report: Report)` whose default for
   the coding workspace inserts into `reports`. This write bypasses the
   model's patch path but still runs `rules`; it is committed as step N+1
   with a Patch log entry marked `"source":"subagent"` so replay is exact.
6. The parent's pending observation becomes
   `subagent ui-review finished (done, 12 steps): <summary>. Merged into
   reports["ui-review"]: findings (2), files (1), data keys [renders, issues].`
   It is truncated like any tool output and charged to the observation
   budget. Outcome `Continue`.

Cancel mid-child: the child's partial state is committed as `outcome:
Capped` with whatever `summary` it had (or a harness-written one), the
parent parks `Cancelled` as today. Re-delegating the same child overwrites
`reports[name]`; the old report stays in the patch log.

### Rules over reports (no Rust required)

Parent rules stay Rust for the coding skills, but configured children need a
way to gate the parent without code. A small predicate language in the
skill's config stanza, evaluated by `validate_default` after `rules`:

```json
"rules": [
  { "when": { "action": "done" },
    "require": { "pointer": "/reports/review/outcome", "eq": "done" },
    "message": "run delegate(review) and get a clean report before done" },
  { "when": { "action": "done" },
    "require": { "pointer": "/reports/review/findings", "empty": true } }
]
```

Predicates: `eq`, `ne`, `exists`, `empty`, `non_empty`, `gt/lt` on numbers;
`when` matches on action type and optionally on the patch touching a
pointer. Messages are shown to the model verbatim as rule violations. This
is intentionally not a general expression language.

### Prompt and budget accounting

- Parent prompt: unchanged sections plus **Pinned** when a guide is pinned.
  `reports` renders inside AgentState like any field; a report's `summary`
  is capped at 1200 chars and `data` at 600 tokens when rendered (the full
  value stays in state; the rendering truncates with a marker).
- Child prompt: its own `BoundedPrompt` with its own budgets. The parent
  pays only for the observation and the `reports` entry.
- Server usage from child steps is summed into `ReportStats` and shown in
  the TUI block header; `eval` reports parent and child totals separately.

### Changes to existing types (summary)

- `Action::Delegate`; `Skill::{allowed_delegates, allows_ask_user,
  absorb_report}` (provided); `HarnessState::pinned`; `Session.model:
  Arc<dyn Model>`; `LogEntry.agent: Option<String>`; a `ChildSkill`
  (configured) type in `statie-skills` implementing `Skill<Workspace =
  ReportWorkspace>`; `Report`/`ReportWorkspace`/`ReportStats` in
  `statie-core`; `SqliteStore` gains a `region` (agent path) so one file
  holds parent and children; `RunOptions`/CLI gain nothing new (children are
  reached only through delegation).
- New tool `guide`; new tool `screenshot` (Playwright via `npx`, PNG into
  the sandbox) and an image-bearing observation (`Observation::Image { path,
  caption }`, sent as Ollama `images`, budgeted at a fixed 512 tokens) so a
  vision-capable child can look at what it built. The image never enters the
  log; the path does.

### Testing plan

- Unit: `Report` schema has no `required` inside `MergePatch`; `data_schema`
  validation rejects with a path; predicate evaluation table-tested;
  `Pinned` TTL expiry and refresh; `guide` tool pins/unpins.
- Session: parent delegates to a mock-scripted child; assert the child's
  prompts are bounded and never contain the parent's workspace; the parent's
  next prompt contains the observation line and `reports["x"]`; the patch log
  has a `source: subagent` entry; replay reproduces the parent state; cancel
  mid-child yields `Capped`; a child that tries `ask_user` is rejected;
  depth-2 delegation is rejected.
- CLI: a `subagents` stanza round-trips through `config show`; a mock
  headless run that delegates prints the collapsed block.
- Live (field notes, not CI): coding task with `ui-review` child on
  gemma4:26b — the parent's `srv_max` must not grow with the child's step
  count.

## Testing conventions

- Unit tests live beside code (`#[cfg(test)]`). Integration tests in `<crate>/tests/`.
- Compile-time invariants get `compile_fail` doctests in `core/src/lib.rs`
  (A1 Reasoning not Promptable; A2 store commit rejects RawPatch; A3
  ValidatedPatch not Clone; A9 tool APIs reject `Path`).
- `MockModel` (in `statie-models`) replays a `Vec<MockTurn { reasoning, content }>`
  and records every `BoundedPrompt` it received (`prompts()`), so tests assert
  prompt size stays flat across steps.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets -D warnings`
  must both pass at every milestone.
