//! `write_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{backup_before_edit, cap_output, join_string_array_args, write_atomic};

/// If an existing target file has more than this many lines, `write_file`
/// refuses to overwrite it: rewriting a whole file that large from memory
/// (rather than editing it) risks silently damaging unrelated lines the
/// model didn't mean to touch. Use `edit_file` for changes to a file this
/// size or larger.
const MAX_EXISTING_FILE_LINES: usize = 40;

/// Fixed part of the large-existing-file refusal, following `"{path} exists
/// with {n} lines"`. Quoted (verbatim) in [`WriteFile::description`], per
/// D13, so the two cannot drift.
pub const EXISTS_WITH_LINES_SUFFIX: &str = "; write_file replaces the whole file and you would \
    be writing it from memory. Use edit_file (old/new, or after/before + new) for changes to an \
    existing file. If the file is beyond repair, `revert_file` restores its content from before \
    this session's first edit. If you have read the whole file since your last edit of it, the \
    harness allows the rewrite automatically.";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Path to the file, relative to the sandbox root. Parent directories
    /// are created if missing.
    #[serde(alias = "file", alias = "file_path")]
    pub path: String,
    /// The full contents to write.
    pub content: String,
    /// Harness-set only (BACKLOG.md item 15): when true, the whole-file
    /// rewrite guard (`MAX_EXISTING_FILE_LINES`) is skipped because the
    /// harness's own `seen` record shows a full, recent read of this file.
    /// Not part of the schema offered to the model -- `write_file_confirmed_
    /// by_seen` (`skills/src/coding/mod.rs`) sets this on the args after
    /// stripping whatever the model itself sent for it, so the model cannot
    /// claim the confirmation on its own.
    #[serde(default)]
    #[schemars(skip)]
    pub confirmed_full_read: bool,
}

/// Writes a text file into the sandbox, creating parent directories and
/// writing atomically (temp file + rename).
pub struct WriteFile {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESCRIPTION.get_or_init(|| {
            format!(
                "Write a text file into the sandbox, creating parent directories as needed. \
                 Give path and content (the full file contents); write_file replaces the whole \
                 file, it does not patch it. An existing file over 40 lines is refused (name \
                 and line count first) unless the harness has recorded a full recent read of \
                 it, with the text \"{EXISTS_WITH_LINES_SUFFIX}\" For changes to an existing \
                 file, prefer edit_file."
            )
        })
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<WriteFileArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["path", "content"]);
        let args: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let sandbox_path = ctx.root.join(&args.path)?;
        let path = sandbox_path.as_path();

        if path.is_file() {
            let existing =
                std::fs::read(path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            // Count raw newline bytes rather than decoding to UTF-8: an
            // existing file the model can't read as text shouldn't crash
            // this guard, and the line count only needs to be a threshold
            // check, not an exact rendering.
            let n = bytecount_lines(&existing);
            if n > MAX_EXISTING_FILE_LINES && !args.confirmed_full_read {
                return Err(ToolError::ExecutionFailed(format!(
                    "{} exists with {n} lines{EXISTS_WITH_LINES_SUFFIX}",
                    args.path
                )));
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        }

        backup_before_edit(&ctx.root, &args.path)?;

        write_atomic(path, args.content.as_bytes())?;

        let bytes = args.content.len();
        let out = format!("wrote {bytes} bytes to {}", args.path);
        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Counts lines the way `str::lines()` would (one per `\n`, plus one more
/// for a final non-empty line with no trailing newline), but over raw bytes
/// so it works even if `bytes` isn't valid UTF-8.
fn bytecount_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}
