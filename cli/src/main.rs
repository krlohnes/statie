//! `statie`: the binary. Config loading, session wiring, and the `statie`
//! commands (ARCHITECTURE.md "Workspace layout" / "Configuration").

mod coding;
mod config;
mod eval;
mod metrics;
mod toy;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use statie_augment::{RepoBm25, RepoBm25Options, SessionLogBm25};
use statie_core::{SandboxRoot, Session, SessionConfig, TiktokenTokenizer, TokenBudget};
use statie_tui::{HeadlessSummary, RunOptions, SkillRegistry};

use crate::coding::CodingRegistry;
use crate::config::Config;
use crate::toy::{ToyRegistry, toy_tool_set};

#[derive(Parser)]
#[command(
    name = "statie",
    version,
    about = "A mutable-state, interactive coding harness for local LLMs"
)]
struct Cli {
    /// Path to the config file. Overrides $STATIE_CONFIG and the default
    /// search locations.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(flatten)]
    run: RunArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Seed the first message of the session. Combined with -p/--prompt,
    /// this becomes the first message and the -p values follow it in order.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Run headlessly: seed a user message, drive the session to
    /// completion (or the step cap), print the transcript, and exit.
    /// Repeatable; each occurrence becomes one seeded user message, in
    /// order.
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    prompts: Vec<String>,

    /// Continue the existing session in `<sandbox>/.statie/` instead of
    /// archiving it and starting fresh (the default).
    #[arg(short, long)]
    resume: bool,

    /// Which skill to run: `router`, `plan`, `implement`, `review`
    /// (all share the coding workspace) or `toy`. Defaults to `router` when
    /// not given. When resuming (`-r`), an explicit `--skill` always wins
    /// over the session's stored activation; without it, resume starts back
    /// in the skill the session was parked in.
    #[arg(long)]
    skill: Option<String>,

    /// Force the in-memory store + MemoryLog; no files written.
    #[arg(long)]
    memory: bool,

    /// Operator-authored JSON merge patch applied directly to the workspace
    /// before the first step (after resume replay, if any), bypassing the
    /// model entirely. Repeatable; each occurrence is applied in order.
    /// Invalid JSON on any occurrence exits with status 2 before anything
    /// runs.
    #[arg(long = "patch", value_name = "JSON")]
    patches: Vec<String>,

    /// Target counter value for the `toy` skill.
    #[arg(long, default_value_t = 10)]
    toy_target: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Config file management.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Build the model backend and print its capabilities (and, for
    /// ollama, whether the configured model is present on the server).
    Check,
    /// Compare statie's bounded-workspace prompt against an append-only
    /// baseline harness on the same task (milestone 7).
    Eval {
        /// Which task to run: `toy` or `coding`.
        #[arg(long, default_value = "toy")]
        task: String,
        /// Target counter value for the `toy` task.
        #[arg(long, default_value_t = 10)]
        steps: u64,
        /// Print the result as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Print the full transcript of both runs to stderr.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Read-only summary of one session log: committed steps, rejections,
    /// tool errors, edits landed, and more, plus breakdowns of rejection
    /// and tool-error reasons.
    Metrics {
        /// Path to the session log (JSONL). Defaults to
        /// `.statie/session.jsonl` under the current directory.
        path: Option<PathBuf>,
        /// Print the summary as one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Write the default config to disk.
    Init {
        /// Write to ~/.config/statie/config.json instead of
        /// ./.statie/config.json.
        #[arg(long)]
        global: bool,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective config (defaults merged with the resolved file)
    /// as pretty JSON, plus the path it came from.
    Show,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Config {
            command: ConfigCommand::Init { global, force },
        }) => cmd_config_init(global, force),
        Some(Command::Config {
            command: ConfigCommand::Show,
        }) => cmd_config_show(cli.config),
        Some(Command::Check) => cmd_check(cli.config).await,
        Some(Command::Eval {
            task,
            steps,
            json,
            verbose,
        }) => eval::cmd_eval(cli.config, task, steps, json, verbose).await,
        Some(Command::Metrics { path, json }) => metrics::cmd_metrics(path, json),
        None => cmd_run(cli.config, cli.run).await,
    }
}

fn cmd_config_init(global: bool, force: bool) -> anyhow::Result<()> {
    let path = if global {
        config::global_config_path()
            .ok_or_else(|| anyhow::anyhow!("could not determine the user config directory"))?
    } else {
        config::local_config_path()
    };

    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }

    if global {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
    } else {
        Config::default().ensure_statie_dir()?;
    }

    let default_cfg = Config::default();
    let json = serde_json::to_string_pretty(&default_cfg)?;
    std::fs::write(&path, json)?;

    println!("wrote {}", path.display());
    Ok(())
}

fn cmd_config_show(cli_flag: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, path) = config::load(cli_flag)?;
    let json = serde_json::to_string_pretty(&cfg)?;
    println!("{json}");
    match path {
        Some(p) => println!("# source: {}", p.display()),
        None => println!("# source: defaults"),
    }
    Ok(())
}

async fn cmd_check(cli_flag: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, _path) = config::load(cli_flag)?;
    let model = cfg.build_model()?;
    let caps = model.capabilities();
    println!("backend: {}", cfg.model.backend);
    println!("native_thinking: {}", caps.native_thinking);
    println!("schema_constrained: {}", caps.schema_constrained);
    println!("prefix_cache: {}", caps.prefix_cache);

    if cfg.model.backend == "ollama" {
        let url = format!("{}/api/tags", cfg.model.base_url.trim_end_matches('/'));
        match reqwest::get(&url).await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let present = body
                    .get("models")
                    .and_then(|m| m.as_array())
                    .map(|models| {
                        models.iter().any(|m| {
                            m.get("name").and_then(|n| n.as_str()) == Some(cfg.model.model.as_str())
                                || m.get("model").and_then(|n| n.as_str())
                                    == Some(cfg.model.model.as_str())
                        })
                    })
                    .unwrap_or(false);
                println!("ollama server: reachable at {}", cfg.model.base_url);
                println!("model `{}` present: {}", cfg.model.model, present);
            }
            Ok(resp) => {
                println!("ollama server: reachable but returned {}", resp.status());
            }
            Err(e) => {
                println!("ollama server: unreachable at {} ({e})", cfg.model.base_url);
            }
        }
    }

    Ok(())
}

/// Initializes `tracing_subscriber` from `$STATIE_LOG`/`$RUST_LOG`. In
/// headless mode, logs go to stderr. In TUI mode, logs go to
/// `.statie/statie.log` so the terminal is not corrupted by interleaved
/// log lines.
fn init_tracing(headless: bool, sandbox_root: &std::path::Path) {
    let filter = std::env::var("STATIE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_new(filter.clone())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    if headless {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .try_init();
    } else {
        let log_dir = sandbox_root.join(".statie");
        if std::fs::create_dir_all(&log_dir).is_ok() {
            let log_path = log_dir.join("statie.log");
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_ansi(false)
                    .try_init();
            }
        }
    }
}

async fn cmd_run(cli_flag: Option<PathBuf>, run: RunArgs) -> anyhow::Result<()> {
    let RunArgs {
        prompt,
        prompts,
        resume,
        skill,
        memory,
        patches,
        toy_target,
    } = run;

    // Parsed and validated before anything else runs (config load, sandbox
    // setup, session activation): a malformed `--patch` value is a usage
    // error, not a runtime one, so it exits 2 like clap's own argument
    // errors rather than 1 like a failed run.
    let patches = match parse_patches(&patches) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    // Headless mode is exactly "one or more -p/--prompt values". A bare
    // positional PROMPT alone seeds the TUI's first message rather than
    // triggering headless mode.
    let headless = !prompts.is_empty();
    let messages = initial_messages(prompt, prompts);

    let (cfg, path) = config::load(cli_flag)?;
    let root = SandboxRoot::new(&cfg.sandbox_root)?;

    if !memory {
        cfg.ensure_statie_dir()?;
    }

    init_tracing(headless, root.path());

    if !memory {
        if resume {
            if cfg.has_existing_session() {
                let msg = format!(
                    "resuming session at {}",
                    cfg.resolved_store_path().display()
                );
                eprintln!("{msg}");
                tracing::info!("{msg}");
            }
        } else if let Some(dir) = cfg.archive_session()? {
            let msg = format!("archived previous session to {}", dir.display());
            eprintln!("{msg}");
            tracing::info!("{msg}");
        }
    }

    // An explicit `--skill` always wins over a resumed session's stored
    // activation; absent one, fall back to `router` as before (the driver
    // resolves the stored activation itself when `resume` is set).
    let skill_explicit = skill.is_some();
    let skill_set = skill.unwrap_or_else(|| "router".to_string());

    match skill_set.as_str() {
        "toy" => {
            run_toy(
                cfg, path, root, headless, messages, memory, toy_target, resume, patches,
            )
            .await
        }
        "router" | "plan" | "implement" | "review" => {
            run_coding(
                cfg,
                path,
                root,
                &skill_set,
                headless,
                messages,
                memory,
                resume,
                skill_explicit,
                patches,
            )
            .await
        }
        other => anyhow::bail!(
            "unknown skill `{other}` (expected one of: router, plan, implement, review, toy)"
        ),
    }
}

/// Parses each `--patch` value as a JSON object, in order. On the first
/// invalid one, returns a message naming its position (1-based, matching
/// how a user would count repeated flags) and the parse error, so `cmd_run`
/// can print it and exit 2 before any session work happens.
fn parse_patches(raw: &[String]) -> Result<Vec<serde_json::Value>, String> {
    raw.iter()
        .enumerate()
        .map(|(i, s)| {
            serde_json::from_str::<serde_json::Value>(s)
                .map_err(|e| format!("--patch #{}: invalid JSON: {e}", i + 1))
        })
        .collect()
}

/// Combines the positional PROMPT (if any) with the `-p`/`--prompt` values
/// into the ordered list of initial user messages: positional first, then
/// the `-p` values in the order given.
fn initial_messages(prompt: Option<String>, prompts: Vec<String>) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    if let Some(p) = prompt {
        messages.push(p);
    }
    messages.extend(prompts);
    messages
}

/// Builds the startup banner lines: the config source, the model backend
/// being used, the sandbox root, whether this run is resuming or fresh, and
/// a usage hint. Sent as `UiEvent::Info` lines before the first activation
/// so a user in an unconfigured directory sees *why* the first model call
/// might fail, instead of the TUI silently dying on their first Enter (the
/// bug this banner accompanies a fix for).
fn build_banner(
    cfg: &Config,
    path: &Option<PathBuf>,
    root: &SandboxRoot,
    resume: bool,
) -> Vec<String> {
    let config_line = match path {
        Some(p) => format!("config: {}", p.display()),
        None => "config: defaults (no config file found; run `statie config init` and set model.base_url)".to_string(),
    };
    let model_line = if cfg.model.backend == "mock" {
        "model: mock".to_string()
    } else {
        format!(
            "model: {} {} @ {}",
            cfg.model.backend, cfg.model.model, cfg.model.base_url
        )
    };
    let sandbox_line = format!("sandbox: {}", root.path().display());
    let session_line = if resume {
        format!("session: resuming {}", cfg.resolved_store_path().display())
    } else {
        "session: fresh".to_string()
    };
    vec![
        config_line,
        model_line,
        sandbox_line,
        session_line,
        "type a message and press Enter; /help for commands; ctrl-c to quit".to_string(),
    ]
}

/// Builds a `Session<CodingWorkspace>` wired exactly like the toy path
/// (config store/log/augmenters/budgets, `statie_tools::default_tools`)
/// and dispatches to `run_tui`/`run_headless`, starting in `skill`.
#[allow(clippy::too_many_arguments)]
async fn run_coding(
    cfg: Config,
    path: Option<PathBuf>,
    root: SandboxRoot,
    skill: &str,
    headless: bool,
    messages: Vec<String>,
    memory: bool,
    resume: bool,
    skill_explicit: bool,
    patches: Vec<serde_json::Value>,
) -> anyhow::Result<()> {
    let banner = build_banner(&cfg, &path, &root, resume);
    let model = cfg.build_model()?;
    let tools = statie_tools::default_tools(&cfg.tools);
    let tokenizer = Box::new(TiktokenTokenizer);
    let maintainer = cfg.build_maintainer()?;

    let session_config = SessionConfig {
        budgets: cfg.effective_budgets(),
        retries: cfg.retries,
        root,
    };

    let (store, log) = if memory {
        (
            cfg.build_memory_store::<statie_skills::CodingWorkspace>(),
            cfg.build_memory_log(),
        )
    } else {
        (
            cfg.build_store::<statie_skills::CodingWorkspace>()?,
            cfg.build_log()?,
        )
    };

    let mut session = Session::new(
        store,
        Arc::clone(&log),
        model.into(),
        tools,
        tokenizer,
        session_config,
    );
    if let Some(m) = maintainer {
        session = session.with_maintainer(m);
    }
    session = session.with_working_set(cfg.working_set.into());
    session = session.with_guards(cfg.guards);

    if cfg.augmenters.repo_bm25.enabled {
        let repo_bm25 = RepoBm25::build(&session.config().root, RepoBm25Options::default())?;
        session = session.with_augmenter(
            Box::new(repo_bm25),
            TokenBudget(cfg.augmenters.repo_bm25.budget),
        );
    }
    if cfg.augmenters.session_log_bm25.enabled {
        let session_log_bm25 = SessionLogBm25::new(Arc::clone(&log), Vec::new(), 8);
        session = session.with_augmenter(
            Box::new(session_log_bm25),
            TokenBudget(cfg.augmenters.session_log_bm25.budget),
        );
    }

    let registry: Arc<dyn SkillRegistry<statie_skills::CodingWorkspace>> =
        Arc::new(CodingRegistry::with_guards(cfg.guards));

    let opts = RunOptions {
        initial_skill: skill.to_string(),
        initial_messages: messages,
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner,
        resume,
        replay_entries: 40,
        skill_explicit,
        patches,
    };

    if headless {
        let mut stdout = std::io::stdout();
        let summary: HeadlessSummary =
            statie_tui::run_headless(session, registry, opts, &mut stdout).await?;
        println!(
            "steps={} outcome={} prompt_tokens={:?}",
            summary.steps, summary.final_outcome, summary.prompt_tokens
        );
        Ok(())
    } else {
        if let Err(e) = statie_tui::run_tui(session, registry, opts).await {
            anyhow::bail!("statie exited with an error: {e}");
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_toy(
    cfg: Config,
    path: Option<PathBuf>,
    root: SandboxRoot,
    headless: bool,
    messages: Vec<String>,
    memory: bool,
    toy_target: u64,
    resume: bool,
    patches: Vec<serde_json::Value>,
) -> anyhow::Result<()> {
    let banner = build_banner(&cfg, &path, &root, resume);
    let model = cfg.build_model()?;
    let tools = toy_tool_set();
    let tokenizer = Box::new(TiktokenTokenizer);

    let session_config = SessionConfig {
        budgets: cfg.budget.into(),
        retries: cfg.retries,
        root,
    };

    let (store, log) = if memory {
        (
            cfg.build_memory_store::<statie_skills::ToyWorkspace>(),
            cfg.build_memory_log(),
        )
    } else {
        (
            cfg.build_store::<statie_skills::ToyWorkspace>()?,
            cfg.build_log()?,
        )
    };

    let mut session = Session::new(
        store,
        Arc::clone(&log),
        model.into(),
        tools,
        tokenizer,
        session_config,
    );

    if cfg.augmenters.repo_bm25.enabled {
        let repo_bm25 = RepoBm25::build(&session.config().root, RepoBm25Options::default())?;
        session = session.with_augmenter(
            Box::new(repo_bm25),
            TokenBudget(cfg.augmenters.repo_bm25.budget),
        );
    }
    if cfg.augmenters.session_log_bm25.enabled {
        let session_log_bm25 = SessionLogBm25::new(Arc::clone(&log), Vec::new(), 8);
        session = session.with_augmenter(
            Box::new(session_log_bm25),
            TokenBudget(cfg.augmenters.session_log_bm25.budget),
        );
    }

    let registry: Arc<dyn SkillRegistry<statie_skills::ToyWorkspace>> =
        Arc::new(ToyRegistry::new(toy_target));

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: messages,
        max_autonomous_steps: cfg.max_autonomous_steps,
        banner,
        resume,
        replay_entries: 40,
        skill_explicit: false,
        patches,
    };

    if headless {
        let mut stdout = std::io::stdout();
        let summary: HeadlessSummary =
            statie_tui::run_headless(session, registry, opts, &mut stdout).await?;
        println!(
            "steps={} outcome={} prompt_tokens={:?}",
            summary.steps, summary.final_outcome, summary.prompt_tokens
        );
        Ok(())
    } else {
        if let Err(e) = statie_tui::run_tui(session, registry, opts).await {
            anyhow::bail!("statie exited with an error: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod run_args_tests {
    use super::*;

    #[test]
    fn positional_alone_seeds_one_message() {
        assert_eq!(
            initial_messages(Some("hi".to_string()), Vec::new()),
            vec!["hi".to_string()]
        );
    }

    #[test]
    fn prompts_alone_seed_in_order() {
        assert_eq!(
            initial_messages(None, vec!["a".to_string(), "b".to_string()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn positional_then_prompts_are_ordered_positional_first() {
        assert_eq!(
            initial_messages(
                Some("first".to_string()),
                vec!["second".to_string(), "third".to_string()]
            ),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn neither_yields_no_messages() {
        assert_eq!(initial_messages(None, Vec::new()), Vec::<String>::new());
    }
}
