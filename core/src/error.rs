//! Error types.

use crate::version::Version;

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("patch did not match the expected shape: {0}")]
    Malformed(String),
    #[error("patch application failed: {0}")]
    ApplyFailed(String),
    #[error("skill rule violated: {0}")]
    RuleViolation(String),
    #[error("cannot park with unacked inbox items: {0:?}")]
    UnackedInbox(Vec<u64>),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid JSON output: {0}")]
    InvalidJson(String),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("version mismatch: expected {expected:?}, actual {actual:?}")]
    VersionMismatch { expected: Version, actual: Version },
    #[error("store backend error: {0}")]
    Backend(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serde(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("log backend error: {0}")]
    Backend(String),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model request was cancelled")]
    Cancelled,
    #[error("model backend error: {0}")]
    Backend(String),
    #[error("model returned invalid output: {0}")]
    InvalidOutput(String),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArgs(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("path is absolute: {0}")]
    AbsolutePath(String),
    #[error("path escapes the sandbox root via `..`: {0}")]
    ParentEscape(String),
    #[error("resolved path escapes the sandbox root: {0}")]
    OutsideRoot(String),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    OverBudget(#[from] crate::prompt::OverBudget),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("transition from {from:?} to {to} is not allowed")]
    TransitionNotAllowed { from: Option<String>, to: String },
    #[error("no activation is currently active")]
    NoActivation,
    #[error("io error: {0}")]
    Io(String),
}
