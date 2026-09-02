//! Budget fitting: trims a list of candidate snippets down to a token
//! budget, stopping at the first one that would overflow it.

use statie_core::{Snippet, TokenBudget, Tokenizer};

/// Keep snippets in order while the running total of
/// `tok.count(&format!("[{source}]\n{text}"))` stays `<= budget.0`. Stops at
/// the first snippet that would exceed the budget (does not skip past it to
/// try a later, smaller one).
pub fn fit_snippets(snippets: Vec<Snippet>, budget: TokenBudget, tok: &dyn Tokenizer) -> Vec<Snippet> {
    let mut kept = Vec::new();
    let mut total = 0usize;

    for snippet in snippets {
        let rendered = format!("[{}]\n{}", snippet.source, snippet.text);
        let tokens = tok.count(&rendered);
        if total + tokens > budget.0 {
            break;
        }
        total += tokens;
        kept.push(snippet);
    }

    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use statie_core::HeuristicTokenizer;

    #[test]
    fn stops_at_first_overflowing_snippet_even_if_a_later_one_would_fit() {
        let tok = HeuristicTokenizer;
        let snippets = vec![
            Snippet {
                source: "a".to_string(),
                text: "x".repeat(1000), // large, will overflow a small budget
            },
            Snippet {
                source: "b".to_string(),
                text: "y".to_string(), // tiny, would fit alone
            },
        ];
        let fitted = fit_snippets(snippets, TokenBudget(10), &tok);
        assert!(fitted.is_empty());
    }

    #[test]
    fn keeps_snippets_while_within_budget() {
        let tok = HeuristicTokenizer;
        let snippets = vec![
            Snippet {
                source: "a".to_string(),
                text: "short".to_string(),
            },
            Snippet {
                source: "b".to_string(),
                text: "short2".to_string(),
            },
        ];
        let fitted = fit_snippets(snippets, TokenBudget(1000), &tok);
        assert_eq!(fitted.len(), 2);
    }

    #[test]
    fn zero_budget_yields_empty() {
        let tok = HeuristicTokenizer;
        let snippets = vec![Snippet {
            source: "a".to_string(),
            text: "x".to_string(),
        }];
        let fitted = fit_snippets(snippets, TokenBudget(0), &tok);
        assert!(fitted.is_empty());
    }
}
