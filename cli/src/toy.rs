//! The `toy` skill set: `ToySkill` plus its `echo` tool. `echo` is not part
//! of `statie-tools::default_tools` (it's test/demo scaffolding, not a
//! coding tool), so it's registered here for the `toy` skill set only.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet};
use statie_skills::{ToySkill, ToyWorkspace};
use statie_tui::{DynSkill, SkillRegistry};

/// Echoes the given text back. Used only by the `toy` skill.
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

/// Builds the `ToolSet` for the `toy` skill: the `echo` tool only.
pub fn toy_tool_set() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(EchoTool));
    tools
}

/// A `SkillRegistry` containing only `toy`.
pub struct ToyRegistry {
    skill: Arc<ToySkill>,
}

impl ToyRegistry {
    pub fn new(target: u64) -> Self {
        ToyRegistry {
            skill: Arc::new(ToySkill::new(target)),
        }
    }
}

impl SkillRegistry<ToyWorkspace> for ToyRegistry {
    fn names(&self) -> Vec<String> {
        vec!["toy".to_string()]
    }

    fn get(&self, name: &str) -> Option<DynSkill<ToyWorkspace>> {
        if name == "toy" {
            Some(self.skill.clone() as DynSkill<ToyWorkspace>)
        } else {
            None
        }
    }
}
