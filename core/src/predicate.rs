//! A small predicate language for configured children (milestone 9,
//! ARCHITECTURE.md "Rules over reports"). Parent rules stay Rust, but a
//! configured child gates itself with a `rules` stanza in its config:
//!
//! ```json
//! "rules": [
//!   { "when": { "action": "done" },
//!     "require": { "pointer": "/reports/review/outcome", "eq": "done" },
//!     "message": "run delegate(review) and get a clean report before done" }
//! ]
//! ```
//!
//! Deliberately not a general expression language: one pointer, a fixed set
//! of predicates, all of which must hold for the rule's `require` to pass.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One gate: when it applies, `require` must hold or the action is
/// rejected with `message` (or a generated message).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rule {
    /// When this rule applies. Omitted or default means it applies to
    /// every action.
    #[serde(default)]
    pub when: When,
    /// The predicate that must hold once this rule applies.
    pub require: Require,
    /// Shown to the model verbatim in place of the generated message.
    #[serde(default)]
    pub message: Option<String>,
}

/// Restricts which action a [`Rule`] applies to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct When {
    /// The rule applies only when the action type equals this. Omitted
    /// means the rule applies to every action.
    #[serde(default)]
    pub action: Option<String>,
}

/// A predicate over the JSON value at `pointer` in state. All fields set
/// (non-`None`) must pass for the predicate to hold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Require {
    /// RFC 6901 JSON pointer into state, e.g. `/reports/review/outcome`.
    /// A pointer that resolves to nothing is treated as an absent value
    /// for every predicate below.
    pub pointer: String,
    /// The value at `pointer` must equal this.
    #[serde(default)]
    pub eq: Option<Value>,
    /// The value at `pointer` must not equal this.
    #[serde(default)]
    pub ne: Option<Value>,
    /// The value at `pointer` must (or must not) be present.
    #[serde(default)]
    pub exists: Option<bool>,
    /// The value at `pointer` must (or must not) be empty. Applies to
    /// arrays, strings, objects, and null (null counts as empty); a bool
    /// or number is neither empty nor non-empty and fails either check.
    #[serde(default)]
    pub empty: Option<bool>,
    /// The value at `pointer` must (or must not) be non-empty. See `empty`
    /// for what counts as empty.
    #[serde(default)]
    pub non_empty: Option<bool>,
    /// The value at `pointer` must be a number greater than this.
    #[serde(default)]
    pub gt: Option<f64>,
    /// The value at `pointer` must be a number less than this.
    #[serde(default)]
    pub lt: Option<f64>,
}

/// Whether a JSON value counts as empty, for `empty`/`non_empty`. Bool and
/// number have no notion of emptiness.
fn emptiness(val: Option<&Value>) -> Option<bool> {
    match val {
        None => Some(true),
        Some(Value::Null) => Some(true),
        Some(Value::String(s)) => Some(s.is_empty()),
        Some(Value::Array(a)) => Some(a.is_empty()),
        Some(Value::Object(o)) => Some(o.is_empty()),
        Some(Value::Bool(_)) | Some(Value::Number(_)) => None,
    }
}

/// Renders a JSON value for a message, eliding anything over 80 chars.
fn render(val: &Value) -> String {
    let s = serde_json::to_string(val).unwrap_or_else(|_| "?".to_string());
    if s.chars().count() > 80 {
        let truncated: String = s.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

/// Renders an absent value's placeholder the same way a present one would
/// be described in a message.
fn render_opt(val: Option<&Value>) -> String {
    match val {
        Some(v) => render(v),
        None => "absent".to_string(),
    }
}

/// Evaluates one [`Require`] against `state`, returning `Err(detail)` for
/// the first predicate that fails.
fn evaluate_require(req: &Require, state: &Value) -> Result<(), String> {
    let val = state.pointer(&req.pointer);

    if let Some(expected) = req.exists {
        let exists = val.is_some();
        if exists != expected {
            return Err(if expected {
                "must exist".to_string()
            } else {
                "must not exist".to_string()
            });
        }
    }

    if let Some(ref expected) = req.eq {
        let matches = val == Some(expected);
        if !matches {
            return Err(format!(
                "must equal {} (was {})",
                render(expected),
                render_opt(val)
            ));
        }
    }

    if let Some(ref forbidden) = req.ne {
        let matches = val == Some(forbidden);
        if matches {
            return Err(format!("must not equal {}", render(forbidden)));
        }
    }

    if let Some(expected) = req.empty {
        match emptiness(val) {
            Some(is_empty) if is_empty == expected => {}
            Some(_) => {
                return Err(if expected {
                    format!("must be empty (was {})", render_opt(val))
                } else {
                    "must be non-empty".to_string()
                });
            }
            None => {
                return Err(format!(
                    "must be empty (was {}, which has no emptiness)",
                    render_opt(val)
                ));
            }
        }
    }

    if let Some(expected) = req.non_empty {
        match emptiness(val) {
            Some(is_empty) if is_empty != expected => {}
            Some(_) => {
                return Err(if expected {
                    "must be non-empty".to_string()
                } else {
                    format!("must be empty (was {})", render_opt(val))
                });
            }
            None => {
                return Err(format!(
                    "must be non-empty (was {}, which has no emptiness)",
                    render_opt(val)
                ));
            }
        }
    }

    if let Some(gt) = req.gt {
        match val.and_then(Value::as_f64) {
            Some(v) if v > gt => {}
            Some(v) => return Err(format!("must be > {gt} (was {v})")),
            None => return Err(format!("must be a number (was {})", render_opt(val))),
        }
    }

    if let Some(lt) = req.lt {
        match val.and_then(Value::as_f64) {
            Some(v) if v < lt => {}
            Some(v) => return Err(format!("must be < {lt} (was {v})")),
            None => return Err(format!("must be a number (was {})", render_opt(val))),
        }
    }

    Ok(())
}

/// Evaluates `rules` in order against `action_type` and `state`. The first
/// applying rule (`when.action` is `None` or equals `action_type`) whose
/// `require` fails returns `Err` with the rule's own `message` if set, else
/// a generated `"rule violated: {pointer} {detail}"`. `Ok(())` if every
/// applying rule passes.
pub fn evaluate(rules: &[Rule], action_type: &str, state: &Value) -> Result<(), String> {
    for rule in rules {
        let applies = match &rule.when.action {
            Some(expected) => expected == action_type,
            None => true,
        };
        if !applies {
            continue;
        }

        if let Err(detail) = evaluate_require(&rule.require, state) {
            let msg = rule
                .message
                .clone()
                .unwrap_or_else(|| format!("rule violated: {} {}", rule.require.pointer, detail));
            return Err(msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(pointer: &str) -> Require {
        Require {
            pointer: pointer.to_string(),
            eq: None,
            ne: None,
            exists: None,
            empty: None,
            non_empty: None,
            gt: None,
            lt: None,
        }
    }

    fn rule(when: When, require: Require, message: Option<&str>) -> Rule {
        Rule {
            when,
            require,
            message: message.map(str::to_string),
        }
    }

    // -- eq --

    #[test]
    fn eq_pass_and_fail() {
        let rules = vec![rule(
            When::default(),
            Require {
                eq: Some(json!("done")),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&rules, "x", &json!({ "a": "done" })).is_ok());
        assert!(evaluate(&rules, "x", &json!({ "a": "blocked" })).is_err());
    }

    #[test]
    fn eq_generated_message_exact_text() {
        let rules = vec![rule(
            When::default(),
            Require {
                eq: Some(json!("done")),
                ..req("/reports/review/outcome")
            },
            None,
        )];
        let err = evaluate(
            &rules,
            "x",
            &json!({ "reports": { "review": { "outcome": "blocked" } } }),
        )
        .unwrap_err();
        assert_eq!(
            err,
            r#"rule violated: /reports/review/outcome must equal "done" (was "blocked")"#
        );
    }

    #[test]
    fn eq_absent_pointer_fails() {
        let rules = vec![rule(
            When::default(),
            Require {
                eq: Some(json!(1)),
                ..req("/missing")
            },
            None,
        )];
        let err = evaluate(&rules, "x", &json!({})).unwrap_err();
        assert_eq!(err, "rule violated: /missing must equal 1 (was absent)");
    }

    // -- ne --

    #[test]
    fn ne_pass_and_fail() {
        let rules = vec![rule(
            When::default(),
            Require {
                ne: Some(json!(2)),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&rules, "x", &json!({ "a": 1 })).is_ok());
        assert!(evaluate(&rules, "x", &json!({ "a": 2 })).is_err());
    }

    #[test]
    fn ne_absent_pointer_passes() {
        let rules = vec![rule(
            When::default(),
            Require {
                ne: Some(json!(2)),
                ..req("/missing")
            },
            None,
        )];
        assert!(evaluate(&rules, "x", &json!({})).is_ok());
    }

    // -- exists --

    #[test]
    fn exists_true_and_false() {
        let exists_true = vec![rule(
            When::default(),
            Require {
                exists: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&exists_true, "x", &json!({ "a": 1 })).is_ok());
        assert!(evaluate(&exists_true, "x", &json!({})).is_err());

        let exists_false = vec![rule(
            When::default(),
            Require {
                exists: Some(false),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&exists_false, "x", &json!({})).is_ok());
        assert!(evaluate(&exists_false, "x", &json!({ "a": 1 })).is_err());
    }

    // -- empty / non_empty on collections and null --

    #[test]
    fn empty_and_non_empty_on_array_string_object_null() {
        let empty_true = vec![rule(
            When::default(),
            Require {
                empty: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&empty_true, "x", &json!({ "a": [] })).is_ok());
        assert!(evaluate(&empty_true, "x", &json!({ "a": [1] })).is_err());
        assert!(evaluate(&empty_true, "x", &json!({ "a": "" })).is_ok());
        assert!(evaluate(&empty_true, "x", &json!({ "a": {} })).is_ok());
        assert!(evaluate(&empty_true, "x", &json!({ "a": null })).is_ok());

        let non_empty_true = vec![rule(
            When::default(),
            Require {
                non_empty: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&non_empty_true, "x", &json!({ "a": "hi" })).is_ok());
        assert!(evaluate(&non_empty_true, "x", &json!({ "a": "" })).is_err());
    }

    #[test]
    fn empty_absent_pointer_passes_non_empty_absent_fails() {
        let empty_true = vec![rule(
            When::default(),
            Require {
                empty: Some(true),
                ..req("/missing")
            },
            None,
        )];
        assert!(evaluate(&empty_true, "x", &json!({})).is_ok());

        let non_empty_true = vec![rule(
            When::default(),
            Require {
                non_empty: Some(true),
                ..req("/missing")
            },
            None,
        )];
        assert!(evaluate(&non_empty_true, "x", &json!({})).is_err());
    }

    #[test]
    fn bool_and_number_have_no_emptiness() {
        let empty_true = vec![rule(
            When::default(),
            Require {
                empty: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&empty_true, "x", &json!({ "a": false })).is_err());
        assert!(evaluate(&empty_true, "x", &json!({ "a": 0 })).is_err());

        let non_empty_true = vec![rule(
            When::default(),
            Require {
                non_empty: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&non_empty_true, "x", &json!({ "a": true })).is_err());
        assert!(evaluate(&non_empty_true, "x", &json!({ "a": 5 })).is_err());
    }

    // -- gt / lt --

    #[test]
    fn gt_lt_pass_and_fail() {
        let gt = vec![rule(
            When::default(),
            Require {
                gt: Some(10.0),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&gt, "x", &json!({ "a": 15.0 })).is_ok());
        assert!(evaluate(&gt, "x", &json!({ "a": 5.0 })).is_err());

        let lt = vec![rule(
            When::default(),
            Require {
                lt: Some(10.0),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&lt, "x", &json!({ "a": 5.0 })).is_ok());
        assert!(evaluate(&lt, "x", &json!({ "a": 15.0 })).is_err());
    }

    #[test]
    fn gt_on_non_number_and_absent_fails() {
        let gt = vec![rule(
            When::default(),
            Require {
                gt: Some(10.0),
                ..req("/a")
            },
            None,
        )];
        let err = evaluate(&gt, "x", &json!({ "a": "abc" })).unwrap_err();
        assert_eq!(err, r#"rule violated: /a must be a number (was "abc")"#);

        let err_absent = evaluate(&gt, "x", &json!({})).unwrap_err();
        assert_eq!(
            err_absent,
            "rule violated: /a must be a number (was absent)"
        );
    }

    // -- when filtering --

    #[test]
    fn when_action_filters_rule() {
        let rules = vec![rule(
            When {
                action: Some("done".to_string()),
            },
            Require {
                eq: Some(json!(1)),
                ..req("/a")
            },
            None,
        )];
        let state = json!({ "a": 2 });
        assert!(evaluate(&rules, "other", &state).is_ok());
        assert!(evaluate(&rules, "done", &state).is_err());
    }

    // -- ordering / first failure wins --

    #[test]
    fn first_failing_rule_wins() {
        let rules = vec![
            rule(
                When::default(),
                Require {
                    eq: Some(json!(1)),
                    ..req("/a")
                },
                Some("first fail"),
            ),
            rule(
                When::default(),
                Require {
                    eq: Some(json!(2)),
                    ..req("/a")
                },
                Some("second fail"),
            ),
        ];
        let err = evaluate(&rules, "x", &json!({ "a": 3 })).unwrap_err();
        assert_eq!(err, "first fail");
    }

    // -- custom message wins --

    #[test]
    fn custom_message_wins_over_generated() {
        let rules = vec![rule(
            When::default(),
            Require {
                eq: Some(json!(1)),
                ..req("/a")
            },
            Some("custom message"),
        )];
        let err = evaluate(&rules, "x", &json!({ "a": 2 })).unwrap_err();
        assert_eq!(err, "custom message");
    }

    // -- multiple predicates in one require, all must hold --

    #[test]
    fn all_predicates_in_one_require_must_hold() {
        let rules = vec![rule(
            When::default(),
            Require {
                eq: Some(json!(1)),
                exists: Some(true),
                ..req("/a")
            },
            None,
        )];
        assert!(evaluate(&rules, "x", &json!({ "a": 1 })).is_ok());
        assert!(evaluate(&rules, "x", &json!({ "b": 1 })).is_err());
    }

    // -- serde round trip --

    #[test]
    fn rule_serde_round_trip() {
        let r = rule(
            When {
                action: Some("done".to_string()),
            },
            Require {
                eq: Some(json!("ok")),
                gt: Some(3.0),
                ..req("/x")
            },
            Some("go fix it"),
        );
        let s = serde_json::to_string(&r).unwrap();
        let back: Rule = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn deserializes_minimal_config_form() {
        let s = r#"{"require": {"pointer": "/x", "eq": 1}}"#;
        let r: Rule = serde_json::from_str(s).unwrap();
        assert_eq!(r.when, When::default());
        assert_eq!(r.message, None);
        assert_eq!(r.require.pointer, "/x");
        assert_eq!(r.require.eq, Some(json!(1)));
        assert_eq!(r.require.ne, None);
        assert_eq!(r.require.gt, None);
    }
}
