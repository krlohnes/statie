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
    /// rule audit"; `guards.seen_map`/`guards.decision_field`; BACKLOG.md
    /// item 2; `guards.tests_field`; BACKLOG.md item 5;
    /// `guards.grounded_findings`; item 12, grounded review findings;
    /// `guards.situational_rules`; unit 26, situational rules).
    /// `Guards::default()` gives exactly [`CodingRegistry::new`].
    pub fn with_guards(guards: Guards) -> Self {
        Self::with_guards_and_listed(guards, true)
    }

    /// [`CodingRegistry::with_guards`] plus `thinker.listed`
    /// (`cli/src/config.rs`; BACKLOG.md item 14): `false` omits `think` from
    /// the tool list the model sees (`PlanSkill`/`ImplementSkill`/
    /// `ReviewSkill::tools()`) without disabling `think` itself -- the
    /// harness's own substitutions (repeated-failure, early-park) still
    /// construct a `think` action directly and are unaffected.
    pub fn with_guards_and_listed(guards: Guards, listed: bool) -> Self {
        CodingRegistry {
            skills: coding_skills_configured(
                vec![],
                guards.done_stays_done,
                guards.seen_map,
                guards.decision_field,
                guards.tests_field,
                guards.grounded_findings,
                listed,
                guards.situational_rules,
            ),
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
