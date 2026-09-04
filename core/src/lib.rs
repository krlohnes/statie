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
mod delegate;
mod error;
mod green;
mod guards;
mod log;
mod loop_types;
mod maintainer;
mod model;
mod patch;
mod predicate;
mod prompt;
mod promptable;
mod read_window;
mod report;
mod sandbox;
mod session;
mod skill;
mod store;
mod tokenizer;
mod tool;
mod version;
mod working_set;

pub use augmenter::Augmenter;
pub use bm25::{Bm25Index, tokenize};
pub use delegate::{CancelToken, ChildRequest, ChildRunner, InboxItem as ChildInboxItem};
pub use error::{
    LogError, ModelError, ParseError, PatchError, SandboxError, SessionError, StoreError, ToolError,
};
pub use green::{
    CheckpointFile, GREEN_CHECKPOINT_FILE_CAP, GreenCheckpoint, Snapshot, edited_paths, is_pass,
    restore, snapshot,
};
pub use guards::Guards;
pub use log::{
    LogEntry, LogKind, MemoryLog, SessionLog, SessionLogExt, StepRecord, TaggedLog, read_jsonl,
};
pub use loop_types::{Event, Executable, Outcome, Park};
pub use maintainer::{
    MaintainCall, MaintainError, MaintainInput, MaintainOutput, Maintainer, MaintainerReply,
    MockMaintainer, build_maintainer_prompt, maintainer_output_schema,
};
pub use model::{
    Action, Capabilities, Model, NullSink, RawOutput, Schema, StepOutput, StepSink, Usage,
    restrict_action_types,
};
pub use patch::{
    CheckedPatch, InboxAcked, PatchStrategy, RawPatch, SchemaChecked, ValidatedPatch, WithinBudget,
};
pub use predicate::{Require, Rule, When, evaluate};
pub use prompt::{
    AgentState, Augmentation, BoundedPrompt, HarnessView, OverBudget, PromptBuilder, Section,
    Snippet, SystemPrompt,
};
pub use promptable::{Notes, Observation, Promptable, Reasoning, ToolOutputFull};
pub use read_window::{ReadWindowHeader, parse_read_header};
pub use report::{FileNote, Report, ReportOutcome, ReportStats, ReportWorkspace};
pub use sandbox::{SandboxPath, SandboxRoot};
pub use session::{Active, CancelHandle, Session, SessionConfig};
pub use skill::{
    Commitment, ParsedOutput, Patch, Skill, SkillRules, SkillSpec, ValidatedStep, check_inbox,
    validate_default,
};
pub use store::{ApplyFn, LoggedPatch, StateStore, apply_fn};
pub use tokenizer::{Budgets, HeuristicTokenizer, TiktokenTokenizer, TokenBudget, Tokenizer};
pub use tool::{Tool, ToolCtx, ToolList, ToolSet};
pub use version::{Committed, CommittedState, HarnessState, InboxItem, Pinned, Version};
pub use working_set::{WorkingSet, WorkingSetConfig};
