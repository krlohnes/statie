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
2. Load state + version. If `skill.is_terminal(agent)` → `Done`.
3. Run augmenters (each with its own budget; total capped by `budgets.augmentation`). The Augmentation section is omitted entirely when no augmenter returns a snippet.
4. Build `BoundedPrompt` (sections above) with the current version. `OverBudget` → `SessionError::OverBudget` (not retried).
5. `model.step(prompt, &Schema::of::<StepOutput<<K::Patch as PatchStrategy<W>>::Patch>>(), sink, cancel)`. Cancelled → `Parked(Cancelled)`; nothing committed. Log reasoning (kind Reasoning) regardless, including on outputs that go on to be rejected.
6. Parse content → `ParsedOutput<W>`. On failure → pending observation `"output rejected: <reason>"`, failures += 1, log System `{"event":"rejected", "reason"}`; if failures > retries → reset failures, log System `{"event":"failure"}`, `Parked(Failure)`; else `Continue`. State unchanged.
7. `skill.validate(&cur, parsed)` → same rejection path on error, with the validator's message. If the validated patch changes `skill.handoff(next)` relative to `skill.handoff(cur.agent)` and the action is not `Done`, reject the same way: a handoff target may only change together with action `done`, otherwise the *next* step would silently hand off before the model ever saw this step's tool result.
8. `commit`: assert `patch.version() == version` (B1) else reject as stale (`"stale version"`); build next `HarnessState` (acks applied, step+1, active_tools = skill.tools(), activation = skill.name()); `store.commit(...)`; wrap the action in `Executable::new` only here. failures = 0. Log Patch `{"version","patch"}` + Action entries.
9. `execute(Executable<Action>)`: `Tool` → look up in ToolSet (unknown tool → rejection observation, log System `tool_error`, `Continue`, not counted as a validation failure), run with cancel via `tokio::select!`; output logged whole as ToolOutput `{"name","output","truncated"}`; `Observation::truncate` becomes pending; `Continue`. A tool `Err` → log System `tool_error`, rejection observation, `Continue`. Cancel mid-tool → `Observation::tool_aborted` pending, log System `cancelled`, `Parked(Cancelled)`. `AskUser`/`Yield` → `Parked`. `Done` → checks `skill.handoff` again, then `Done`/`Handoff`.
10. Return `StepRecord { entries, outcome }` — caller must `log.append_record(rec)` to get the `Outcome`.

Retry counter resets on successful commit. Pending observation is cleared once it has been put into a prompt (taken via `Option::take`).

### Guards against non-progress (field-driven)

Every one of these came out of a live run in which the model burned 10 to 60
steps without changing anything; each is a harness rule, not a prompt hint,
so it holds when the model ignores instructions.

- **Loop guard.** The last 8 committed tool calls (name, args, output or
  error text) are kept; the third identical triple gets a note prepended to
  its observation saying nothing has changed. Tool errors count.
- **Read guard.** Consecutive read-only calls (`read_file`, `search`,
  `filter_last_tool_call`, `list_files`, `recall`) on one path are counted;
  any mutating call (`write_file`, `edit_file`, `shell`, `test`) resets all
  counters. From the fourth read of a path with no edit in between, the
  observation says so and asks for the edit or for the line numbers to be
  written into the workspace.
- **Active tools.** A call to a registered tool the skill does not list is
  rejected with the skill's list; the prompt and the executor use the same
  list.
- **Handoff only on `done`.** A patch that changes `skill.handoff(next)`
  under any other action is rejected (step 7 above).
- **Rejections change the prompt.** Every rejection observation carries
  `attempt k of n`; the second identical reason adds a sentence demanding a
  different output, so a low-temperature model cannot replay itself into the
  retry cap.
- **Commitment.** A workspace may expose a `Commitment { tool, path, what }`
  through `Skill::commitment`; the coding workspace stores it in `next`.
  While a mutating commitment is set, the model may read the committed path
  at most twice; any other read-only call is rejected ("make that call now,
  or change `next` and say why"). A matching mutating call spends the
  commitment: the harness clears `next` with a `source: "harness"` patch and
  the observation says so. The Harness View shows the open commitment and
  the reads left. This exists because reasoning is discarded by design: a
  decision that is not state is lost, and the model demonstrably makes the
  decision, then reads "one more thing", then forgets it. The coding skills
  validate the slot itself (`commitment_rules`): `next.tool` must be one of
  `edit_file`, `write_file`, `shell`, `test` (a commitment is the change,
  not the look; a `read_file` commitment once deadlocked against the read
  cap), `what` must be non-empty, and `edit_file`/`write_file` need a
  `path`.
- **Silent reads.** `Skill::silent_read_cap` (default `None`; the
  implement skill returns `Some(2)`) bounds consecutive read-only calls
  whose patch wrote nothing to the workspace while no commitment is set.
  A read that records anything (a file note, a plan step, a summary line)
  is never counted and resets the streak; a mutating call or a new
  commitment resets it too. Past the cap, a silent read is rejected with
  what to write, and a hint to `search` rather than page. This replaced a
  plain count of consecutive reads, which fired on three legitimate looks
  at a 989-line file: reading is not the problem, reading that leaves
  nothing in state is (the paper's limitation two in miniature).
- **Redundant verification.** The most recent `shell`/`test` command is
  kept (normalized: trimmed, cut at the first pipe, `2>&1` dropped) with
  the first line of its result. Running the same command again with no
  `edit_file`/`write_file` since does not run it: the step's patch commits,
  the call is skipped, a `skipped_redundant` System entry is logged, and
  the observation quotes the earlier result ("nothing has changed since
  you last ran ...; make the change first"). Only an exact repeat of the
  latest command is caught; A, B, A is fine. This was a rejection until a
  live run showed the model marking its plan step done in the same output
  as the redundant call: rejecting the step threw the progress away.
- **Done stays done.** A patch that moves a plan step from `done` back to
  `pending` or `in_progress` is rejected (`plan_rules` in the coding
  skills), naming the first open step to work on instead, or "add a new
  step for new work". After a resume the model has three times re-read its
  plan from the top, reopened step 1 ("run the tests to observe the
  errors"), and redone finished work. `done -> skipped` and reopening a
  skipped step are allowed; operator patches go through the same rule.
- **Redundant reads.** Every successful read-only call is remembered by
  (tool, args) until the next mutating call. An identical call before any
  mutation is not re-run: the patch commits, the call is skipped, and the
  observation names the step that already ran it (read a different range,
  search, or make the committed change). `filter_last_tool_call` is
  exempt (re-reading is its job). A skipped call never fulfils a
  commitment and does not touch the read counters.
- **Repeated failure.** The session keeps the normalized failure summary
  of the most recent failing `test` (the text after `--- failure summary
  ---` up to the next section) and how many consecutive failing runs
  have shared it. From the second identical failure the observation is
  prefixed with a note saying so, in one of two shapes: "your edit(s)
  since did not change it; read the exact lines the error points to"
  when an `edit_file`/`write_file` ran in between, or "nothing was
  edited in between" when none did. A `repeated_failure` System entry
  carries the count and which shape. A pass or a different failure
  clears the memory. Thirteen of the thirty-four runs from 21 to 54 had
  three or more `test` runs fail with the same first line, the worst
  twenty-two in a row on one unclosed brace, and the model never
  noticed the text had not changed.

### Output schema shape

`Schema::of::<StepOutput<P>>()` is what the backend sends as `format` in
schema mode and what the prompt pastes otherwise. `Action` implements
`JsonSchema` by hand as one flat object (`type` enum + optional `name`,
`args`, `message`, `skill`, `task`, `required: ["type"]`) instead of the
derived `oneOf` over variants: under grammar-constrained decoding a small
model took the shortest branch (`{"type":"yield"}`) while its reasoning
said to call a tool. Serde parsing of the tagged enum is unchanged; tests
in `core/src/model.rs` assert no `oneOf`/`anyOf`/`allOf` under `action`
and validate every variant against the schema.

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

**Delegate executor implementation notes (deviations from the design
above):** the `delegate` unit's `ConfiguredChildRunner` always runs a child
over an `InMemoryStore<ReportWorkspace>` rather than a sqlite region, since
`SqliteStore` gaining a `region` concept is itself listed above as a change
still to make, not something to build against; and `statie-skills` gained
real (not dev-only) dependencies on `statie-store`, `async-trait`, and
`tokio-util`, since `ConfiguredChildRunner` needs `InMemoryStore` and must
implement the `async_trait`-based `ChildRunner` trait.

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

## Milestone 10: context maintenance

Design: statie-spec.md section 9. The paper drops the reasoning trace once
the patch is validated; every dogfood stall was a decision that lived in
that trace and died with it. The maintainer is a second model call per
step whose only job is to distil the trace into a bounded, harness-owned
`notes` field before it is gone. State stays authoritative; notes are one
field, not a transcript.

### Types (`core/src/maintainer.rs`)

- `Maintainer::maintain(MaintainInput) -> Result<MaintainOutput, MaintainError>`.
  Input: this step's reasoning text, the raw patch, the action, the
  observation the next step will see, the committed agent state, the open
  commitment, the previous notes, and `budgets.notes` as a `TokenBudget`.
  Output: new notes and an optional `next` commitment.
- `MaintainError::{Backend, Parse, OverBudget, Timeout}`.
- `MockMaintainer` (scripted outputs, records every call) for tests.
- `ModelMaintainer` (`statie-models`) drives any `Model` with
  `SystemPrompt::for_maintainer` and a flat output schema; one retry when
  the notes come back over budget, then `OverBudget`.
- `Notes` is a sealed `Promptable` with a crate-private constructor; the
  session wraps `Skill::notes(ws)` into it for rendering.

### Session (`run_maintainer`)

Runs after `Outcome::Continue` and after `AskUser`/`Yield` parks; never on
a rejection, `Done`, cancel, or failure. Applies `{"notes", "next"?}`
through the trusted-patch path shared with operator patches, logged with
`source: "maintainer"`, plus a System entry `{"event":"maintainer",
"notes_tokens", "next_set"}`. `next` is included only when the workspace's
own commitment is `None`; if the skill rejects it, the patch is retried
once without `next`. Any error logs `{"event":"maintainer_error","reason"}`
and keeps the previous notes; the step outcome never changes. `StepSink`
gets `maintainer_done` / `maintainer_failed` hooks.

### Ownership

`Skill::harness_owned_fields` names top-level keys the model may not
change; `validate_default` rejects a model patch whose post-patch value for
such a key differs from the committed value ("field `notes` is
harness-owned in skill `implement` and cannot be patched"). Operator and
maintainer patches skip the check. The coding workspace owns `notes` and
`reports`; the earlier `reports` shape heuristic is gone.

### Prompt and budget

`Section::Notes` renders between the agent state and the harness view under
"notes (kept by the harness from your previous reasoning):", only when
non-empty, against `budgets.notes` (default 400). The maintainer's own
prompt carries no spec and no tool list: previous notes, agent state,
commitment, patch, action, reasoning, observation, in that order, with
reasoning and observation truncated head+tail to fit.

### Config and eval

`maintainer: { model?, budget, timeout_secs, prompt_budget }`; `model` has the
shape of the top-level `model` and defaults to it; `budget` overrides
`budgets.notes`. The TUI and headless output print `[maintainer] notes: N
tokens` (plus `, set next`) and `[maintainer error] <reason>`. `statie eval` reports `rej`,
`tool_err`, `edits` (landed/attempted), `first_edit`, and `maint`
(runs/errors) per harness, so the maintainer's effect on stalls is
measurable on the same task.

## Milestone 11: working set

Design: statie-spec.md section 10. The prompt carried one observation; a
coding step needs the last few. `WorkingSet::from_log` (core) derives,
every step, a bounded section from the parent's own log entries: one
line per executed call for the last `working_set.recent_calls` steps
(`step N: tool args -> first result line`, errors and skips marked), then
the last `working_set.max_windows` distinct `read_file`/`search` outputs
verbatim, newest first, each headed `--- tool path lines a-b (step n) ---`.
A window is replaced by a later identical call and dropped once a later
`edit_file`/`write_file` touched its path. This step's own observation is
not repeated. `budgets.working_set` (default 6000) bounds the section:
oldest windows drop whole, then recent-call lines, and a single oversized
window is head-truncated with a marker. It renders after the harness view
and before augmentation. It is not an `Augmenter`: augmentation is
"never load-bearing" and clamped separately; the working set is what the
model just read. `working_set.max_windows: 0` disables it. Nothing is
persisted (C2).

Why: seven runs on one unit produced no edit because the model never had
the failing test and the file to change in one prompt; it re-read what
the harness had dropped and the redundancy rule then refused the re-read.
The rule audit in spec section 10 follows from this: several guards may
have been compensation for a starved prompt.

Edit windows (spec section 10, "Edit windows"): a successful
`edit_file`/`write_file` output is a window on its path, keyed by path,
replacing every earlier window on that path; a read made after the edit
sits beside it. Failed and skipped edits are not windows. Run 24 showed
why: an edit dropped the path's read windows, and the next anchor was
written from memory and missed the file seven times.

## Commit-and-note: `Action::Continue`

A rejection discards the step's patch and spends one of the retries; on
a small model the retry is usually a different wrong action (run 26:
`done`, `done` to review, empty `ask_user`, `done`, cap). Where the patch
is fine and only the action is wrong, a skill's `validate` override may
keep the patch and replace the action with the harness-internal
`Action::Continue { note }`: it is never in the model-facing schema (a
model output naming `"type": "continue"` is rejected at parse), it runs
no tool, spends no commitment, logs a `continued` System entry, and
makes `note` the next observation. The coding skills use it for a
premature `done` in implement (the first open plan step is named, with
the bookkeeping path first: "if that step's work is already done, set
its status to done and run `test`; if not, make the change"; run 54
had landed the edit, sent `done` without marking the step, and obeyed
the earlier "make the change" wording by re-inserting the same arms
seven times) and an empty `ask_user` (all three skills). Two more from
run 59, both keyed on `HarnessState.green_step` (set on every `test`
call: the step on a pass, `None` on a fail; never cleared otherwise, so
it survives the handoff): implement's `done` with the plan complete but
no passing test is a `Continue` ("run `test` now; if it fails, fix what
it reports"), and review's approval (`done: true`, no findings) with no
passing test is a `Continue` ("approval needs a passing unfiltered run;
send it back with the failure as a finding"). Run 59 had implement hand
off straight after a failing `test`, and review run the unfiltered suite,
watch it fail, and approve. And an `ask_user` whose message has no `?`
in it is a `Continue` in all three skills ("a statement of what you will
do next, not a question; do it with a tool call"): of the fifteen
model-written `ask_user` parks in runs 21-68, fourteen were "I need to
fix X. I will first read Y", a plan narrated to a user who had nothing
to answer. The test is the character, not the grammar; a real question
without its `?` gets one steer and, if the model insists three times,
the repeat cap parks it anyway. Review's approval on a filtered pass is
the last of these: `HarnessState.green_filtered` is set beside
`green_step` when the passing `test` output carries the tool's
"filtered out" note, the Harness View shows "(filtered; run the
unfiltered target before approving)", and an approval while it is set
is a `Continue` asking for the unfiltered run (run 67 approved on the
filtered target with the note in front of it; the suite failed on
disk). Implement keeps its green either way. One conversion goes
further than a note: review's blind approval (`done: true`, no
findings, `tool_calls == 0`) is replaced by a `test` call on
`HarnessState.last_test_cmd` (the last `test` command the session ran,
set beside `green_step`; no `cmd` when none), so the model's next
prompt holds a real test result instead of a request to get one. Runs
45-46 and 69-70 showed the look-first note never moves this model; run
71 showed a test observation does. The same mechanism, with the patch left
unchanged, backs the `next` shape rules (`strip_malformed_next`) and
the working set's redundant-call skips: the slot or call is dropped,
the note says why, the step still lands. A conversion is a steer, not a
wall: the same `Continue` note on `CONTINUE_REPEAT_CAP` (3) consecutive
steps parks the session as a question carrying the note (runs 45-46: a
review model sent the same blind approval 35 times), and every
conversion that replaces the action also drops any `next_skill` in the
patch, since the core rule rejects a skill change on a non-`done`
action.

Two tool-side rules from run 27 sit beside it: a repeated identical
`read_file` whose last window was truncated pages forward (args rewritten
to the next offset, a `paged_read` entry, a note prefixed to the window)
instead of being skipped, because twelve skips in a row taught the model
nothing; and `write_file` refuses to replace an existing file over 40
lines, since a model rewriting a file from memory damaged lines it never
meant to touch. `filter_last_tool_call` matches an invalid regex
literally rather than erroring. `edit_file` strips a repeated anchor from
`new` (38 of 102 calls across six runs put the `after` anchor back at
the top of `new`, duplicating the anchored block each time), refuses a
`new` that is only the anchor, and notes when an edit leaves a code
file's braces unbalanced. From runs 38-43: an `after`/`before`
insertion whose text (two or more non-blank lines) is already in the
file is refused with the line range and the words "this edit is
already applied; run `test` to verify instead of inserting it again"
(one run's file grew from 198 to 357 lines by re-inserting the same
arms on every retry, after which every anchor was ambiguous; with the
refusal worded as a plain failure, run 54 retried it seven times; with
"run `test`" in it, run 71 retried it nine times because the block was
there but wrong, so the refusal now quotes the existing lines, numbered
and capped at 40, and says to replace them with old/new); `search` with a `path` and no `pattern`
shows the file's first window instead of failing on the missing field;
`revert_file` (implement only) restores a path to the bytes it had
before the session's first `edit_file`/`write_file` of it, or removes a
file created this session: both write tools take a one-time backup
under `.statie/orig/` before their first modification of a path (never
overwritten, so it is the original; a fresh start clears the directory,
`--resume` keeps it), and the `write_file` refusal points at it. Six
runs had parked on "I need to clean up the mess in `headless.rs`" with
no way back. And when exact and per-line matching both fail, `old` and anchors are
matched with all whitespace collapsed and mapped back to a line range
(the model writes a four-line struct pattern on one line; 13 misses in
one run), with token boundaries respected and ambiguity an error.
Review's send-back is a conversion too: `done: true` with findings
becomes `done: false` plus a handoff to implement, and `next_skill`
on a tool call becomes the handoff when there are findings, or is
dropped with a note when there are none. And a filtered
test is not a verification: the `test` tool notes when a passing cargo
run filtered tests out (run 44: the target test passed, one arm
shadowed another, and the suite failure that showed it was never run),
and review is told to run the unfiltered target before approving. The plan skill's `done` on an empty plan
is a `Continue` too.

## Green checkpoint

Run 28 landed its unit at committed step 94 (the target test passed, then
the whole suite) and then spent sixty steps polishing duplicate arms
until the file no longer parsed. Nothing in the harness had noticed the
green state. `core/src/green.rs` fixes that: when the `test` tool reports
`result: pass`, the session snapshots every file the activation has
edited (paths from successful `edit_file`/`write_file` actions in the
log; files over 1 MiB are listed as skipped), logs `green_checkpoint`,
and prefixes the observation with "verification passed at step N ...
hand off (`done` with `next_skill: review`); cleanup beyond the task is
review's call". The session tracks the most recent `test` result; at
every park site (`ask_user`, `yield`, `done`/handoff, retry-cap failure,
and the driver's autonomous step cap through
`Active::restore_green_if_stale`) a checkpoint whose pass is no longer
the latest result is written back to the sandbox, files created after
it are deleted, `restored_green` is logged, and the park message says
so. The checkpoint lives in memory for the session (C2) and survives a
handoff, since review may send the work back to implement. `shell`
refuses `sed -i`/`perl -i` for the same reason `write_file` refuses
large existing files: both edit from remembered positions, not from
the file's current text.

Two more from runs 32-37. Review must look before it approves: the
harness state counts `tool_calls` per activation, and review's
`done: true` with none is a `Continue` asking it to read the changed
files or run the tests (run 32's review approved a diff with duplicated
arms in one step of 71 tokens). And a transient backend failure is not
a park: `OllamaModel` retries a 5xx or dropped connection after
`model.retry_delay_secs` up to `model.retries_on_5xx` times, aborting
the wait on cancellation, because the runner on the dogfood host
restarts itself within seconds and four of five audit runs had died on
its first crash.

## Milestone 12: rule audit

`Guards` (`core/src/guards.rs`) is one `bool` per non-progress rule the
spec's audit table marks "peel": `redundant_reads` (7.8), `silent_reads`,
`read_guard`, `redundant_verify` (7.7), `done_stays_done`, and
`commitment_read_cap`. All default `true`; `Session::with_guards` and the
CLI's `guards` config section set them. The five session-level rules are
gated at their existing check sites in `core/src/session.rs`; with
`commitment_read_cap` off, a read against a set commitment runs and the
observation gets a one-line "still pending" note instead of a rejection.
`done_stays_done` lives in the coding skills: `plan_rules` takes the flag,
each of `PlanSkill`/`ImplementSkill`/`ReviewSkill` carries it (builder
`with_done_stays_done`), and `coding_skills_configured(delegates, flag)`
builds the set; `CodingRegistry::with_guards` is the CLI's one entry point.
With no `guards` section the behaviour is byte-for-byte what it was.

Run 21, the first with the working set, changed the audit's order before
it started (spec section 10, "Run 21"): the commitment slot could deadlock
on a call the redundancy rules skip, a `shell`/`test` commitment was
compared by `path`, and a non-mutating `next` was a rejection when the
model was using the slot to name its next look. Those three are fixed
ahead of the switch runs, and done-stays-done is peeled first.

The audit itself (runs 38-43 one per switch, runs 51-65 three per
switch; tables in the README field notes) left the defaults as they
are: every switch on except `commitment_read_cap`. Silent reads and the
read guard are the two whose absence showed (no green in six runs);
done-stays-done and redundant-verify are the only candidates to peel,
and only with more runs than the audit had.

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
