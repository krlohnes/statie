//! Tokenizers and budgets.

use once_cell::sync::Lazy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tiktoken_rs::CoreBPE;

/// Something that can count tokens in a string.
pub trait Tokenizer: Send + Sync {
    fn count(&self, s: &str) -> usize;
}

/// ceil(chars / 4). Used in tests; cheap and deterministic.
pub struct HeuristicTokenizer;

impl Tokenizer for HeuristicTokenizer {
    fn count(&self, s: &str) -> usize {
        s.chars().count().div_ceil(4)
    }
}

static CL100K: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer data"));

/// cl100k_base via tiktoken-rs. Default tokenizer at runtime.
pub struct TiktokenTokenizer;

impl Tokenizer for TiktokenTokenizer {
    fn count(&self, s: &str) -> usize {
        CL100K.encode_with_special_tokens(s).len()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Budgets {
    pub total: usize,
    pub observation: usize,
    pub augmentation: usize,
    /// Cap on the harness-maintained `notes` section (see
    /// `crate::maintainer`). Default 400 tokens.
    #[serde(default = "default_notes_budget")]
    pub notes: usize,
    /// Cap on the working-set section (statie-spec.md "10. Working set").
    /// Default 6000 tokens.
    #[serde(default = "default_working_set_budget")]
    pub working_set: usize,
}

fn default_notes_budget() -> usize {
    400
}

fn default_working_set_budget() -> usize {
    6000
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            total: 24000,
            observation: 3000,
            augmentation: 2000,
            notes: default_notes_budget(),
            working_set: default_working_set_budget(),
        }
    }
}

pub struct TokenBudget(pub usize);
