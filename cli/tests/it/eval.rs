//! End-to-end test for `statie eval` (milestone 7): both harnesses succeed
//! on the same mock script, but the append-only baseline's prompt grows
//! while statie's stays flat.

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

#[test]
fn eval_toy_json_shows_flat_statie_and_growing_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    // 3 echo turns (counter 1, 2, 3) then a `done` turn -- 4 steps total.
    // Each of the 3 echo turns scripts server usage (100/10, 110/11, 120/12
    // -- sums to 330 prompt / 33 completion tokens); the `done` turn does not.
    let mock_script = serde_json::json!([
        {
            "content": serde_json::json!({
                "patch": {"counter": 1, "recent_summary": "did step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hello 1"}},
                "ack_inbox": [1],
            }).to_string(),
            "prompt_tokens": 100,
            "completion_tokens": 10,
        },
        {
            "content": serde_json::json!({
                "patch": {"counter": 2, "recent_summary": "did step 2"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hello 2"}},
            }).to_string(),
            "prompt_tokens": 110,
            "completion_tokens": 11,
        },
        {
            "content": serde_json::json!({
                "patch": {"counter": 3, "recent_summary": "did step 3"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hello 3"}},
            }).to_string(),
            "prompt_tokens": 120,
            "completion_tokens": 12,
        },
        {
            "content": serde_json::json!({
                "patch": {"counter": 3},
                "action": {"type": "done"},
            }).to_string()
        },
    ]);

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": mock_script,
        },
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "eval",
            "--task",
            "toy",
            "--steps",
            "3",
            "--json",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );

    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");

    let statie = &value["statie"];
    let baseline = &value["baseline"];

    let statie_tokens: Vec<u64> = statie["prompt_tokens"]
        .as_array()
        .expect("statie.prompt_tokens should be an array")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(
        statie_tokens.len(),
        3,
        "statie.prompt_tokens: {statie_tokens:?}"
    );
    let min = *statie_tokens.iter().min().unwrap();
    let max = *statie_tokens.iter().max().unwrap();
    // The working set (statie-spec.md "10. Working set") is on by default
    // (`Session::with_working_set` is called with the config's own
    // `working_set` section, whose built-in default is "on": 4 windows, 6
    // recent-call lines), and this eval only runs 3 model-calling steps --
    // short of the 6-step plateau the working-set unit tests exercise --
    // so its "recent calls:" block legitimately grows by one line per step
    // for the whole run (observed: [751, 801, 820], a 69-token spread).
    // 20 predates that growth; widen enough to absorb it while still
    // catching the append-only-style growth (hundreds of tokens per step)
    // this assertion exists to distinguish `statie` from `baseline` on.
    assert!(
        max - min <= 100,
        "statie prompt should stay flat: {statie_tokens:?}"
    );

    let baseline_tokens: Vec<u64> = baseline["prompt_tokens"]
        .as_array()
        .expect("baseline.prompt_tokens should be an array")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(
        baseline_tokens.len(),
        3,
        "baseline.prompt_tokens: {baseline_tokens:?}"
    );
    // The terminal `done` step short-circuits before calling the model, so it
    // records no prompt; the three model-calling steps must show the
    // append-only baseline's prompt growing strictly with every entry.
    for w in baseline_tokens.windows(2) {
        assert!(
            w[1] > w[0],
            "baseline.prompt_tokens should be strictly increasing across model-calling steps: {baseline_tokens:?}"
        );
    }

    let statie_total = statie["total_prompt_tokens"].as_u64().unwrap();
    let baseline_total = baseline["total_prompt_tokens"].as_u64().unwrap();
    assert!(
        baseline_total > statie_total,
        "baseline_total={baseline_total} statie_total={statie_total}"
    );

    assert_eq!(statie["success"], serde_json::json!(true));
    assert_eq!(baseline["success"], serde_json::json!(true));
    assert_eq!(statie["steps"], serde_json::json!(4));
    assert_eq!(baseline["steps"], serde_json::json!(4));

    // Server-reported usage: 3 of the 4 steps in each harness call the model
    // with scripted usage (100+110+120 = 330 prompt tokens); the terminal
    // `done` step reports none.
    assert_eq!(
        statie["server_total_prompt_tokens"],
        serde_json::json!(330),
        "statie: {statie}"
    );
    assert_eq!(
        statie["server_total_completion_tokens"],
        serde_json::json!(33),
        "statie: {statie}"
    );
    assert_eq!(
        baseline["server_total_prompt_tokens"],
        serde_json::json!(330),
        "baseline: {baseline}"
    );
    let statie_server_tokens: Vec<u64> = statie["server_prompt_tokens"]
        .as_array()
        .expect("statie.server_prompt_tokens should be an array")
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(statie_server_tokens, vec![100, 110, 120]);
    assert_eq!(statie["server_max_prompt_tokens"], serde_json::json!(120));
}

/// The non-JSON (table) output uses the `est_*`/`srv_*` headers, with `-` in
/// the server columns when the backend reports no usage (the plain `mock`
/// backend here, with no scripted usage).
#[test]
fn eval_toy_table_has_srv_headers_and_dash_for_no_usage() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    let mock_script = serde_json::json!([
        {
            "content": serde_json::json!({
                "patch": {"counter": 1, "recent_summary": "did step 1"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hello 1"}},
                "ack_inbox": [1],
            }).to_string()
        },
        {
            "content": serde_json::json!({
                "patch": {"counter": 1},
                "action": {"type": "done"},
            }).to_string()
        },
    ]);

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": mock_script,
        },
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "eval",
            "--task",
            "toy",
            "--steps",
            "1",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );

    assert!(stdout.contains("srv_total"), "stdout={stdout}");
    assert!(stdout.contains("srv_max"), "stdout={stdout}");
    assert!(stdout.contains("srv_growth"), "stdout={stdout}");
    assert!(stdout.contains("srv_completion"), "stdout={stdout}");
    assert!(stdout.contains("est_total"), "stdout={stdout}");
    assert!(
        stdout.contains(" - "),
        "expected `-` in a server column: {stdout}"
    );
}
