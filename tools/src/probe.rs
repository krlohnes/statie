//! `probe` tool (BACKLOG.md item 7, "Trace by execution"): runs a short
//! snippet against the current code in an isolated worktree and returns
//! what it prints. PAL/Program-of-Thoughts and LDB both point at the same
//! failure mode: a model decomposes a problem well but computes badly (our
//! own trap run: `4600 / 100 * 90` computed as zero every time), and
//! showing measured intermediate values instead of asking the model to
//! trace them by hand recovers real benchmark points. This tool is the
//! general primitive: it never imagines a value, it prints one.
//!
//! Unlike `think`/`try_fixes` (`tools/src/think.rs`, `tools/src/try_fixes.rs`),
//! this tool runs directly in the registry -- it needs nothing from the
//! session beyond the `ToolCtx` every other registry tool gets.
//!
//! Both languages share the same shape: [`crate::worktree::snapshot`] the
//! sandbox, [`crate::worktree::candidate`] with an edit that drops the
//! snippet file into the worktree, [`crate::worktree::run_in`] the
//! language's runner, then [`crate::worktree::remove`] -- always, on every
//! path, so the sandbox is never left holding a probe file and no worktree
//! ever lingers under `.statie/worktrees/`.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::worktree::{self, Candidate, Snapshot, WorktreeError};

/// A snippet longer than this is refused before anything runs -- a probe is
/// meant to be a few lines that print values, not a rewrite.
const MAX_SNIPPET_CHARS: usize = 4000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbeArgs {
    /// The snippet to run. Rust: the body of a `#[test]` function (use
    /// `crate_name::path::to::item` to reach the crate's own code;
    /// `println!`/`dbg!` output is returned). Python: a module's top-level
    /// code, run with the sandbox root on `sys.path`.
    #[schemars(length(min = 1, max = 4000))]
    pub code: String,
    /// "rust" or "python". Defaults from the sandbox: rust when
    /// `Cargo.toml` exists at the root, python when there are `.py` files
    /// and no `Cargo.toml`.
    pub lang: Option<String>,
}

/// Runs a short snippet against the current code in an isolated copy of the
/// sandbox and returns what it prints.
pub struct Probe {
    pub default_timeout_secs: u64,
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn description(&self) -> &str {
        "Run a short snippet against the current code in an isolated copy of the sandbox and \
         return what it prints. Use it to see real intermediate values instead of computing \
         them in your head: print the inputs, the result of each step, and the expected value. \
         Rust snippets are the body of a `#[test]` in the crate's tests directory (use \
         `crate_name::path::to::item`; `println!` and `dbg!` are shown); Python snippets are a \
         module run with the sandbox on `sys.path`."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ProbeArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: ProbeArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if args.code.chars().count() > MAX_SNIPPET_CHARS {
            return Err(ToolError::InvalidArgs(format!(
                "snippet is too long ({} chars); probe refuses anything over {MAX_SNIPPET_CHARS} \
                 chars -- keep it to a few lines that print values",
                args.code.chars().count()
            )));
        }

        let lang = match args.lang {
            Some(l) => l,
            None => detect_lang(ctx.root.path()).ok_or_else(|| {
                ToolError::ExecutionFailed("cannot tell the language; pass lang".to_string())
            })?,
        };

        let root = ctx.root.path().to_path_buf();
        let code = args.code.clone();
        let timeout = std::time::Duration::from_secs(self.default_timeout_secs);
        let max_output_bytes = self.max_output_bytes;

        let output = match lang.as_str() {
            "rust" => run_rust_probe(&root, &code, timeout, max_output_bytes).await?,
            "python" => run_python_probe(&root, &code, timeout, max_output_bytes).await?,
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "unsupported lang {other:?}; expected \"rust\" or \"python\""
                )));
            }
        };

        Ok(ToolOutputFull::new(output))
    }
}

/// Detects the sandbox's language the same way the tool's `lang` default is
/// documented: rust when `Cargo.toml` exists at the root, python when there
/// are `.py` files and no `Cargo.toml`; `None` (caller errors) otherwise.
fn detect_lang(root: &std::path::Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        return Some("rust".to_string());
    }
    if has_py_files(root) {
        return Some("python".to_string());
    }
    None
}

/// Shallow-ish scan for any `.py` file under `root`, skipping common noise
/// directories so a stray venv/target doesn't dominate the walk. Only needs
/// to answer yes/no, so it stops at the first hit.
fn has_py_files(root: &std::path::Path) -> bool {
    fn walk(dir: &std::path::Path, depth: u32) -> bool {
        if depth > 6 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "venv"
                || name == ".venv"
                || name == "__pycache__"
            {
                continue;
            }
            if path.is_dir() {
                if walk(&path, depth + 1) {
                    return true;
                }
            } else if name.ends_with(".py") {
                return true;
            }
        }
        false
    }
    walk(root, 0)
}

/// Reads the `[package] name` line out of `Cargo.toml` at `root`, returning
/// the crate's module name (`-` replaced by `_`, matching how cargo derives
/// the lib target's module name from the package name).
fn crate_module_name(root: &std::path::Path) -> Result<String, ToolError> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| ToolError::ExecutionFailed(format!("failed to read Cargo.toml: {e}")))?;

    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim();
            let name = rest.trim_matches(|c| c == '"' || c == '\'');
            if name != rest && !name.is_empty() {
                return Ok(name.replace('-', "_"));
            }
        }
    }

    Err(ToolError::ExecutionFailed(
        "no `[package] name = \"...\"` line found in Cargo.toml".to_string(),
    ))
}

/// Builds and runs the Rust probe: writes `tests/__statie_probe.rs`
/// containing a single `#[test] fn __statie_probe()` wrapping `code`, runs
/// `cargo test --test __statie_probe -- --nocapture`, and always removes
/// the worktree afterward regardless of outcome.
async fn run_rust_probe(
    root: &std::path::Path,
    code: &str,
    timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<String, ToolError> {
    let crate_name = crate_module_name(root)?;

    let snapshot = do_snapshot(root)?;
    let code_owned = code.to_string();
    let crate_name_for_edit = crate_name.clone();
    let candidate_result = do_candidate(&snapshot, move |worktree| {
        let test_file = worktree.join("tests").join("__statie_probe.rs");
        if let Some(parent) = test_file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let contents = format!(
            "#![allow(unused)]\nuse {crate_name_for_edit}::*;\n#[test]\nfn __statie_probe() {{\n{code_owned}\n}}\n"
        );
        std::fs::write(&test_file, contents).map_err(|e| e.to_string())
    });

    let candidate = match candidate_result {
        Ok(c) => c,
        Err(e) => return Err(worktree_err(e)),
    };

    let run_result = worktree::run_in(
        &candidate,
        "cargo test --test __statie_probe -- --nocapture",
        timeout,
        max_output_bytes,
    )
    .await;

    let removed = worktree::remove(candidate);

    let run_result = run_result.map_err(worktree_err)?;
    removed.map_err(worktree_err)?;

    Ok(format_rust_output(&run_result))
}

/// Strips cargo's own noise from `run_result`'s stdout (the `Compiling`/
/// `Finished`/`Running` lines, the `running 1 test` banner, the
/// `test __statie_probe ... ok`/`FAILED` line, and the `test result:`
/// summary) and prefixes the rest with the verdict line documented for this
/// tool. Compile errors are kept verbatim (nothing rustc prints on a failed
/// build matches those prefixes) since the model needs to see them.
fn format_rust_output(run_result: &worktree::RunResult) -> String {
    let exit = run_result.exit.unwrap_or(-1);
    let mut lines: Vec<&str> = Vec::new();
    for line in run_result.stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Finished")
            || trimmed.starts_with("Running")
            || trimmed.starts_with("running 1 test")
            || trimmed.starts_with("test result:")
            || trimmed.starts_with("test __statie_probe ...")
        {
            continue;
        }
        lines.push(line);
    }
    let mut body = lines.join("\n");
    if !run_result.stderr.trim().is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&run_result.stderr);
    }

    let verdict = if run_result.timed_out {
        "probe (rust): timed out".to_string()
    } else {
        format!("probe (rust): exit {exit}")
    };

    if body.trim().is_empty() {
        verdict
    } else {
        format!("{verdict}\n{body}")
    }
}

/// Builds and runs the Python probe: writes `__statie_probe.py` at the
/// worktree root containing `code` verbatim, runs `python3
/// __statie_probe.py` from the worktree root (so the sandbox's own modules
/// import naturally), and always removes the worktree afterward.
async fn run_python_probe(
    root: &std::path::Path,
    code: &str,
    timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<String, ToolError> {
    let snapshot = do_snapshot(root)?;
    let code_owned = code.to_string();
    let candidate_result = do_candidate(&snapshot, move |worktree| {
        let probe_file = worktree.join("__statie_probe.py");
        std::fs::write(&probe_file, code_owned).map_err(|e| e.to_string())
    });

    let candidate = match candidate_result {
        Ok(c) => c,
        Err(e) => return Err(worktree_err(e)),
    };

    let run_result = worktree::run_in(
        &candidate,
        "python3 __statie_probe.py",
        timeout,
        max_output_bytes,
    )
    .await;

    let removed = worktree::remove(candidate);

    let run_result = run_result.map_err(worktree_err)?;
    removed.map_err(worktree_err)?;

    Ok(format_python_output(&run_result))
}

fn format_python_output(run_result: &worktree::RunResult) -> String {
    let exit = run_result.exit.unwrap_or(-1);
    let verdict = if run_result.timed_out {
        "probe (python): timed out".to_string()
    } else {
        format!("probe (python): exit {exit}")
    };

    let mut out = verdict;
    if !run_result.stdout.is_empty() {
        out.push('\n');
        out.push_str(&run_result.stdout);
    }
    if !run_result.stderr.trim().is_empty() {
        out.push('\n');
        out.push_str(&run_result.stderr);
    }
    out
}

fn worktree_err(e: WorktreeError) -> ToolError {
    ToolError::ExecutionFailed(format!("probe failed: {e}"))
}

/// `snapshot`/`candidate` run synchronous git subprocess calls; `probe`'s
/// `call` is itself async, but these are quick enough (like `try_fixes`
/// would be) to run inline rather than via `spawn_blocking` -- matching how
/// this crate's other worktree-backed code (`worktree::CandidateRunner`)
/// only bothers with `spawn_blocking` when driven through the `core` trait
/// object from the session loop.
fn do_snapshot(root: &std::path::Path) -> Result<Snapshot, ToolError> {
    worktree::snapshot(root).map_err(worktree_err)
}

fn do_candidate<F>(snapshot: &Snapshot, edit: F) -> Result<Candidate, WorktreeError>
where
    F: FnOnce(&std::path::Path) -> Result<(), String>,
{
    worktree::candidate(snapshot, edit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_schema_requires_code_not_lang() {
        let schema = Schema::of::<ProbeArgs>();
        let value = schema.as_value();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("schema should have required");
        assert_eq!(required, &vec![Value::String("code".to_string())]);

        let props = value
            .get("properties")
            .and_then(Value::as_object)
            .expect("schema should have properties");
        assert!(props.contains_key("code"));
        assert!(props.contains_key("lang"));
    }

    #[test]
    fn crate_module_name_reads_package_name_and_replaces_dashes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(crate_module_name(dir.path()).unwrap(), "my_crate");
    }

    #[test]
    fn crate_module_name_errors_without_package_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        assert!(crate_module_name(dir.path()).is_err());
    }

    #[test]
    fn detect_lang_prefers_rust_when_cargo_toml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("a.py"), "print(1)\n").unwrap();
        assert_eq!(detect_lang(dir.path()), Some("rust".to_string()));
    }

    #[test]
    fn detect_lang_python_when_only_py_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "print(1)\n").unwrap();
        assert_eq!(detect_lang(dir.path()), Some("python".to_string()));
    }

    #[test]
    fn detect_lang_none_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_lang(dir.path()), None);
    }

    #[tokio::test]
    async fn snippet_over_max_chars_is_refused_before_running() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = statie_core::ToolCtx {
            root: statie_core::SandboxRoot::new(dir.path()).unwrap(),
            log: std::sync::Arc::new(statie_core::MemoryLog::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let tool = Probe {
            default_timeout_secs: 60,
            max_output_bytes: 200_000,
        };
        let long_code = "x".repeat(5000);
        let err = tool
            .call(
                serde_json::json!({"code": long_code, "lang": "python"}),
                &ctx,
            )
            .await
            .expect_err("must refuse an over-length snippet");
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("too long")),
            "{err}"
        );
        // Nothing ran: no worktree scratch dir should have been created.
        assert!(!dir.path().join(".statie").join("worktrees").exists());
    }
}
