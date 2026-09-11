use super::support;

use serde_json::json;
use statie_core::Tool;
use statie_tools::Shell;

#[tokio::test]
async fn shell_output_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let tool = Shell {
        default_timeout_secs: 120,
        max_output_bytes: 1000,
    };

    let out = tool
        .call(json!({"cmd": "yes | head -c 300000"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.len() <= 1100, "len = {}", text.len());
    assert!(
        text.contains("output capped"),
        "got tail: {}",
        &text[text.len().saturating_sub(80)..]
    );
}
