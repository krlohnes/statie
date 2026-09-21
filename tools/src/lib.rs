//! statie-tools: the coding harness's tool set — read, write, edit, search,
//! shell, test, recall, and filter_last_tool_call — all sandboxed against a
//! `statie_core::SandboxRoot`.

mod common;
mod edit_file;
mod filter_last;
mod list_files;
mod probe;
mod read_file;
mod recall;
mod revert_file;
mod search;
mod shell;
mod test_tool;
mod think;
mod try_fixes;
mod worktree;
mod write_file;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use statie_core::ToolSet;

pub use edit_file::{
    EditFile, OLD_MATCHED_BY_ANCHORS_PREFIX, OLD_NOT_FOUND_PREFIX, OLD_NOT_FOUND_SUFFIX,
    WRONG_FORM_MSG,
};
pub use filter_last::FilterLastToolCall;
pub use list_files::ListFiles;
pub use probe::{Probe, ProbeArgs};
pub use read_file::{REDUNDANT_READ_REFUSAL_PREFIX, ReadFile};
pub use recall::Recall;
pub use revert_file::RevertFile;
pub use search::Search;
pub use shell::Shell;
pub use test_tool::{NO_TEST_COMMAND_DETECTED_MSG, TestTool, detect_test_command};
pub use think::{Think, ThinkArgs};
pub use try_fixes::{TryFixes, TryFixesArgs};
pub use worktree::{
    Candidate, RunResult, Snapshot, WorktreeError, WorktreeRunner, apply_to_sandbox, candidate,
    diff, remove, run_in, run_in_with_env, snapshot,
};
pub use write_file::{EXISTS_WITH_LINES_SUFFIX, WriteFile};

fn default_shell_timeout_secs() -> u64 {
    120
}

fn default_allow_shell() -> bool {
    true
}

fn default_max_output_bytes() -> usize {
    200_000
}

/// Configuration for the tool set: shell timeout, whether shell/test tools
/// are registered at all, and the per-tool output cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    pub shell_timeout_secs: u64,
    pub allow_shell: bool,
    pub max_output_bytes: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        ToolConfig {
            shell_timeout_secs: default_shell_timeout_secs(),
            allow_shell: default_allow_shell(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

/// Builds the default `ToolSet`: `read_file`, `write_file`, `list_files`,
/// and `recall` always; `shell` and `test` only when `cfg.allow_shell`.
pub fn default_tools(cfg: &ToolConfig) -> ToolSet {
    let mut tools = ToolSet::new();

    tools.register(Arc::new(ReadFile::new(cfg.max_output_bytes)));
    tools.register(Arc::new(WriteFile {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(EditFile {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(RevertFile {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(ListFiles {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(Search {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(Recall {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(FilterLastToolCall {
        max_output_bytes: cfg.max_output_bytes,
    }));
    tools.register(Arc::new(Think));
    tools.register(Arc::new(TryFixes));
    tools.register(Arc::new(Probe {
        default_timeout_secs: cfg.shell_timeout_secs,
        max_output_bytes: cfg.max_output_bytes,
    }));

    if cfg.allow_shell {
        tools.register(Arc::new(Shell {
            default_timeout_secs: cfg.shell_timeout_secs,
            max_output_bytes: cfg.max_output_bytes,
        }));
        tools.register(Arc::new(TestTool {
            default_timeout_secs: cfg.shell_timeout_secs,
            max_output_bytes: cfg.max_output_bytes,
        }));
    }

    tools
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn default_config_matches_spec() {
        let cfg = ToolConfig::default();
        assert_eq!(cfg.shell_timeout_secs, 120);
        assert!(cfg.allow_shell);
        assert_eq!(cfg.max_output_bytes, 200_000);
    }

    #[test]
    fn default_tools_registers_all_when_shell_allowed() {
        let cfg = ToolConfig::default();
        let tools = default_tools(&cfg);
        let mut names = tools.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "edit_file",
                "filter_last_tool_call",
                "list_files",
                "probe",
                "read_file",
                "recall",
                "revert_file",
                "search",
                "shell",
                "test",
                "think",
                "try_fixes",
                "write_file"
            ]
        );
    }

    #[test]
    fn default_tools_skips_shell_and_test_when_disallowed() {
        let cfg = ToolConfig {
            allow_shell: false,
            ..ToolConfig::default()
        };
        let tools = default_tools(&cfg);
        let mut names = tools.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "edit_file",
                "filter_last_tool_call",
                "list_files",
                "probe",
                "read_file",
                "recall",
                "revert_file",
                "search",
                "think",
                "try_fixes",
                "write_file"
            ]
        );
    }

    #[test]
    fn default_tools_contains_think_with_required_question_arg() {
        let cfg = ToolConfig::default();
        let tools = default_tools(&cfg);
        let think = tools.get("think").expect("think should be registered");
        assert_eq!(think.name(), "think");

        let schema = think.args_schema();
        let value = schema.as_value();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("schema should have required");
        assert_eq!(required, &vec![Value::String("question".to_string())]);
    }

    #[tokio::test]
    async fn think_call_returns_session_dispatch_error() {
        let cfg = ToolConfig::default();
        let tools = default_tools(&cfg);
        let think = tools.get("think").expect("think should be registered");

        let dir = tempfile::tempdir().unwrap();
        let ctx = statie_core::ToolCtx {
            root: statie_core::SandboxRoot::new(dir.path()).unwrap(),
            log: Arc::new(statie_core::MemoryLog::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = think
            .call(serde_json::json!({"question": "why?"}), &ctx)
            .await
            .expect_err("think's call must always fail");
        assert!(
            matches!(err, statie_core::ToolError::ExecutionFailed(ref m) if m == "`think` is run by the session, not the tool registry"),
            "{err}"
        );
    }

    #[test]
    fn tool_config_deserializes_with_partial_json() {
        let cfg: ToolConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.shell_timeout_secs, 120);
        assert!(cfg.allow_shell);
        assert_eq!(cfg.max_output_bytes, 200_000);

        let cfg2: ToolConfig = serde_json::from_str(r#"{"allow_shell": false}"#).unwrap();
        assert!(!cfg2.allow_shell);
        assert_eq!(cfg2.shell_timeout_secs, 120);
    }
}
