//! The `Augmenter` trait: pluggable, composable retrieval, each with its own
//! token budget.

use async_trait::async_trait;

use crate::prompt::Snippet;
use crate::promptable::Observation;
use crate::tokenizer::{TokenBudget, Tokenizer};

#[async_trait]
pub trait Augmenter<W>: Send + Sync {
    fn name(&self) -> &str;
    async fn augment(
        &self,
        s: &W,
        obs: Option<&Observation>,
        budget: TokenBudget,
        tok: &dyn Tokenizer,
    ) -> Vec<Snippet>;
}
