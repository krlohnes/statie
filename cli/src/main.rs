//! `statie`: the binary. Config loading, session wiring, and the `statie`
//! commands (ARCHITECTURE.md "Workspace layout" / "Configuration").

mod coding;
mod config;
mod eval;
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
#[command(name = "statie", about = "A mutable-state, interactive coding harness for local LLMs")]
struct Cli {
    /// Path to the config file. Overrides $STATIE_CONFIG and the default
    /// search locations.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a session.
    Run {
        /// Which skill to run: `router`, `plan`, `implement`, `review`
        /// (all share the coding workspace) or `toy`.
        #[arg(long, default_value = "router")]
        skill: String,
        /// Run without the TUI, driving the loop to completion (or the
        /// step cap) and printing a summary line.
        #[arg(long)]
        headless: bool,
        /// Seed the session with one or more initial user messages.
        #[arg(long = "message")]
        messages: Vec<String>,
        /// Force the in-memory store + MemoryLog; no files written.
        #[arg(long)]
        memory: bool,
        /// Target counter value for the `toy` skill.
        #[arg(long, default_value_t = 10)]
        toy_target: u64,
    },
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
        Command::Config {
            command: ConfigCommand::Init { global, force },
        } => cmd_config_init(global, force),
        Command::Config {
            command: ConfigCommand::Show,
        } => cmd_config_show(cli.config),
        Command::Check => cmd_check(cli.config).await,
        Command::Run {
            skill,
            headless,
            messages,
            memory,
            toy_target,
        } => cmd_run(cli.config, skill, headless, messages, memory, toy_target).await,
        Command::Eval {
            task,
            steps,
            json,
            verbose,
        } => eval::cmd_eval(cli.config, task, steps, json, verbose).await,
    }
}

fn cmd_config_init(global: bool, force: bool) -> anyhow::Result<()> {
    let path = if global {
        config::global_config_path().ok_or_else(|| anyhow::anyhow!("could not determine the user config directory"))?
    } else {
        config::local_config_path()
    };

    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
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
                                || m.get("model").and_then(|n| n.as_str()) == Some(cfg.model.model.as_str())
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
            if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_ansi(false)
                    .try_init();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_run(
    cli_flag: Option<PathBuf>,
    skill_set: String,
    headless: bool,
    messages: Vec<String>,
    memory: bool,
    toy_target: u64,
) -> anyhow::Result<()> {
    let (cfg, _path) = config::load(cli_flag)?;
    let root = SandboxRoot::new(&cfg.sandbox_root)?;

    init_tracing(headless, root.path());

    match skill_set.as_str() {
        "toy" => run_toy(cfg, root, headless, messages, memory, toy_target).await,
        "router" | "plan" | "implement" | "review" => {
            run_coding(cfg, root, &skill_set, headless, messages, memory).await
        }
        other => anyhow::bail!(
            "unknown skill `{other}` (expected one of: router, plan, implement, review, toy)"
        ),
    }
}

/// Builds a `Session<CodingWorkspace>` wired exactly like the toy path
/// (config store/log/augmenters/budgets, `statie_tools::default_tools`)
/// and dispatches to `run_tui`/`run_headless`, starting in `skill`.
async fn run_coding(
    cfg: Config,
    root: SandboxRoot,
    skill: &str,
    headless: bool,
    messages: Vec<String>,
    memory: bool,
) -> anyhow::Result<()> {
    let model = cfg.build_model()?;
    let tools = statie_tools::default_tools(&cfg.tools);
    let tokenizer = Box::new(TiktokenTokenizer);

    let session_config = SessionConfig {
        budgets: cfg.budget.into(),
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

    let mut session = Session::new(store, Arc::clone(&log), model, tools, tokenizer, session_config);

    if cfg.augmenters.repo_bm25.enabled {
        let repo_bm25 = RepoBm25::build(&session.config().root, RepoBm25Options::default())?;
        session = session.with_augmenter(Box::new(repo_bm25), TokenBudget(cfg.augmenters.repo_bm25.budget));
    }
    if cfg.augmenters.session_log_bm25.enabled {
        let session_log_bm25 = SessionLogBm25::new(Arc::clone(&log), Vec::new(), 8);
        session = session.with_augmenter(
            Box::new(session_log_bm25),
            TokenBudget(cfg.augmenters.session_log_bm25.budget),
        );
    }

    let registry: Arc<dyn SkillRegistry<statie_skills::CodingWorkspace>> = Arc::new(CodingRegistry::new());

    let opts = RunOptions {
        initial_skill: skill.to_string(),
        initial_messages: messages,
        max_autonomous_steps: cfg.max_autonomous_steps,
    };

    if headless {
        let mut stdout = std::io::stdout();
        let summary: HeadlessSummary = statie_tui::run_headless(session, registry, opts, &mut stdout).await?;
        println!(
            "steps={} outcome={} prompt_tokens={:?}",
            summary.steps, summary.final_outcome, summary.prompt_tokens
        );
        Ok(())
    } else {
        statie_tui::run_tui(session, registry, opts).await?;
        Ok(())
    }
}

async fn run_toy(
    cfg: Config,
    root: SandboxRoot,
    headless: bool,
    messages: Vec<String>,
    memory: bool,
    toy_target: u64,
) -> anyhow::Result<()> {
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
        (cfg.build_store::<statie_skills::ToyWorkspace>()?, cfg.build_log()?)
    };

    let mut session = Session::new(store, Arc::clone(&log), model, tools, tokenizer, session_config);

    if cfg.augmenters.repo_bm25.enabled {
        let repo_bm25 = RepoBm25::build(&session.config().root, RepoBm25Options::default())?;
        session = session.with_augmenter(Box::new(repo_bm25), TokenBudget(cfg.augmenters.repo_bm25.budget));
    }
    if cfg.augmenters.session_log_bm25.enabled {
        let session_log_bm25 = SessionLogBm25::new(Arc::clone(&log), Vec::new(), 8);
        session = session.with_augmenter(
            Box::new(session_log_bm25),
            TokenBudget(cfg.augmenters.session_log_bm25.budget),
        );
    }

    let registry: Arc<dyn SkillRegistry<statie_skills::ToyWorkspace>> = Arc::new(ToyRegistry::new(toy_target));

    let opts = RunOptions {
        initial_skill: "toy".to_string(),
        initial_messages: messages,
        max_autonomous_steps: cfg.max_autonomous_steps,
    };

    if headless {
        let mut stdout = std::io::stdout();
        let summary: HeadlessSummary = statie_tui::run_headless(session, registry, opts, &mut stdout).await?;
        println!(
            "steps={} outcome={} prompt_tokens={:?}",
            summary.steps, summary.final_outcome, summary.prompt_tokens
        );
        Ok(())
    } else {
        statie_tui::run_tui(session, registry, opts).await?;
        Ok(())
    }
}
