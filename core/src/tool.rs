//! Tools: sandboxed, schema-described, invoked via `ToolCtx`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::log::SessionLog;
use crate::model::Schema;
use crate::promptable::{Promptable, ToolOutputFull, sealed};
use crate::sandbox::SandboxRoot;

pub struct ToolCtx {
    pub root: SandboxRoot,
    pub log: Arc<dyn SessionLog>,
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn args_schema(&self) -> Schema;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError>;
}

/// A registry of available tools.
#[derive(Default, Clone)]
pub struct ToolSet {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolSet {
    pub fn new() -> Self {
        ToolSet {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// A `ToolList` (for the prompt) restricted to the given names. Unknown
    /// names are silently skipped.
    pub fn list_for(&self, names: &[String]) -> ToolList {
        let entries = names
            .iter()
            .filter_map(|n| self.tools.get(n))
            .map(|t| ToolListEntry {
                name: t.name().to_string(),
                description: t.description().to_string(),
                args_schema: t.args_schema(),
            })
            .collect();
        ToolList(entries)
    }
}

struct ToolListEntry {
    name: String,
    description: String,
    args_schema: Schema,
}

/// Renders name, description, and compact-JSON args schema per tool.
pub struct ToolList(Vec<ToolListEntry>);

impl sealed::Sealed for ToolList {}
impl Promptable for ToolList {
    fn render(&self) -> String {
        if self.0.is_empty() {
            return "(no tools available)".to_string();
        }
        self.0
            .iter()
            .map(|t| {
                let schema_str =
                    serde_json::to_string(t.args_schema.as_value()).unwrap_or_else(|_| "{}".to_string());
                format!("- {}: {}\n  args: {}", t.name, t.description, schema_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
