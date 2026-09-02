//! `RepoBm25`: BM25 retrieval over the sandbox's source files, chunked by
//! line ranges.

use std::path::Path;
use std::sync::RwLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use statie_core::{Augmenter, Bm25Index, Observation, SandboxRoot, Snippet, TokenBudget, Tokenizer};

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

struct RepoState {
    index: Bm25Index,
    chunks: Vec<Chunk>,
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
        let (index, chunks) = index_root(root, &opts)?;
        Ok(RepoBm25 {
            root: root.clone(),
            opts,
            state: RwLock::new(RepoState { index, chunks }),
        })
    }

    /// Rebuilds the index from disk.
    pub fn refresh(&mut self) -> std::io::Result<()> {
        let (index, chunks) = index_root(&self.root, &self.opts)?;
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.index = index;
        state.chunks = chunks;
        Ok(())
    }
}

fn index_root(root: &SandboxRoot, opts: &RepoBm25Options) -> std::io::Result<(Bm25Index, Vec<Chunk>)> {
    let mut index = Bm25Index::new();
    let mut chunks: Vec<Chunk> = Vec::new();

    let walker = walkdir::WalkDir::new(root.path())
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e, root.path(), &opts.exclude_dirs));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        if let Some(exts) = &opts.extensions {
            let matches = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.iter().any(|allowed| allowed == e))
                .unwrap_or(false);
            if !matches {
                continue;
            }
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() as usize > opts.max_file_bytes {
            continue;
        }

        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        let rel = path
            .strip_prefix(root.path())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

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

    Ok((index, chunks))
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
