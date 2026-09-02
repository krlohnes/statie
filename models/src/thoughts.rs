//! Shared helper for backends that don't deliver native thinking: extracting
//! a leading `<thoughts>...</thoughts>` block into a `Reasoning`.

use statie_core::Reasoning;

const OPEN_TAG: &str = "<thoughts>";
const CLOSE_TAG: &str = "</thoughts>";

/// Extracts a leading `<thoughts>...</thoughts>` block (tolerating leading
/// whitespace before it) into a `Reasoning`, returning the trimmed
/// remainder as content.
///
/// If the block is present but never closed, everything after the opening
/// tag is treated as reasoning and the remaining content is empty — the
/// model was cut off mid-thought, so there is no reliable JSON content to
/// recover, and surfacing the partial reasoning is more useful than
/// discarding it.
///
/// When there is no `<thoughts>` block at all, this returns `(None,
/// content.trim())` unchanged.
pub fn strip_thoughts(content: &str) -> (Option<Reasoning>, String) {
    let trimmed_start = content.trim_start();
    let Some(after_open) = trimmed_start.strip_prefix(OPEN_TAG) else {
        return (None, content.trim().to_string());
    };

    match after_open.find(CLOSE_TAG) {
        Some(close_idx) => {
            let reasoning_text = &after_open[..close_idx];
            let remainder = &after_open[close_idx + CLOSE_TAG.len()..];
            (
                Some(Reasoning::new(reasoning_text.trim())),
                remainder.trim().to_string(),
            )
        }
        None => (Some(Reasoning::new(after_open.trim())), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_block_extracts_reasoning_and_remainder() {
        let (reasoning, content) =
            strip_thoughts("<thoughts>hmm</thoughts>\n{\"patch\":{},\"action\":{\"type\":\"yield\"}}");
        assert_eq!(reasoning.expect("reasoning").as_ref(), "hmm");
        assert_eq!(content, r#"{"patch":{},"action":{"type":"yield"}}"#);
    }

    #[test]
    fn with_leading_whitespace_before_block() {
        let (reasoning, content) = strip_thoughts("   \n<thoughts>ok</thoughts>{}");
        assert_eq!(reasoning.expect("reasoning").as_ref(), "ok");
        assert_eq!(content, "{}");
    }

    #[test]
    fn without_block_returns_none_and_trimmed_content() {
        let (reasoning, content) = strip_thoughts("  {\"patch\":{}}  ");
        assert!(reasoning.is_none());
        assert_eq!(content, r#"{"patch":{}}"#);
    }

    #[test]
    fn unterminated_block_takes_everything_as_reasoning_with_empty_content() {
        let (reasoning, content) = strip_thoughts("<thoughts>still thinking, never closed");
        assert_eq!(
            reasoning.expect("reasoning").as_ref(),
            "still thinking, never closed"
        );
        assert_eq!(content, "");
    }
}
