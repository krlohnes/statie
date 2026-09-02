//! `SkillRegistry`: the lookup the driver uses to resolve skill names to
//! `Skill` trait objects, plus `RunOptions`.

use std::sync::Arc;

use statie_core::Skill;
use statie_patch::JsonMergePatch;

/// A type-erased skill, boxed behind the workspace's default patch strategy.
pub type DynSkill<W> = Arc<dyn Skill<Workspace = W, Patch = JsonMergePatch<W>>>;

/// A registry of skills addressable by name, shared across the driver and
/// the CLI/TUI wiring.
pub trait SkillRegistry<W>: Send + Sync {
    fn names(&self) -> Vec<String>;
    fn get(&self, name: &str) -> Option<DynSkill<W>>;
}

/// Options controlling a driver run: which skill to start in, any messages
/// to feed in immediately, and the autonomous-step cap.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub initial_skill: String,
    pub initial_messages: Vec<String>,
    pub max_autonomous_steps: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            initial_skill: "toy".to_string(),
            initial_messages: Vec::new(),
            max_autonomous_steps: 200,
        }
    }
}
