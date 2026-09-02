//! `RepoBm25`: BM25 retrieval over the sandbox's source files, chunked by
//! line ranges.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use statie_core::{
    Augmenter, Bm25Index, Observation, SandboxRoot, Snippet, TokenBudget, Tokenizer,
};

use crate::fit::fit_snippets;
use crate::query::query_text;

/// Options controlling how `RepoBm25` walks and chunks the sandbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoBm25Options {
    /// Files larger than this are skipped.
    pub max_file_bytes: usize,
    /// Number of lines per chunk (the last chunk in a file may be shorter).
    pub chunk_lines: usize,
    /// Maximum number of snippets `augment` returns per call.
    pub max_snippets: usize,
    /// Directory names skipped at any depth.
    pub exclude_dirs: Vec<String>,
    /// If `Some`, only files whose extension is in this list are indexed.
    /// `None` means any text (valid UTF-8) file.
    pub extensions: Option<Vec<String>>,
}

impl Default for RepoBm25Options {
    fn default() -> Self {
        RepoBm25Options {
            max_file_bytes: 200_000,
            chunk_lines: 40,
            max_snippets: 4,
            exclude_dirs: vec![
                ".git".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".statie".to_string(),
                "__pycache__".to_string(),
                ".venv".to_string(),
                "venv".to_string(),
            ],
            extensions: None,
        }
    }
}

/// One indexed chunk of a file: a contiguous, 1-based inclusive line range.
struct Chunk {
    path: String,
    start_line: usize,
    end_line: usize,
    text: String,
}

/// Fingerprint of every indexed file's on-disk state, keyed by relative
/// path: `(mtime as nanos since epoch, 0 if unavailable; file length)`.
type Fingerprint = BTreeMap<String, (u64, u64)>;

struct RepoState {
    index: Bm25Index,
    chunks: Vec<Chunk>,
    fingerprint: Fingerprint,
}

/// BM25 retrieval over the files under a `SandboxRoot`.
pub struct RepoBm25 {
    root: SandboxRoot,
    opts: RepoBm25Options,
    state: RwLock<RepoState>,
}

impl RepoBm25 {
    /// Walks `root` (skipping `opts.exclude_dirs` at any depth), skips files
    /// over `opts.max_file_bytes` or that aren't valid UTF-8, splits each
    /// remaining file into `opts.chunk_lines`-line chunks, and indexes each
    /// chunk with text = `"{relative path} {chunk text}"`.
    pub fn build(root: &SandboxRoot, opts: RepoBm25Options) -> std::io::Result<Self> {
        let (index, chunks, fingerprint) = index_root(root, &opts)?;
        Ok(RepoBm25 {
            root: root.clone(),
            opts,
            state: RwLock::new(RepoState {
                index,
                chunks,
                fingerprint,
            }),
        })
    }

    /// Rebuilds the index from disk unconditionally.
    pub fn refresh(&self) -> std::io::Result<()> {
        let (index, chunks, fingerprint) = index_root(&self.root, &self.opts)?;
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.index = index;
        state.chunks = chunks;
        state.fingerprint = fingerprint;
        Ok(())
    }

    /// Computes the current on-disk fingerprint (same walk and filters as
    /// `index_root`, but metadata only — no file reads).
    fn current_fingerprint(&self) -> std::io::Result<Fingerprint> {
        current_fingerprint(&self.root, &self.opts)
    }

    /// Re-indexes from disk if the sandbox's file set/mtimes/lengths have
    /// changed since the last index or refresh. Returns `true` if it
    /// re-indexed, `false` if the stored index was already up to date.
    pub fn refresh_if_changed(&self) -> std::io::Result<bool> {
        let current = self.current_fingerprint()?;
        let changed = {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            state.fingerprint != current
        };
        if !changed {
            return Ok(false);
        }

        let (index, chunks, fingerprint) = index_root(&self.root, &self.opts)?;
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.index = index;
        state.chunks = chunks;
        state.fingerprint = fingerprint;
        Ok(true)
    }
}

/// Returns `true` if `path` (relative to `root`) passes the exclude-dir,
/// extension, and size filters used by both `index_root` and
/// `current_fingerprint`.
fn is_indexable_file(
    entry: &walkdir::DirEntry,
    opts: &RepoBm25Options,
) -> Option<std::fs::Metadata> {
    if !entry.file_type().is_file() {
        return None;
    }
    let path = entry.path();

    if let Some(exts) = &opts.extensions {
        let matches = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|allowed| allowed == e))
            .unwrap_or(false);
        if !matches {
            return None;
        }
    }

    let metadata = entry.metadata().ok()?;
    if metadata.len() as usize > opts.max_file_bytes {
        return None;
    }

    Some(metadata)
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn fingerprint_entry(metadata: &std::fs::Metadata) -> (u64, u64) {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (mtime, metadata.len())
}

fn current_fingerprint(root: &SandboxRoot, opts: &RepoBm25Options) -> std::io::Result<Fingerprint> {
    let mut fingerprint = Fingerprint::new();

    let walker = walkdir::WalkDir::new(root.path())
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e, root.path(), &opts.exclude_dirs));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let Some(metadata) = is_indexable_file(&entry, opts) else {
            continue;
        };
        let rel = relative_path(root.path(), entry.path());
        fingerprint.insert(rel, fingerprint_entry(&metadata));
    }

    Ok(fingerprint)
}

fn index_root(
    root: &SandboxRoot,
    opts: &RepoBm25Options,
) -> std::io::Result<(Bm25Index, Vec<Chunk>, Fingerprint)> {
    let mut index = Bm25Index::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut fingerprint = Fingerprint::new();

    let walker = walkdir::WalkDir::new(root.path())
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e, root.path(), &opts.exclude_dirs));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let Some(metadata) = is_indexable_file(&entry, opts) else {
            continue;
        };
        let path = entry.path();
        let rel = relative_path(root.path(), path);

        // This file passed the walk/exclude/extension/size filters, so it
        // counts toward the fingerprint regardless of whether it turns out
        // to be readable/valid UTF-8 below.
        fingerprint.insert(rel.clone(), fingerprint_entry(&metadata));

        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }

        let chunk_lines = opts.chunk_lines.max(1);
        let mut start = 0usize;
        while start < lines.len() {
            let end = (start + chunk_lines).min(lines.len());
            let chunk_text = lines[start..end].join("\n");
            let start_line = start + 1;
            let end_line = end;

            let id = chunks.len() as u64;
            let doc_text = format!("{rel} {chunk_text}");
            index.add(id, &doc_text);
            chunks.push(Chunk {
                path: rel.clone(),
                start_line,
                end_line,
                text: chunk_text,
            });

            start = end;
        }
    }

    Ok((index, chunks, fingerprint))
}

fn is_excluded_dir(entry: &walkdir::DirEntry, root: &Path, exclude_dirs: &[String]) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    if !entry.file_type().is_dir() {
        return false;
    }
    let _ = root;
    entry
        .file_name()
        .to_str()
        .map(|name| exclude_dirs.iter().any(|ex| ex == name))
        .unwrap_or(false)
}

#[async_trait]
impl<W: Serialize + Send + Sync> Augmenter<W> for RepoBm25 {
    fn name(&self) -> &str {
        "repo_bm25"
    }

    async fn augment(
        &self,
        s: &W,
        obs: Option<&Observation>,
        budget: TokenBudget,
        tok: &dyn Tokenizer,
    ) -> Vec<Snippet> {
        let query = query_text(s, obs);
        if query.trim().is_empty() {
            return Vec::new();
        }

        if let Err(err) = self.refresh_if_changed() {
            tracing::warn!("repo_bm25: failed to refresh index, using stale index: {err}");
        }

        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let hits = state.index.search(&query, self.opts.max_snippets);

        let snippets: Vec<Snippet> = hits
            .into_iter()
            .filter_map(|(id, _score)| state.chunks.get(id as usize))
            .map(|chunk| Snippet {
                source: format!("{}:{}-{}", chunk.path, chunk.start_line, chunk.end_line),
                text: chunk.text.clone(),
            })
            .collect();

        fit_snippets(snippets, budget, tok)
    }
}
