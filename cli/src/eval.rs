//! `statie eval`: milestone 7's append-only baseline comparison
//! (statie-spec.md §10, "Token and success metrics on the same tasks").
//!
//! Builds two fresh sessions from the same config for the chosen task:
//! **statie** (the config's own budgets and augmenters disabled) and
//! **baseline** (identical session/skill/tools/model, but with
//! `TranscriptAugmenter` added at an effectively unbounded budget, and
//! `budgets.total`/`budgets.augmentation` raised to match -- modeling an
//! append-only messages array). Both run headlessly against the same mock
//! script, so this is fully offline and deterministic.

use std::io::Write;
use std::sync::Arc;

use statie_augment::TranscriptAugmenter;
use statie_core::{
    MemoryLog, SandboxRoot, Session, SessionConfig, SessionLog, TiktokenTokenizer, TokenBudget,
};
use statie_skills::ToyWorkspace;
use statie_tui::{HeadlessSummary, RunOptions, SkillRegistry};

use crate::coding::CodingRegistry;
use crate::config::Config;
use crate::toy::{ToyRegistry, toy_tool_set};

/// A huge budget standing in for "unbounded" -- big enough that the mock
/// scripts used in tests and manual runs never trip `OverBudget`, while
/// still being a concrete, honest number rather than `usize::MAX`.
const UNBOUNDED_BUDGET: usize = 1_000_000;

/// Per-harness metrics collected from one `HeadlessSummary`. `prompt_tokens`
/// et al. are the local `TiktokenTokenizer` (cl100k) estimate; `server_*`
/// are the backend's own reported counts (Ollama's `prompt_eval_count`/
/// `eval_count`, or an OpenAI-compatible server's `usage`), present only for
/// model calls that reported them (empty for the `mock` backend unless the
/// script attaches usage).
#[derive(Debug, Clone, serde::Serialize)]
struct HarnessMetrics {
    steps: usize,
    success: bool,
    prompt_tokens: Vec<usize>,
    total_prompt_tokens: usize,
    max_prompt_tokens: usize,
    /// `last - first` prompt token count; 0 for an empty or flat series.
    growth: i64,
    server_prompt_tokens: Vec<u64>,
    server_total_prompt_tokens: u64,
    server_max_prompt_tokens: u64,
    server_total_completion_tokens: u64,
    /// `last - first` server-reported prompt token count; 0 for an empty or
    /// flat series.
    server_growth: i64,
}

impl HarnessMetrics {
    fn from_summary(summary: &HeadlessSummary, success: bool) -> Self {
        let prompt_tokens = summary.prompt_tokens.clone();
        let total_prompt_tokens: usize = prompt_tokens.iter().sum();
        let max_prompt_tokens = prompt_tokens.iter().copied().max().unwrap_or(0);
        let growth = match (prompt_tokens.first(), prompt_tokens.last()) {
            (Some(first), Some(last)) => *last as i64 - *first as i64,
            _ => 0,
        };

        let server_prompt_tokens = summary.server_prompt_tokens.clone();
        let server_total_prompt_tokens: u64 = server_prompt_tokens.iter().sum();
        let server_max_prompt_tokens = server_prompt_tokens.iter().copied().max().unwrap_or(0);
        let server_total_completion_tokens: u64 = summary.server_completion_tokens.iter().sum();
        let server_growth = match (server_prompt_tokens.first(), server_prompt_tokens.last()) {
            (Some(first), Some(last)) => *last as i64 - *first as i64,
            _ => 0,
        };

        HarnessMetrics {
            steps: summary.steps,
            success,
            prompt_tokens,
            total_prompt_tokens,
            max_prompt_tokens,
            growth,
            server_prompt_tokens,
            server_total_prompt_tokens,
            server_max_prompt_tokens,
            server_total_completion_tokens,
            server_growth,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct EvalResult {
    statie: HarnessMetrics,
    baseline: HarnessMetrics,
}

pub async fn cmd_eval(
    cli_flag: Option<std::path::PathBuf>,
    task: String,
    steps: u64,
    json: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let (cfg, _path) = crate::config::load(cli_flag)?;

    let filter = std::env::var("STATIE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "warn".to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .try_init();

    let result = match task.as_str() {
        "toy" => run_toy_eval(&cfg, steps, verbose).await?,
        "coding" => run_coding_eval(&cfg, verbose).await?,
        other => anyhow::bail!("unknown eval task `{other}` (expected one of: toy, coding)"),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_table(&result);
    }

    Ok(())
}

/// `est_*` columns are the local cl100k (`TiktokenTokenizer`) estimate;
/// `srv_*` columns are the backend's own reported token counts (Ollama's
/// `prompt_eval_count`/`eval_count`, or an OpenAI-compatible server's
/// `usage`) — `-` when the backend reported none (e.g. the `mock` backend
/// with no scripted usage). `srv_max` is the number to compare against
/// `model.num_ctx`.
fn print_table(result: &EvalResult) {
    println!(
        "{:<10} {:>6} {:>8} {:>10} {:>8} {:>11} {:>9} {:>8} {:>11} {:>15}",
        "harness",
        "steps",
        "success",
        "est_total",
        "est_max",
        "est_growth",
        "srv_total",
        "srv_max",
        "srv_growth",
        "srv_completion"
    );
    for (name, m) in [("statie", &result.statie), ("baseline", &result.baseline)] {
        let has_server = !m.server_prompt_tokens.is_empty();
        let srv_total = if has_server {
            m.server_total_prompt_tokens.to_string()
        } else {
            "-".to_string()
        };
        let srv_max = if has_server {
            m.server_max_prompt_tokens.to_string()
        } else {
            "-".to_string()
        };
        let srv_growth = if has_server {
            m.server_growth.to_string()
        } else {
            "-".to_string()
        };
        let srv_completion = if has_server {
            m.server_total_completion_tokens.to_string()
        } else {
            "-".to_string()
        };
        println!(
            "{:<10} {:>6} {:>8} {:>10} {:>8} {:>11} {:>9} {:>8} {:>11} {:>15}",
            name,
            m.steps,
            m.success,
            m.total_prompt_tokens,
            m.max_prompt_tokens,
            m.growth,
            srv_total,
            srv_max,
            srv_growth,
            srv_completion,
        );
    }
}

/// A sink for a headless run's plain-text transcript: `/dev/null`-style
/// discard, or stderr under `--verbose`.
fn transcript_sink(verbose: bool) -> Box<dyn Write + Send> {
    if verbose {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::sink())
    }
}

/// A fresh, isolated sandbox root for one eval run.
fn fresh_sandbox() -> anyhow::Result<(tempfile::TempDir, SandboxRoot)> {
    let dir = tempfile::tempdir()?;
    let root = SandboxRoot::new(dir.path())?;
    Ok((dir, root))
}

/// Config for the "statie" harness: the loaded config's own budgets,
/// augmenters disabled (a clean, in-memory comparison -- the point is the
/// bounded-workspace prompt shape, not BM25 retrieval quality).
fn statie_session_config(cfg: &Config, root: SandboxRoot) -> SessionConfig {
    SessionConfig {
        budgets: cfg.budget.into(),
        retries: cfg.retries,
        root,
    }
}

/// Config for the "baseline" harness: same retries, but `total` and
/// `augmentation` raised to `UNBOUNDED_BUDGET` so the append-only
/// `TranscriptAugmenter` is never rejected as over budget.
fn baseline_session_config(cfg: &Config, root: SandboxRoot) -> SessionConfig {
    let mut budgets: statie_core::Budgets = cfg.budget.into();
    budgets.total = UNBOUNDED_BUDGET;
    budgets.augmentation = UNBOUNDED_BUDGET;
    SessionConfig {
        budgets,
        retries: cfg.retries,
        root,
    }
}

async fn run_toy_eval(cfg: &Config, steps: u64, verbose: bool) -> anyhow::Result<EvalResult> {
    // -- statie harness --
    let (_dir_a, root_a) = fresh_sandbox()?;
    let model_a = cfg.build_model()?;
    let tools_a = toy_tool_set();
    let log_a: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session_a = Session::new(
        cfg.build_memory_store::<ToyWorkspace>(),
        Arc::clone(&log_a),
        model_a,
        tools_a,
        Box::new(TiktokenTokenizer),
        statie_session_config(cfg, root_a),
    );
    let registry_a: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(ToyRegistry::new(steps));
    let opts_a = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["start".to_string()],
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let mut sink_a = transcript_sink(verbose);
    let summary_a = statie_tui::run_headless(session_a, registry_a, opts_a, &mut sink_a).await?;
    let success_a = {
        let state = &summary_a.final_state;
        let counter = state
            .pointer("/agent/counter")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        counter >= steps && summary_a.final_outcome == "done"
    };

    // -- baseline harness --
    let (_dir_b, root_b) = fresh_sandbox()?;
    let model_b = cfg.build_model()?;
    let tools_b = toy_tool_set();
    let log_b: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let mut session_b = Session::new(
        cfg.build_memory_store::<ToyWorkspace>(),
        Arc::clone(&log_b),
        model_b,
        tools_b,
        Box::new(TiktokenTokenizer),
        baseline_session_config(cfg, root_b),
    );
    session_b = session_b.with_augmenter(
        Box::new(TranscriptAugmenter::new(Arc::clone(&log_b))),
        TokenBudget(UNBOUNDED_BUDGET),
    );
    let registry_b: Arc<dyn SkillRegistry<ToyWorkspace>> = Arc::new(ToyRegistry::new(steps));
    let opts_b = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: vec!["start".to_string()],
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let mut sink_b = transcript_sink(verbose);
    let summary_b = statie_tui::run_headless(session_b, registry_b, opts_b, &mut sink_b).await?;
    let success_b = {
        let state = &summary_b.final_state;
        let counter = state
            .pointer("/agent/counter")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        counter >= steps && summary_b.final_outcome == "done"
    };

    Ok(EvalResult {
        statie: HarnessMetrics::from_summary(&summary_a, success_a),
        baseline: HarnessMetrics::from_summary(&summary_b, success_b),
    })
}

const CODING_TASK_MESSAGE: &str = "Create a file named hello.txt in the repository root containing exactly the line: hello statie";

fn coding_success(sandbox: &std::path::Path, outcome: &str) -> bool {
    if outcome != "done" {
        return false;
    }
    let path = sandbox.join("hello.txt");
    match std::fs::read_to_string(&path) {
        Ok(content) => content.trim() == "hello statie",
        Err(_) => false,
    }
}

async fn run_coding_eval(cfg: &Config, verbose: bool) -> anyhow::Result<EvalResult> {
    // -- statie harness --
    let (dir_a, root_a) = fresh_sandbox()?;
    let model_a = cfg.build_model()?;
    let tools_a = statie_tools::default_tools(&cfg.tools);
    let log_a: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session_a = Session::new(
        cfg.build_memory_store::<statie_skills::CodingWorkspace>(),
        Arc::clone(&log_a),
        model_a,
        tools_a,
        Box::new(TiktokenTokenizer),
        statie_session_config(cfg, root_a),
    );
    let registry_a: Arc<dyn SkillRegistry<statie_skills::CodingWorkspace>> =
        Arc::new(CodingRegistry::new());
    let opts_a = RunOptions {
        initial_skill: "router".to_string(),
        initial_messages: vec![CODING_TASK_MESSAGE.to_string()],
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let mut sink_a = transcript_sink(verbose);
    let summary_a = statie_tui::run_headless(session_a, registry_a, opts_a, &mut sink_a).await?;
    let success_a = coding_success(dir_a.path(), &summary_a.final_outcome);

    // -- baseline harness --
    let (dir_b, root_b) = fresh_sandbox()?;
    let model_b = cfg.build_model()?;
    let tools_b = statie_tools::default_tools(&cfg.tools);
    let log_b: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let mut session_b = Session::new(
        cfg.build_memory_store::<statie_skills::CodingWorkspace>(),
        Arc::clone(&log_b),
        model_b,
        tools_b,
        Box::new(TiktokenTokenizer),
        baseline_session_config(cfg, root_b),
    );
    session_b = session_b.with_augmenter(
        Box::new(TranscriptAugmenter::new(Arc::clone(&log_b))),
        TokenBudget(UNBOUNDED_BUDGET),
    );
    let registry_b: Arc<dyn SkillRegistry<statie_skills::CodingWorkspace>> =
        Arc::new(CodingRegistry::new());
    let opts_b = RunOptions {
        initial_skill: "router".to_string(),
        initial_messages: vec![CODING_TASK_MESSAGE.to_string()],
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner: Vec::new(),
        resume: false,
        replay_entries: 40,
        skill_explicit: false,
    };
    let mut sink_b = transcript_sink(verbose);
    let summary_b = statie_tui::run_headless(session_b, registry_b, opts_b, &mut sink_b).await?;
    let success_b = coding_success(dir_b.path(), &summary_b.final_outcome);

    Ok(EvalResult {
        statie: HarnessMetrics::from_summary(&summary_a, success_a),
        baseline: HarnessMetrics::from_summary(&summary_b, success_b),
    })
}
