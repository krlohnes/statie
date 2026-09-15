//! `read_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, join_string_array_args};

/// Fixed prefix of the redundant-identical-read refusal that
/// `core/src/session.rs`'s 7.8 guard returns for a repeated `read_file`
/// call over an unchanged range (the rest of the message names the call,
/// step, and where to find the earlier output, and is not fixed text).
/// Quoted (verbatim) in [`ReadFile::description`]. This text is produced by
/// the harness's session guard, not by this tool, so it cannot be built
/// from a shared `pub const` without a core source change (out of scope
/// here); it is reproduced here as a literal instead.
pub const REDUNDANT_READ_REFUSAL_PREFIX: &str = statie_core::REDUNDANT_READ_PREFIX;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file, relative to the sandbox root.
    #[serde(alias = "file", alias = "file_path")]
    pub path: String,
    /// 1-based line number to start reading from. Defaults to 1.
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Defaults to 100; the window is
    /// also capped by size, so prefer several small windows or `search`
    /// over one big read.
    pub limit: Option<usize>,
}

pub(crate) fn default_limit() -> usize {
    100
}

pub(crate) fn default_max_window_bytes() -> usize {
    6_000
}

/// Reads a text file (or a window of it) from the sandbox, prefixing each
/// line with its line number.
pub struct ReadFile {
    pub max_output_bytes: usize,
    /// Byte budget for the rendered, numbered window (independent of
    /// `limit`): the window is cut early, at a line boundary, once
    /// rendering the next line would exceed this. Roughly 1500 tokens at
    /// the harness's ~4 bytes/token estimate for dense source.
    pub max_window_bytes: usize,
}

impl ReadFile {
    pub fn new(max_output_bytes: usize) -> Self {
        ReadFile {
            max_output_bytes,
            max_window_bytes: default_max_window_bytes(),
        }
    }
}

impl Default for ReadFile {
    fn default() -> Self {
        ReadFile::new(200_000)
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESCRIPTION.get_or_init(|| {
            format!(
                "Read a window of a text file. Give path, and optionally offset (1-based start \
                 line, default 1) and limit (max lines, default 100; the window is also \
                 size-capped). The output header shows the total line count and the range \
                 actually shown, and ends with a trailer naming the next offset when more lines \
                 remain. Calling this again with the same path/offset/limit after nothing has \
                 changed is refused, starting \"{REDUNDANT_READ_REFUSAL_PREFIX}\"; pass a \
                 different offset/limit, or search, or make the change you committed to."
            )
        })
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ReadFileArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["path"]);
        let args: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or_else(default_limit);

        let out = read_window(&args.path, offset, limit, self.max_window_bytes, ctx)?;

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Builds the same numbered-window body `read_file` returns (header line
/// plus `{n:>5} | {line}` rows, plus the "more lines" trailer), for `path`
/// starting at 1-based `offset` for up to `limit` lines, cut early at a line
/// boundary once the window would exceed `max_window_bytes`. Shared with
/// `search`, which falls back to this when called with a `path` and no
/// `pattern`.
pub(crate) fn read_window(
    path: &str,
    offset: usize,
    limit: usize,
    max_window_bytes: usize,
    ctx: &ToolCtx,
) -> Result<String, ToolError> {
    let sandbox_path = ctx.root.join(path)?;
    let fs_path = sandbox_path.as_path();

    if !fs_path.exists() {
        return Err(ToolError::ExecutionFailed(format!("no such file: {path}")));
    }
    if !fs_path.is_file() {
        return Err(ToolError::ExecutionFailed(format!(
            "not a regular file: {path}"
        )));
    }

    let bytes = std::fs::read(fs_path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| ToolError::ExecutionFailed(format!("not valid utf-8: {path}")))?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    let from = offset.min(total.max(1));
    let start_idx = from.saturating_sub(1);
    let limit_end_idx = (start_idx + limit).min(total);

    // Render lines one at a time, stopping early (at a line boundary)
    // once the next line would push the rendered window past the byte
    // cap. This is independent of `limit`: a `limit` of 100 dense Rust
    // lines can still overflow the observation budget on its own.
    let header_prefix_len = format!("{path} ").len();
    let mut body = String::new();
    let mut end_idx = start_idx;
    let mut byte_capped = false;
    for (i, line) in lines[start_idx..limit_end_idx].iter().enumerate() {
        let n = start_idx + i + 1;
        let rendered = format!("{n:>5} | {line}\n");
        if header_prefix_len + body.len() + rendered.len() > max_window_bytes && end_idx > start_idx
        {
            byte_capped = true;
            break;
        }
        body.push_str(&rendered);
        end_idx = n;
    }
    let to = end_idx;

    let mut out = format!("{path} ({total} lines, showing {from}-{to})\n");
    out.push_str(&body);

    let remaining = total.saturating_sub(to);
    if remaining > 0 {
        if byte_capped {
            out.push_str(&format!(
                "... window cut at {max_window_bytes} bytes ({remaining} more lines); call read_file with offset={}\n",
                to + 1
            ));
        } else {
            out.push_str(&format!(
                "... {remaining} more lines; call read_file with offset={} to continue\n",
                to + 1
            ));
        }
    }

    Ok(out)
}
