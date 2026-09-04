//! The `coding` skill set: `router`/`plan`/`implement`/`review`, sharing
//! `statie_skills::CodingWorkspace` (milestones 5 & 6, statie-spec.md §10).

use std::sync::Arc;

use statie_core::Guards;
use statie_skills::{CodingWorkspace, coding_skills, coding_skills_configured};
use statie_tui::{DynSkill, SkillRegistry};

/// A `SkillRegistry` over `statie_skills::coding_skills()`: `router`,
/// `plan`, `implement`, `review`.
pub struct CodingRegistry {
    skills: Vec<(String, DynSkill<CodingWorkspace>)>,
}

impl CodingRegistry {
    pub fn new() -> Self {
        CodingRegistry {
            skills: coding_skills(),
        }
    }

    /// The four coding skills with the rule-audit switches applied
    /// (`guards.done_stays_done`; statie-spec.md "10. After it works: the
    /// rule audit"). `Guards::default()` gives exactly [`CodingRegistry::new`].
    pub fn with_guards(guards: Guards) -> Self {
        CodingRegistry {
            skills: coding_skills_configured(vec![], guards.done_stays_done),
        }
    }
}

impl Default for CodingRegistry {
    fn default() -> Self {
        CodingRegistry::new()
    }
}

impl SkillRegistry<CodingWorkspace> for CodingRegistry {
    fn names(&self) -> Vec<String> {
        self.skills.iter().map(|(name, _)| name.clone()).collect()
    }

    fn get(&self, name: &str) -> Option<DynSkill<CodingWorkspace>> {
        self.skills
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, skill)| Arc::clone(skill))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_the_four_coding_skills() {
        let registry = CodingRegistry::new();
        let mut names = registry.names();
        names.sort();
        assert_eq!(names, vec!["implement", "plan", "review", "router"]);
    }

    #[test]
    fn get_resolves_each_known_name_and_none_for_unknown() {
        let registry = CodingRegistry::new();
        for name in ["router", "plan", "implement", "review"] {
            assert!(registry.get(name).is_some(), "expected `{name}` to resolve");
        }
        assert!(registry.get("bogus").is_none());
    }
}
