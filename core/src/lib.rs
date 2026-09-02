//! statie-core: the binding type contract between statie's crates.
//!
//! This crate defines the type-level invariants described in
//! `statie-spec.md` §3 and `ARCHITECTURE.md`. Three of them are exercised
//! below as compile-fail doctests.
//!
//! ## A1: `Reasoning` is not `Promptable`
//!
//! `Promptable` is sealed; `Reasoning` deliberately does not implement it,
//! so it can never be threaded into a prompt.
//!
//! ```compile_fail
//! use statie_core::{Promptable, Reasoning};
//!
//! fn f<T: Promptable>(_: &T) {}
//! f(&Reasoning::new("x"));
//! ```
//!
//! ## A3: `ValidatedPatch` is not `Clone`
//!
//! A validated patch may commit at most once; it is deliberately `!Clone`.
//!
//! ```compile_fail
//! use statie_core::ValidatedPatch;
//!
//! fn assert_clone<T: Clone>() {}
//! assert_clone::<ValidatedPatch<()>>();
//! ```
//!
//! ## A9: `SandboxPath` has no public constructor
//!
//! Tool APIs take `SandboxPath`, never a bare `Path`/`PathBuf`; the only way
//! to obtain one is `SandboxRoot::join`.
//!
//! ```compile_fail
//! use std::path::PathBuf;
//! use statie_core::SandboxPath;
//!
//! let _p = SandboxPath(PathBuf::from("/tmp/x"));
//! ```

mod augmenter;
mod bm25;
mod error;
mod log;
mod loop_types;
mod model;
mod patch;
mod prompt;
mod promptable;
mod sandbox;
mod session;
mod skill;
mod store;
mod tokenizer;
mod tool;
mod version;

pub use augmenter::Augmenter;
pub use bm25::{Bm25Index, tokenize};
pub use error::{
    LogError, ModelError, ParseError, PatchError, SandboxError, SessionError, StoreError,
    ToolError,
};
pub use log::{LogEntry, LogKind, MemoryLog, SessionLog, SessionLogExt, StepRecord};
pub use loop_types::{Event, Executable, Outcome, Park};
pub use model::{
    Action, Capabilities, Model, NullSink, RawOutput, Schema, StepOutput, StepSink, Usage,
};
pub use patch::{
    CheckedPatch, InboxAcked, PatchStrategy, RawPatch, SchemaChecked, ValidatedPatch, WithinBudget,
};
pub use prompt::{
    AgentState, Augmentation, BoundedPrompt, HarnessView, OverBudget, PromptBuilder, Section,
    Snippet, SystemPrompt,
};
pub use promptable::{Observation, Promptable, Reasoning, ToolOutputFull};
pub use sandbox::{SandboxPath, SandboxRoot};
pub use session::{Active, CancelHandle, Session, SessionConfig};
pub use skill::{ParsedOutput, Skill, SkillRules, SkillSpec, ValidatedStep, check_inbox, validate_default};
pub use store::{ApplyFn, LoggedPatch, StateStore, apply_fn};
pub use tokenizer::{Budgets, HeuristicTokenizer, TiktokenTokenizer, TokenBudget, Tokenizer};
pub use tool::{Tool, ToolCtx, ToolList, ToolSet};
pub use version::{Committed, CommittedState, HarnessState, InboxItem, Version};
