//! A small, dependency-free BM25 index (k1 = 1.2, b = 0.75).
//!
//! Used by `statie-store`'s `JsonlLog` to index session-log entries at write
//! time. Kept in `statie-core` so other crates (e.g. `statie-augment`'s
//! `RepoBm25`) can reuse the same tokenizer and scoring.

use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Lowercase, split on any non-alphanumeric char, drop tokens shorter than 2
/// chars. Also splits `snake_case`/`camelCase` identifiers into parts AND
/// keeps the whole identifier, so `read_file` yields `read_file`, `read`,
/// `file`.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        push_token(&mut out, raw);
        for part in split_identifier(raw) {
            push_token(&mut out, &part);
        }
    }
    out
}

fn push_token(out: &mut Vec<String>, s: &str) {
    let lower = s.to_lowercase();
    if lower.chars().count() >= 2 {
        out.push(lower);
    }
}

/// Split a `snake_case`/`camelCase` identifier into its component parts.
/// Does not include the whole identifier itself (the caller adds that).
fn split_identifier(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;

    for c in raw.chars() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        current.push(c);
        prev_lower = c.is_lowercase();
    }
    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

/// Per-document postings for one term: doc id -> term frequency in that doc.
type Postings = HashMap<u64, u32>;

/// A small, in-memory BM25 index over documents identified by `u64` id.
#[derive(Default)]
pub struct Bm25Index {
    /// term -> (doc id -> term frequency)
    postings: HashMap<String, Postings>,
    /// doc id -> document length (number of tokens)
    doc_lens: HashMap<u64, u32>,
    total_len: u64,
}

impl Bm25Index {
    pub fn new() -> Self {
        Bm25Index {
            postings: HashMap::new(),
            doc_lens: HashMap::new(),
            total_len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.doc_lens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc_lens.is_empty()
    }

    /// Add a document under `id`. Re-adding an existing id replaces it.
    pub fn add(&mut self, id: u64, text: &str) {
        self.remove(id);

        let tokens = tokenize(text);
        let len = tokens.len() as u32;

        let mut freqs: HashMap<String, u32> = HashMap::new();
        for tok in tokens {
            *freqs.entry(tok).or_insert(0) += 1;
        }

        for (term, freq) in freqs {
            self.postings.entry(term).or_default().insert(id, freq);
        }

        self.doc_lens.insert(id, len);
        self.total_len += len as u64;
    }

    fn remove(&mut self, id: u64) {
        if let Some(old_len) = self.doc_lens.remove(&id) {
            self.total_len -= old_len as u64;
            for postings in self.postings.values_mut() {
                postings.remove(&id);
            }
        }
    }

    fn avg_doc_len(&self) -> f32 {
        if self.doc_lens.is_empty() {
            0.0
        } else {
            self.total_len as f32 / self.doc_lens.len() as f32
        }
    }

    /// Search for `query`, returning up to `limit` `(id, score)` pairs sorted
    /// by score descending, ties by id ascending. Empty query yields no
    /// results. Documents matching zero query terms are not returned.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(u64, f32)> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        let n = self.doc_lens.len() as f32;
        let avg_len = self.avg_doc_len();

        // Dedup query terms so a repeated query token isn't double-counted.
        let mut seen_terms: Vec<String> = Vec::new();
        for t in &query_terms {
            if !seen_terms.contains(t) {
                seen_terms.push(t.clone());
            }
        }

        let mut scores: HashMap<u64, f32> = HashMap::new();

        for term in &seen_terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let df = postings.len() as f32;
            if df == 0.0 {
                continue;
            }
            // BM25 idf (with +1 to keep it non-negative for common terms).
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

            for (&doc_id, &tf) in postings {
                let doc_len = *self.doc_lens.get(&doc_id).unwrap_or(&0) as f32;
                let tf = tf as f32;
                let denom = tf + K1 * (1.0 - B + B * (doc_len / avg_len.max(1.0)));
                let term_score = idf * (tf * (K1 + 1.0)) / denom;
                *scores.entry(doc_id).or_insert(0.0) += term_score;
            }
        }

        let mut results: Vec<(u64, f32)> = scores.into_iter().collect();
        results.sort_by(|a, b| match b.1.partial_cmp(&a.1) {
            Some(std::cmp::Ordering::Equal) | None => a.0.cmp(&b.0),
            Some(ord) => ord,
        });
        results.truncate(limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_doc_with_repeated_term_ranks_above_long_doc_with_one_mention() {
        let mut idx = Bm25Index::new();
        idx.add(1, "parser parser parser assertion");
        idx.add(
            2,
            "a very long document about many unrelated things that only mentions \
             parser once amid a lot of filler words padding this out considerably \
             so its length dominates the score calculation entirely",
        );

        let results = idx.search("parser", 10);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].0, 1,
            "short doc with repeated term should rank first"
        );
        assert!(results[0].1 > results[1].1);
    }

    #[test]
    fn empty_query_is_empty() {
        let mut idx = Bm25Index::new();
        idx.add(1, "hello world");
        assert!(idx.search("", 10).is_empty());
    }

    #[test]
    fn absent_term_returns_empty() {
        let mut idx = Bm25Index::new();
        idx.add(1, "hello world");
        assert!(idx.search("giraffe", 10).is_empty());
    }

    #[test]
    fn identifier_splitting_finds_read_file_for_query_file() {
        let mut idx = Bm25Index::new();
        idx.add(1, "call read_file to load the contents");
        let results = idx.search("file", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn whole_identifier_is_also_indexed() {
        let mut idx = Bm25Index::new();
        idx.add(1, "invoke read_file now");
        let results = idx.search("read_file", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn readding_id_does_not_duplicate_results() {
        let mut idx = Bm25Index::new();
        idx.add(1, "parser assertion failed");
        idx.add(1, "parser assertion failed again");
        assert_eq!(idx.len(), 1);

        let results = idx.search("parser", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn limit_truncates_results() {
        let mut idx = Bm25Index::new();
        for i in 0..5 {
            idx.add(i, "common term shows up everywhere");
        }
        let results = idx.search("common", 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn tokenize_drops_short_tokens_and_lowercases() {
        let toks = tokenize("A Big CAT sat.");
        assert!(toks.contains(&"big".to_string()));
        assert!(toks.contains(&"cat".to_string()));
        assert!(toks.contains(&"sat".to_string()));
        assert!(!toks.contains(&"a".to_string()));
    }

    #[test]
    fn camel_case_identifier_splits_into_parts() {
        let toks = tokenize("readFile");
        assert!(toks.contains(&"readfile".to_string()));
        assert!(toks.contains(&"read".to_string()));
        assert!(toks.contains(&"file".to_string()));
    }
}
