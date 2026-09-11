//! Shared test helpers.

use std::sync::Arc;

use statie_core::{MemoryLog, SandboxRoot, ToolCtx};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
pub fn ctx_for(dir: &std::path::Path) -> ToolCtx {
    ToolCtx {
        root: SandboxRoot::new(dir).unwrap(),
        log: Arc::new(MemoryLog::new()),
        cancel: CancellationToken::new(),
    }
}
