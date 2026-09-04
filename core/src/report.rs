//! The one shape a subagent (child) returns to its parent (milestone 9,
//! ARCHITECTURE.md "Report: the one shape a child returns"). `Report` is
//! harness-authored: the model never constructs one directly. A configured
//! child's workspace type is [`ReportWorkspace`] -- `Report` minus the
//! harness-filled fields (`agent`, `task`, `stats`) plus `recent_summary`
//! and `done`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A file the task has touched or judged relevant, with a one-line note
/// explaining why (e.g. "contains the parser to modify", "existing test to
/// extend"). Shared by the coding workspace and [`Report`]/[`ReportWorkspace`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileNote {
    /// Path to the file, relative to the sandbox root.
    pub path: String,
    /// One line: why this file matters to the task. Defaults to empty: the
    /// merge-patch schema strips `required` inside the patch, so a
    /// schema-constrained model may legitimately send `{"path": ...}`
    /// alone, and a missing note must not reject the whole patch.
    #[serde(default)]
    pub note: String,
}

/// How a child activation ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutcome {
    /// The child finished its task successfully.
    #[default]
    Done,
    /// The child could not proceed without input only the parent (or user)
    /// can give; see `summary`/`data.reason` for why.
    Blocked,
    /// The child hit an unrecoverable error.
    Failed,
    /// The child ran out of steps, budget, or was cancelled before it could
    /// reach a terminal outcome; `summary` reflects whatever partial
    /// progress it had recorded.
    Capped,
}

/// Server-reported token usage and step accounting for one child run,
/// summed across every step it took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReportStats {
    /// Number of committed steps the child took.
    pub steps: u64,
    /// Sum of server-reported prompt tokens across the child's steps.
    pub server_prompt_tokens: u64,
    /// Sum of server-reported completion tokens across the child's steps.
    pub server_completion_tokens: u64,
    /// Wall-clock time the child ran for, in milliseconds.
    pub wall_ms: u64,
}

/// The one shape a child returns to its parent. Deliberately general:
/// `findings` and `files` are first-class because the coding rules and the
/// TUI already understand them; everything else specific to a child's job
/// goes in `data`, checked against that child's declared JSON schema.
///
/// Written by the harness (via [`Skill::absorb_report`](crate::Skill::absorb_report)),
/// never by the model.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    /// The child skill's name.
    pub agent: String,
    /// The parent's delegation text, verbatim.
    pub task: String,
    /// How the child's activation ended.
    pub outcome: ReportOutcome,
    /// At most 1200 characters. Also becomes the parent's pending
    /// observation after delegation.
    pub summary: String,
    /// Concrete findings; empty means nothing to fix.
    pub findings: Vec<String>,
    /// Files touched or judged relevant by the child.
    pub files: Vec<FileNote>,
    /// Skill-specific data, checked against the child's declared JSON
    /// schema.
    pub data: serde_json::Map<String, serde_json::Value>,
    /// Steps, server prompt/completion tokens, and wall-clock time the
    /// child spent producing this report.
    pub stats: ReportStats,
}

/// The child's workspace type. A configured child runs as
/// `Session<ReportWorkspace>`: this is `Report` minus the harness-filled
/// fields (`agent`, `task`, `stats`) plus `recent_summary` and `done`, the
/// two fields every workspace needs for cross-step memory and termination.
///
/// Rules (enforced by the child skill, not encoded in this type): `done`
/// requires a non-empty `summary`; `outcome != Done` requires non-empty
/// `findings` or a `data.reason`; `data` must validate against the child's
/// declared JSON schema.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReportWorkspace {
    /// How the child's activation ended so far.
    #[serde(default)]
    pub outcome: ReportOutcome,
    /// At most 1200 characters. Also becomes the parent's pending
    /// observation after delegation.
    #[serde(default)]
    pub summary: String,
    /// Concrete findings; empty means nothing to fix.
    #[serde(default)]
    pub findings: Vec<String>,
    /// Files touched or judged relevant by the child.
    #[serde(default)]
    pub files: Vec<FileNote>,
    /// Skill-specific data, checked against the child's declared JSON
    /// schema.
    #[serde(default)]
    pub data: serde_json::Map<String, serde_json::Value>,
    /// One short paragraph maintained by the child describing what it did
    /// last -- the only memory of recent activity across the child's own
    /// steps.
    #[serde(default)]
    pub recent_summary: String,
    /// True once the child considers its task complete and `summary` is
    /// ready to hand back to the parent.
    #[serde(default)]
    pub done: bool,
}

impl Report {
    /// Builds the harness-authored `Report` from a finished child's
    /// workspace, filling in the fields the child's workspace doesn't
    /// carry (`agent`, `task`, `stats`).
    pub fn from_workspace(
        agent: impl Into<String>,
        task: impl Into<String>,
        ws: &ReportWorkspace,
        stats: ReportStats,
    ) -> Report {
        Report {
            agent: agent.into(),
            task: task.into(),
            outcome: ws.outcome,
            summary: ws.summary.clone(),
            findings: ws.findings.clone(),
            files: ws.files.clone(),
            data: ws.data.clone(),
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> Report {
        Report {
            agent: "ui-review".to_string(),
            task: "check the login page".to_string(),
            outcome: ReportOutcome::Done,
            summary: "renders fine".to_string(),
            findings: vec!["button too small".to_string()],
            files: vec![FileNote {
                path: "src/login.rs".to_string(),
                note: "renders the login form".to_string(),
            }],
            data: serde_json::json!({"renders": true, "issues": ["button too small"]})
                .as_object()
                .cloned()
                .unwrap(),
            stats: ReportStats {
                steps: 12,
                server_prompt_tokens: 3100,
                server_completion_tokens: 400,
                wall_ms: 45_000,
            },
        }
    }

    #[test]
    fn report_serde_round_trips() {
        let report = sample_report();
        let json = serde_json::to_string(&report).expect("serialize");
        let back: Report = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.agent, report.agent);
        assert_eq!(back.task, report.task);
        assert_eq!(back.outcome, report.outcome);
        assert_eq!(back.summary, report.summary);
        assert_eq!(back.findings, report.findings);
        assert_eq!(back.files, report.files);
        assert_eq!(back.data, report.data);
        assert_eq!(back.stats, report.stats);
    }

    #[test]
    fn report_outcome_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReportOutcome::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::to_string(&ReportOutcome::Capped).unwrap(),
            "\"capped\""
        );
    }

    #[test]
    fn report_workspace_default_schema_has_seven_fields() {
        let schema = crate::model::Schema::of::<ReportWorkspace>();
        let value = schema.as_value();
        let props = value
            .get("properties")
            .expect("schema should have properties")
            .as_object()
            .expect("properties should be an object");
        for field in [
            "outcome",
            "summary",
            "findings",
            "files",
            "data",
            "recent_summary",
            "done",
        ] {
            assert!(props.contains_key(field), "missing field {field}");
        }
        assert_eq!(
            props.len(),
            7,
            "expected exactly seven fields, got {props:?}"
        );
    }

    #[test]
    fn from_workspace_copies_fields() {
        let ws = ReportWorkspace {
            outcome: ReportOutcome::Blocked,
            summary: "need more info".to_string(),
            findings: vec!["missing config".to_string()],
            files: vec![FileNote {
                path: "a.rs".to_string(),
                note: "relevant".to_string(),
            }],
            data: serde_json::json!({"reason": "no access"})
                .as_object()
                .cloned()
                .unwrap(),
            recent_summary: "looked around".to_string(),
            done: true,
        };
        let stats = ReportStats {
            steps: 5,
            server_prompt_tokens: 100,
            server_completion_tokens: 50,
            wall_ms: 2000,
        };
        let report = Report::from_workspace("ui-review", "check the page", &ws, stats);
        assert_eq!(report.agent, "ui-review");
        assert_eq!(report.task, "check the page");
        assert_eq!(report.outcome, ws.outcome);
        assert_eq!(report.summary, ws.summary);
        assert_eq!(report.findings, ws.findings);
        assert_eq!(report.files, ws.files);
        assert_eq!(report.data, ws.data);
        assert_eq!(report.stats, stats);
    }
}

#[cfg(test)]
mod file_note_tests {
    use super::FileNote;

    #[test]
    fn file_note_without_note_parses_with_empty_note() {
        let n: FileNote = serde_json::from_value(serde_json::json!({"path": "a.rs"})).unwrap();
        assert_eq!(n.path, "a.rs");
        assert_eq!(n.note, "");
    }
}
