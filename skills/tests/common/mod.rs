//! Shared test helpers for the session-loop integration tests
//! (`toy_loop.rs`, `recovery.rs`): an `EchoTool`, `ToolSet`/`SessionConfig`
//! builders, and an `ArcModel` wrapper so tests can keep an `Arc<MockModel>`
//! handle after the model has been moved into a `Session`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, MemoryLog, RawOutput, SandboxRoot, Schema, Session, SessionConfig,
    SessionLog, StepSink, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet,
};
use statie_models::MockModel;
use statie_skills::ToyWorkspace;
use statie_store::InMemoryStore;

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the given text back."
    }

    fn args_schema(&self) -> Schema {
        #[derive(schemars::JsonSchema)]
        struct Args {
            #[allow(dead_code)]
            text: String,
        }
        Schema::of::<Args>()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("missing `text`".to_string()))?;
        Ok(ToolOutputFull::new(format!("echo: {text}")))
    }
}

pub fn tool_set() -> ToolSet {
    let mut ts = ToolSet::new();
    ts.register(Arc::new(EchoTool));
    ts
}

pub fn config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

pub fn config_with_retries(root: &tempfile::TempDir, retries: u32) -> SessionConfig {
    let mut c = config(root);
    c.retries = retries;
    c
}

/// A `Model` wrapper around `Arc<MockModel>` so tests can keep an `Arc`
/// handle to introspect `prompts()`/`calls()` after the model has been
/// moved (boxed) into a `Session`.
pub struct ArcModel(pub Arc<MockModel>);

#[async_trait::async_trait]
impl statie_core::Model for ArcModel {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.0.step(prompt, schema, sink, cancel).await
    }
}

pub fn make_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<ToyWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = Session::new(
        Box::new(InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Box::new(ArcModel(model)),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}
