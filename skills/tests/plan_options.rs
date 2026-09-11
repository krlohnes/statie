//! Unit 11 ("options-and-verdict mode for `think`"): the plan skill's rules
//! text (`PlanSkill::spec().instructions()`, `skills/src/coding/mod.rs`)
//! must tell the model to call `think` with `mode: "options"` before
//! writing the plan for a design fork; the implement skill's rules text
//! must not carry that sentence.

use statie_core::Skill;
use statie_skills::{ImplementSkill, PlanSkill};

/// The exact sentence Unit 11's spec requires in the plan skill's rules
/// text.
const OPTIONS_MODE_SENTENCE: &str = "For a design fork with more than one reasonable approach, call `think` with `mode: \"options\"` before writing the plan.";

#[test]
fn plan_rules_text_contains_the_options_mode_sentence() {
    let skill = PlanSkill::new();
    let instructions = skill.spec().instructions().to_string();
    assert!(
        instructions.contains(OPTIONS_MODE_SENTENCE),
        "expected the plan skill's rules text to contain the options-mode sentence; \
         instructions were:\n{instructions}"
    );
}

#[test]
fn implement_rules_text_does_not_contain_the_options_mode_sentence() {
    let skill = ImplementSkill::new();
    let instructions = skill.spec().instructions().to_string();
    assert!(
        !instructions.contains(OPTIONS_MODE_SENTENCE),
        "the implement skill's rules text must NOT contain the options-mode sentence \
         (only plan gets it); instructions were:\n{instructions}"
    );
}
