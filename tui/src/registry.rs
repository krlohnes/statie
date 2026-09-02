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
/// to feed in immediately, the autonomous-step cap, and a startup banner.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub initial_skill: String,
    pub initial_messages: Vec<String>,
    pub max_autonomous_steps: usize,
    /// Lines sent as `UiEvent::Info` before the first activation (config
    /// path, model backend, sandbox root, a usage hint).
    pub banner: Vec<String>,
    /// When true, and the session already has committed state, the driver
    /// replays recent log history before the first activation (and, unless
    /// `skill_explicit` is set, starts in the stored `harness.activation`
    /// rather than `initial_skill`), so a resumed run looks and behaves like
    /// a resume instead of a fresh start.
    pub resume: bool,
    /// How many of the most recent log entries to replay on resume.
    pub replay_entries: usize,
    /// True when `initial_skill` came from an explicit user choice (e.g.
    /// `--skill` on the CLI) rather than a default fallback. When true, the
    /// driver never overrides `initial_skill` with the stored activation on
    /// resume, even though the replay still happens.
    pub skill_explicit: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            initial_skill: "toy".to_string(),
            initial_messages: Vec::new(),
            max_autonomous_steps: 200,
            banner: Vec::new(),
            resume: false,
            replay_entries: 40,
            skill_explicit: false,
        }
    }
}
