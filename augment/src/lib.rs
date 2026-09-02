//! statie-augment: pluggable, composable retrieval augmenters —
//! `RepoBm25` (BM25 over the sandbox's source files), `SessionLogBm25`
//! (BM25 over the session log), and `TranscriptAugmenter` (the append-only
//! eval baseline) — plus the shared `query_text`/`fit_snippets` helpers
//! `RepoBm25`/`SessionLogBm25` are built on.

mod fit;
mod query;
mod repo;
mod session_log;
mod transcript;

pub use fit::fit_snippets;
pub use query::query_text;
pub use repo::{RepoBm25, RepoBm25Options};
pub use session_log::SessionLogBm25;
pub use transcript::TranscriptAugmenter;
