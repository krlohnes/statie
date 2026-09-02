//! `TuiError`: the top-level error type for `run_tui` and `run_headless`.

use statie_core::{LogError, SessionError};

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("unknown skill: {0}")]
    UnknownSkill(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
