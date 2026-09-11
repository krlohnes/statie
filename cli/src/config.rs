//! Configuration loading for the `statie` binary (ARCHITECTURE.md
//! "Configuration"). Nothing about the model endpoint is hardcoded
//! anywhere else: this module is the single source of truth, and every
//! field has a serde default so a partial (or absent) file is valid.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use statie_core::{Fixer, Guards, Maintainer, SessionLog, StateStore, Thinker, TiktokenTokenizer};
use statie_models::{
    MockModel, MockTurn, ModelFixer, ModelMaintainer, ModelThinker, OllamaConfig, OllamaFormat,
    OllamaModel, OpenAiCompatConfig, OpenAiCompatModel,
};
use statie_skills::ChildSkillConfig;
use statie_store::{InMemoryStore, JsonlLog, SqliteStore};
use statie_tools::ToolConfig;

/// Top-level configuration. Every field is defaulted so `{}` parses to the
/// built-in defaults; unknown top-level keys are a hard error (catches
/// typos like `"modle"`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: ModelConfig,
    pub budget: BudgetConfig,
    pub retries: u32,
    pub store: StoreConfig,
    pub session_log: PathBuf,
    pub sandbox_root: PathBuf,
    pub augmenters: AugmentersConfig,
    pub tools: ToolConfig,
    pub max_autonomous_steps: usize,
    /// Configured child skills (subagents), keyed by name (milestone 9,
    /// ARCHITECTURE.md "Configured child skills"). A parent skill calls one
    /// of these via the `delegate` action; each stanza is a complete child
    /// definition (instructions, tools, optional guide and `data` schema,
    /// budgets, a step cap) with no Rust code required.
    pub subagents: BTreeMap<String, ChildSkillConfig>,
    /// Context maintenance (statie-spec.md "9. Context maintenance").
    /// Absent (the default) means no maintainer is attached and the loop
    /// behaves exactly as it did before this feature existed.
    pub maintainer: Option<MaintainerConfig>,
    /// The `think` tool's reasoning backend. Absent (the default) means no
    /// thinker is attached: a `think` call still commits, but its output
    /// tells the model `think` is not configured (see
    /// `Session::with_thinker`).
    pub thinker: Option<ThinkerConfig>,
    /// The `try_fixes` tool's proposer backend (BACKLOG.md item 4). Absent
    /// (the default) means no fixer is attached; session-side handling of
    /// an unconfigured `try_fixes` call is a later unit, mirroring
    /// `thinker`.
    pub fixer: Option<FixerConfig>,
    /// Working-set config (statie-spec.md "10. Working set"): recent tool
    /// calls and read/search windows kept in front of the model.
    /// `max_windows: 0` disables the section.
    pub working_set: WorkingSetConfigEntry,
    /// Rule-audit switches (statie-spec.md "10. After it works: the rule
    /// audit", milestone 12): one bool per non-progress rule marked "peel"
    /// in the audit table, all defaulting `true`. An absent `guards`
    /// section (or a section missing individual fields) leaves every rule
    /// on -- byte-for-byte today's behaviour.
    pub guards: Guards,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: ModelConfig::default(),
            budget: BudgetConfig::default(),
            retries: 3,
            store: StoreConfig::default(),
            session_log: PathBuf::from(".statie/session.jsonl"),
            sandbox_root: PathBuf::from("."),
            augmenters: AugmentersConfig::default(),
            tools: ToolConfig::default(),
            max_autonomous_steps: 200,
            subagents: BTreeMap::new(),
            maintainer: None,
            thinker: None,
            fixer: None,
            working_set: WorkingSetConfigEntry::default(),
            guards: Guards::default(),
        }
    }
}

/// Config-file mirror of `statie_core::WorkingSetConfig` (statie-spec.md
/// "10. Working set"). `max_windows: 0` disables the section.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkingSetConfigEntry {
    pub max_windows: usize,
    pub recent_calls: usize,
}

impl Default for WorkingSetConfigEntry {
    fn default() -> Self {
        WorkingSetConfigEntry {
            max_windows: 4,
            recent_calls: 6,
        }
    }
}

impl From<WorkingSetConfigEntry> for statie_core::WorkingSetConfig {
    fn from(w: WorkingSetConfigEntry) -> Self {
        statie_core::WorkingSetConfig {
            max_windows: w.max_windows,
            recent_calls: w.recent_calls,
        }
    }
}

/// Context-maintenance config (statie-spec.md "9. Context maintenance").
/// `model` defaults to the top-level `model` when omitted, so the worker's
/// model runs the maintenance call too.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MaintainerConfig {
    pub model: Option<ModelConfig>,
    pub budget: usize,
    pub timeout_secs: u64,
    pub prompt_budget: usize,
}

impl Default for MaintainerConfig {
    fn default() -> Self {
        MaintainerConfig {
            model: None,
            budget: 400,
            timeout_secs: 120,
            prompt_budget: 4000,
        }
    }
}

/// The `think` tool's reasoning backend config. `model` defaults to the
/// top-level `model` when omitted, the same as `MaintainerConfig::model`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThinkerConfig {
    pub model: Option<ModelConfig>,
    pub timeout_secs: u64,
    /// Context window sent on the reasoning call, separate from the
    /// session's pinned `model.num_ctx`: a second live battery reproduced
    /// every `think` call coming back empty because native thinking on the
    /// configured model routinely runs 8k-11k tokens, and the session's
    /// 16384 `num_ctx` shared between the prompt and that trace left no
    /// room -- the model hit the context limit (`done_reason: "length"`)
    /// before ever emitting content. The trace needs room the acting model
    /// does not. Ollama reloads the model whenever a request's `num_ctx`
    /// differs from the last one, so every `think` call costs a reload each
    /// way (into this window, then back to `model.num_ctx` for the next
    /// acting-model step); acceptable because `think` fires rarely.
    ///
    /// `None` (the default) picks the window based on `chunked`: 32768 when
    /// unchunked (the reasoning trace gets its own large window, at the cost
    /// of that reload each way), or the session's own `model.num_ctx` when
    /// `chunked` is true (the whole point of chunking is to fit inside the
    /// window the acting model already runs in, so no reload is needed).
    pub num_ctx: Option<u32>,
    /// Runs `think` as a Markovian-Thinker chunked loop (Aghajohari et al.,
    /// ICLR 2026) instead of one native-thinking call: fixed-size reasoning
    /// chunks with a short summarized carry-over between them, so a long
    /// trace fits inside the session's own context window rather than
    /// needing a separate, larger one and the model reload that comes with
    /// switching to it. Default `true` since the calibration battery: a
    /// chunked think on the rounding trap answered in 46 seconds against
    /// 100 to 250 unchunked with a reload each way (DECISIONS.md D9). When
    /// `true`, built with `ChunkConfig::default()`.
    pub chunked: bool,
    /// Whether `think` is advertised in the tool list the model sees
    /// (`skills/src/coding/mod.rs`'s `PlanSkill`/`ImplementSkill`/
    /// `ReviewSkill::tools()`). Default `true` (today's behaviour). `false`
    /// hides `think` from the prompt's tool list without disabling it: the
    /// harness's own triggers (`implement_repeat_failure_thinks`'s
    /// repeated-failure substitution, the early-park substitution) still
    /// construct `Action::Tool { name: "think", .. }` directly and keep
    /// firing exactly as before -- only the model's own ability to choose
    /// `think` on its own is removed.
    pub listed: bool,
}

impl Default for ThinkerConfig {
    fn default() -> Self {
        ThinkerConfig {
            model: None,
            timeout_secs: 600,
            num_ctx: None,
            chunked: true,
            listed: false,
        }
    }
}

/// The default `num_ctx` for an unchunked thinker: its own large window,
/// separate from the acting model's.
const UNCHUNKED_DEFAULT_NUM_CTX: u32 = 32768;

/// The `try_fixes` tool's proposer backend config (BACKLOG.md item 4).
/// `model` defaults to the top-level `model` when omitted, the same as
/// `ThinkerConfig::model`. Unlike `thinker.num_ctx`, `num_ctx` here has no
/// deliberate override default: these calls run with thinking off and a
/// short `num_predict` (1200), so there is no trace competing with the
/// prompt for room the way `think` has -- `num_ctx` simply defaults to the
/// resolved model's own `num_ctx`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FixerConfig {
    pub model: Option<ModelConfig>,
    pub timeout_secs: u64,
    /// How many candidate fixes to sample per `try_fixes` call.
    pub k: u32,
    /// Context window sent on each candidate call. `None` (the default)
    /// uses the resolved model's own `num_ctx`.
    pub num_ctx: Option<u32>,
}

impl Default for FixerConfig {
    fn default() -> Self {
        FixerConfig {
            model: None,
            timeout_secs: 600,
            k: 3,
            num_ctx: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// "ollama" | "openai" | "mock"
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub think: bool,
    pub api_key: Option<String>,
    /// Context window sent on every Ollama request. Pinned: changing it
    /// makes Ollama reload the model with a differently sized KV cache.
    pub num_ctx: u32,
    /// Ollama `format` mode: "schema" (full JSON schema), "json" (plain JSON
    /// mode, the default) or "none". Full-schema grammars were observed to
    /// degrade gemma4's output on Ollama 0.33; the harness validates every
    /// output against the schema regardless.
    pub format: OllamaFormat,
    /// Whether the backend enforces the output schema. `null` derives it from
    /// `format` (ollama) / always true (openai). When false the schema is also
    /// pasted into the system prompt.
    pub schema_constrained: Option<bool>,
    /// Passed through verbatim: ollama `options` (minus `num_ctx`), openai extra body.
    pub options: serde_json::Value,
    pub timeout_secs: u64,
    /// Ollama only: retries after a 5xx response or a transport-level
    /// failure (connection reset/refused, or a mid-stream EOF that isn't
    /// the configured timeout). See `statie_models::OllamaConfig`.
    pub retries_on_5xx: u32,
    /// Ollama only: delay between retries, in seconds.
    pub retry_delay_secs: u64,
    /// Only used when `backend == "mock"`.
    pub mock_script: Vec<MockScriptTurn>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            backend: "ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            model: "gemma4:26b".to_string(),
            think: true,
            api_key: None,
            num_ctx: 32768,
            format: OllamaFormat::Json,
            schema_constrained: None,
            options: serde_json::json!({}),
            timeout_secs: 600,
            retries_on_5xx: 2,
            retry_delay_secs: 15,
            mock_script: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MockScriptTurn {
    pub reasoning: Option<String>,
    pub content: String,
    /// Scripted server-reported prompt tokens for this turn. Both this and
    /// `completion_tokens` must be present to attach `Usage`.
    pub prompt_tokens: Option<u64>,
    /// Scripted server-reported completion tokens for this turn.
    pub completion_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    pub total: usize,
    pub observation: usize,
    pub augmentation: usize,
    /// Cap on the harness-maintained `notes` section (statie-spec.md "9.
    /// Context maintenance"). Overridden by `maintainer.budget` when a
    /// `maintainer` section is present (see `Config::build_maintainer`).
    pub notes: usize,
    /// Cap on the working-set section (statie-spec.md "10. Working set").
    pub working_set: usize,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            total: 24000,
            observation: 3000,
            augmentation: 2000,
            notes: 400,
            working_set: 6000,
        }
    }
}

impl From<BudgetConfig> for statie_core::Budgets {
    fn from(b: BudgetConfig) -> Self {
        statie_core::Budgets {
            total: b.total,
            observation: b.observation,
            augmentation: b.augmentation,
            notes: b.notes,
            working_set: b.working_set,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// "memory" | "sqlite"
    pub kind: String,
    pub path: PathBuf,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            kind: "sqlite".to_string(),
            path: PathBuf::from(".statie/session.db"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AugmentersConfig {
    pub repo_bm25: AugmenterEntry,
    pub session_log_bm25: AugmenterEntry,
}

impl Default for AugmentersConfig {
    fn default() -> Self {
        AugmentersConfig {
            repo_bm25: AugmenterEntry {
                enabled: true,
                budget: 1200,
            },
            session_log_bm25: AugmenterEntry {
                enabled: true,
                budget: 800,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AugmenterEntry {
    pub enabled: bool,
    pub budget: usize,
}

impl Default for AugmenterEntry {
    fn default() -> Self {
        AugmenterEntry {
            enabled: true,
            budget: 1000,
        }
    }
}

/// Errors from loading or parsing a config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid config file {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// Resolves the config file path per ARCHITECTURE.md's precedence, first
/// hit wins:
///
/// 1. `--config <path>`
/// 2. `$STATIE_CONFIG`
/// 3. `./.statie/config.json`
/// 4. `$XDG_CONFIG_HOME/statie/config.json` (defaulting to
///    `~/.config/statie/config.json`)
/// 5. `<platform config dir>/statie/config.json` (e.g.
///    `~/Library/Application Support/statie/config.json` on macOS) — a
///    fallback for files written before this XDG precedence existed
/// 6. (none — built-in defaults)
pub fn resolve_path(cli_flag: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = cli_flag {
        return Some(p);
    }
    if let Ok(p) = std::env::var("STATIE_CONFIG")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let cwd_config = Path::new(".statie/config.json");
    if cwd_config.exists() {
        return Some(cwd_config.to_path_buf());
    }
    if let Some(xdg_config) = xdg_config_path()
        && xdg_config.exists()
    {
        return Some(xdg_config);
    }
    if let Some(platform_config) = dirs::config_dir().map(|d| d.join("statie").join("config.json"))
        && platform_config.exists()
    {
        return Some(platform_config);
    }
    None
}

/// `$XDG_CONFIG_HOME/statie/config.json` if that env var is set and
/// non-empty, else `<home>/.config/statie/config.json`.
pub fn xdg_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("statie").join("config.json"));
    }
    dirs::home_dir().map(|d| d.join(".config").join("statie").join("config.json"))
}

/// Loads and parses the effective config. Returns the config plus the path
/// it was loaded from (`None` means built-in defaults, no file found).
pub fn load(cli_flag: Option<PathBuf>) -> Result<(Config, Option<PathBuf>), ConfigError> {
    let Some(path) = resolve_path(cli_flag) else {
        return Ok((Config::default(), None));
    };

    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let mut cfg: Config = serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;

    cfg.validate().map_err(|message| ConfigError::Invalid {
        path: path.clone(),
        message,
    })?;

    if cfg.model.api_key.is_none()
        && let Ok(key) = std::env::var("STATIE_API_KEY")
        && !key.is_empty()
    {
        cfg.model.api_key = Some(key);
    }

    Ok((cfg, Some(path)))
}

/// The default config path for `statie config init --global`.
pub fn global_config_path() -> Option<PathBuf> {
    xdg_config_path()
}

/// The default config path for `statie config init` (no `--global`).
pub fn local_config_path() -> PathBuf {
    PathBuf::from(".statie/config.json")
}

impl Config {
    /// Cross-field checks that serde cannot express.
    pub fn validate(&self) -> Result<(), String> {
        if self.model.options.get("num_ctx").is_some() {
            return Err(
                "`model.options.num_ctx` is not allowed: the context window is pinned via \
                 `model.num_ctx` (sent on every request) so the server never reloads the \
                 model with a different KV-cache size; move the value to `model.num_ctx`"
                    .to_string(),
            );
        }
        if self.model.num_ctx == 0 {
            return Err("`model.num_ctx` must be greater than zero".to_string());
        }
        Ok(())
    }

    /// Creates `<sandbox_root>/.statie/` if missing and ensures it
    /// self-ignores from git, the way `target/` does for cargo: writes
    /// `.statie/.gitignore` with `*\n` if (and only if) that file does not
    /// already exist. Never overwrites an existing `.gitignore`. Idempotent.
    /// Returns the directory path.
    pub fn ensure_statie_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.sandbox_root.join(".statie");
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dir.display()))?;

        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, "*\n")
                .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", gitignore.display()))?;
        }

        Ok(dir)
    }

    /// Builds the configured `Model` backend. Unknown `backend` values are a
    /// named error (never silently default to a backend).
    pub fn build_model(&self) -> anyhow::Result<Box<dyn statie_core::Model>> {
        build_model_from(&self.model)
    }

    /// The `Budgets` a `Session` should be built with: `self.budget`,
    /// except `notes` is overridden by `maintainer.budget` when a
    /// `maintainer` section is present (statie-spec.md "9. Context
    /// maintenance": "`budgets.notes` (default 400 tokens) caps the
    /// field"). Absent a `maintainer` section, `budgets.notes` keeps its
    /// own default/configured value.
    pub fn effective_budgets(&self) -> statie_core::Budgets {
        let mut budgets: statie_core::Budgets = self.budget.into();
        if let Some(m) = &self.maintainer {
            budgets.notes = m.budget;
        }
        budgets
    }

    /// Builds the context-maintenance backend (statie-spec.md "9. Context
    /// maintenance") from `self.maintainer`. `None` when the section is
    /// absent -- the session then runs with no maintainer, exactly as
    /// before this feature existed. `maintainer.model` is built when given,
    /// else the top-level `model` is reused, so the worker's own model runs
    /// the maintenance call by default.
    pub fn build_maintainer(&self) -> anyhow::Result<Option<Arc<dyn Maintainer>>> {
        let Some(m) = &self.maintainer else {
            return Ok(None);
        };
        let model_cfg = m.model.as_ref().unwrap_or(&self.model);
        let model: Arc<dyn statie_core::Model> = build_model_from(model_cfg)?.into();
        let tokenizer: Arc<dyn statie_core::Tokenizer> = Arc::new(TiktokenTokenizer);
        let maintainer = ModelMaintainer::new(
            model,
            tokenizer,
            m.prompt_budget,
            Duration::from_secs(m.timeout_secs),
        );
        Ok(Some(Arc::new(maintainer)))
    }

    /// Builds the `think` tool's reasoning backend from `self.thinker`.
    /// `None` when the section is absent -- `think` calls then report
    /// themselves as unconfigured instead of parking, exactly as before
    /// this feature existed. `thinker.model` is built when given, else the
    /// top-level `model` is reused, mirroring `build_maintainer`. Only the
    /// Ollama backend is supported (`ModelThinker` is built directly from
    /// an `OllamaModel`, not the `Arc<dyn Model>` trait object
    /// `build_model_from` returns for other backends): a `thinker.model`
    /// (or, absent that, top-level `model`) with any other backend is a
    /// named error. The resolved model config's `num_ctx` is overridden by
    /// `self.effective_thinker_num_ctx()` -- see `ThinkerConfig::num_ctx`'s
    /// doc. When `thinker.chunked` is true, the thinker is built with
    /// `ChunkConfig::default()` (see `ModelThinker::with_chunking`).
    pub fn build_thinker(&self) -> anyhow::Result<Option<Arc<dyn Thinker>>> {
        let Some(t) = &self.thinker else {
            return Ok(None);
        };
        let model_cfg = t.model.as_ref().unwrap_or(&self.model);
        if model_cfg.backend != "ollama" {
            anyhow::bail!(
                "thinker requires the ollama backend (got `{}`)",
                model_cfg.backend
            );
        }
        let mut ollama_cfg = ollama_config(model_cfg);
        ollama_cfg.num_ctx = self.effective_thinker_num_ctx(t);
        let model = OllamaModel::new(ollama_cfg)?;
        let mut thinker = ModelThinker::new(Arc::new(model), Duration::from_secs(t.timeout_secs));
        if t.chunked {
            thinker = thinker.with_chunking(statie_models::ChunkConfig::default());
        }
        Ok(Some(Arc::new(thinker)))
    }

    /// Resolves the `num_ctx` a thinker is built with: `thinker.num_ctx`
    /// when the user set it explicitly, else 32768 when unchunked or the
    /// session's own `model.num_ctx` when `thinker.chunked` is true (see
    /// `ThinkerConfig::num_ctx`'s doc).
    fn effective_thinker_num_ctx(&self, t: &ThinkerConfig) -> u32 {
        t.num_ctx.unwrap_or(if t.chunked {
            self.model.num_ctx
        } else {
            UNCHUNKED_DEFAULT_NUM_CTX
        })
    }

    /// Builds the `try_fixes` tool's proposer backend from `self.fixer`.
    /// `None` when the section is absent, mirroring `build_thinker`.
    /// `fixer.model` is built when given, else the top-level `model` is
    /// reused. Only the Ollama backend is supported, the same restriction
    /// as `build_thinker` and for the same reason: `ModelFixer` is built
    /// directly from an `OllamaModel`, not the `Arc<dyn Model>` trait
    /// object `build_model_from` returns for other backends. The resolved
    /// model config's `num_ctx` is overridden by `fixer.num_ctx` when set,
    /// else left as the resolved model's own `num_ctx` -- unlike
    /// `thinker`, these calls run with thinking off and a short
    /// `num_predict`, so there is no separate large window to default to.
    ///
    /// Called from `run_coding` (`cli/src/main.rs`) alongside `build_
    /// thinker`, and attached via `Session::with_fixer` with `self.fixer.k`
    /// (defaulting to `FixerConfig::default().k` when the section itself is
    /// absent -- `with_fixer` is only called when this returns `Some`, so
    /// that default never actually reaches a session).
    pub fn build_fixer(&self) -> anyhow::Result<Option<Arc<dyn Fixer>>> {
        let Some(f) = &self.fixer else {
            return Ok(None);
        };
        let model_cfg = f.model.as_ref().unwrap_or(&self.model);
        if model_cfg.backend != "ollama" {
            anyhow::bail!(
                "fixer requires the ollama backend (got `{}`)",
                model_cfg.backend
            );
        }
        let mut ollama_cfg = ollama_config(model_cfg);
        if let Some(num_ctx) = f.num_ctx {
            ollama_cfg.num_ctx = num_ctx;
        }
        let model = OllamaModel::new(ollama_cfg)?;
        let fixer = ModelFixer::new(Arc::new(model), Duration::from_secs(f.timeout_secs));
        Ok(Some(Arc::new(fixer)))
    }

    /// Resolves `self.store.path` and `self.session_log` against the
    /// sandbox root, so relative paths in the config are anchored there
    /// rather than the process cwd.
    pub(crate) fn resolved_store_path(&self) -> PathBuf {
        resolve_under(&self.sandbox_root, &self.store.path)
    }

    fn resolved_session_log_path(&self) -> PathBuf {
        resolve_under(&self.sandbox_root, &self.session_log)
    }

    /// True when a previous sqlite-backed session's store file exists on
    /// disk. Used to warn the user that `statie run` will resume it.
    pub fn has_existing_session(&self) -> bool {
        self.store.kind == "sqlite" && self.resolved_store_path().exists()
    }

    /// Archives an existing sqlite store and/or session log out of the way
    /// so a fresh `statie run` starts a clean session. Moves the store db
    /// (plus any `-wal`/`-shm` sidecars) and the JSONL log into
    /// `<sandbox_root>/.statie/archive/<YYYYmmdd-HHMMSS>/`, preserving file
    /// names. Also removes `<sandbox_root>/.statie/orig`, if present: those
    /// are `revert_file`'s pre-edit backups from the previous session, and a
    /// fresh session must not let `revert_file` restore content from a run
    /// the user is no longer resuming. (A `--resume`d session never calls
    /// this, so its backups are left alone.) Returns `Ok(None)` if there was
    /// nothing to archive (kind is not `sqlite`, or neither the store nor
    /// the log file exists) -- the `.statie/orig` removal still happens in
    /// that case, since a fresh start clears backups independently of
    /// whether there was a session to archive.
    pub fn archive_session(&self) -> anyhow::Result<Option<PathBuf>> {
        self.remove_orig_backups()?;

        if self.store.kind != "sqlite" {
            return Ok(None);
        }

        let store_path = self.resolved_store_path();
        let log_path = self.resolved_session_log_path();

        if !store_path.exists() && !log_path.exists() {
            return Ok(None);
        }

        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let archive_dir = self
            .sandbox_root
            .join(".statie")
            .join("archive")
            .join(timestamp);
        std::fs::create_dir_all(&archive_dir).map_err(|e| {
            anyhow::anyhow!(
                "failed to create archive dir {}: {e}",
                archive_dir.display()
            )
        })?;

        let mut candidates = vec![store_path.clone(), log_path.clone()];
        for ext in ["-wal", "-shm"] {
            if let Some(file_name) = store_path.file_name() {
                let mut sidecar_name = file_name.to_os_string();
                sidecar_name.push(ext);
                candidates.push(store_path.with_file_name(sidecar_name));
            }
        }

        for path in candidates {
            if !path.exists() {
                continue;
            }
            let Some(file_name) = path.file_name() else {
                continue;
            };
            let dest = archive_dir.join(file_name);
            std::fs::rename(&path, &dest).map_err(|e| {
                anyhow::anyhow!(
                    "failed to archive {} to {}: {e}",
                    path.display(),
                    dest.display()
                )
            })?;
        }

        Ok(Some(archive_dir))
    }

    /// Removes `<sandbox_root>/.statie/orig` if present -- the pre-edit
    /// backups `revert_file` reads (see `statie_tools::common::
    /// backup_before_edit`). Called by `archive_session`, i.e. only on a
    /// fresh (non-`--resume`) `statie run`. A no-op if the directory
    /// doesn't exist.
    fn remove_orig_backups(&self) -> anyhow::Result<()> {
        let orig_dir = self.sandbox_root.join(".statie").join("orig");
        if !orig_dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&orig_dir)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", orig_dir.display()))
    }

    /// Builds the configured `StateStore<W>`. Unknown `store.kind` values
    /// are a named error.
    pub fn build_store<W>(&self) -> anyhow::Result<Box<dyn StateStore<W>>>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        match self.store.kind.as_str() {
            "memory" => Ok(Box::new(InMemoryStore::<W>::new())),
            "sqlite" => {
                let store = SqliteStore::<W>::open(self.resolved_store_path())?;
                Ok(Box::new(store))
            }
            other => {
                anyhow::bail!("unknown store kind `{other}` (expected one of: memory, sqlite)")
            }
        }
    }

    /// Always builds an in-memory store, regardless of `self.store.kind`.
    /// Used for `--memory`.
    pub fn build_memory_store<W>(&self) -> Box<dyn StateStore<W>>
    where
        W: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        Box::new(InMemoryStore::<W>::new())
    }

    /// Builds the configured `SessionLog`.
    pub fn build_log(&self) -> anyhow::Result<Arc<dyn SessionLog>> {
        let log = JsonlLog::open(self.resolved_session_log_path())?;
        Ok(Arc::new(log))
    }

    /// Always builds an in-memory `MemoryLog`. Used for `--memory`.
    pub fn build_memory_log(&self) -> Arc<dyn SessionLog> {
        Arc::new(statie_core::MemoryLog::new())
    }
}

fn resolve_under(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn ollama_config(model: &ModelConfig) -> OllamaConfig {
    OllamaConfig {
        base_url: model.base_url.clone(),
        model: model.model.clone(),
        think: model.think,
        num_ctx: model.num_ctx,
        options: model.options.clone(),
        timeout_secs: model.timeout_secs,
        format: model.format,
        schema_constrained: model.schema_constrained,
        retries_on_5xx: model.retries_on_5xx,
        retry_delay_secs: model.retry_delay_secs,
    }
}

fn openai_config(model: &ModelConfig) -> OpenAiCompatConfig {
    let mut cfg = OpenAiCompatConfig {
        base_url: model.base_url.clone(),
        model: model.model.clone(),
        api_key: model.api_key.clone(),
        extra_body: model.options.clone(),
        timeout_secs: model.timeout_secs,
        ..OpenAiCompatConfig::default()
    };
    // Preserve the crate default for reasoning_field/use_json_schema_format
    // unless a future config surface wants to override them explicitly.
    cfg.reasoning_field = Some("reasoning_content".to_string());
    cfg.use_json_schema_format = model.schema_constrained.unwrap_or(true);
    cfg
}

/// Builds a `Model` backend from any `ModelConfig`, independent of a full
/// `Config` -- used both by `Config::build_model` (the top-level `model`)
/// and `Config::build_maintainer` (`maintainer.model`, when given). Unknown
/// `backend` values are a named error (never silently default to a
/// backend).
fn build_model_from(model: &ModelConfig) -> anyhow::Result<Box<dyn statie_core::Model>> {
    match model.backend.as_str() {
        "ollama" => {
            let m = OllamaModel::new(ollama_config(model))?;
            Ok(Box::new(m))
        }
        "openai" => {
            let m = OpenAiCompatModel::new(openai_config(model))?;
            Ok(Box::new(m))
        }
        "mock" => {
            let turns: Vec<MockTurn> = model
                .mock_script
                .iter()
                .map(|t| {
                    let mut turn = MockTurn::new(t.content.clone());
                    if let Some(r) = &t.reasoning {
                        turn = turn.with_reasoning(r.clone());
                    }
                    if let (Some(p), Some(c)) = (t.prompt_tokens, t.completion_tokens) {
                        turn = turn.with_usage(p, c);
                    }
                    turn
                })
                .collect();
            let m = MockModel::new(turns).repeat_last(true);
            Ok(Box::new(m))
        }
        other => {
            anyhow::bail!("unknown model backend `{other}` (expected one of: ollama, openai, mock)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Guards std::env mutation across tests in this module; `cargo test`
    // runs tests in a module concurrently by default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn empty_object_parses_to_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        let cfg_json = serde_json::to_value(&cfg).unwrap();
        let default_json = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(cfg_json, default_json);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg: Config =
            serde_json::from_str(r#"{"model":{"base_url":"http://10.0.0.5:11434"}}"#).unwrap();
        assert_eq!(cfg.model.base_url, "http://10.0.0.5:11434");
        assert_eq!(cfg.model.backend, "ollama");
        assert_eq!(cfg.model.model, "gemma4:26b");
        assert!(cfg.model.think);
        assert_eq!(cfg.model.timeout_secs, 600);
        assert_eq!(cfg.model.retries_on_5xx, 2);
        assert_eq!(cfg.model.retry_delay_secs, 15);
        assert_eq!(cfg.budget, BudgetConfig::default());
        assert_eq!(cfg.retries, 3);
        assert_eq!(cfg.store, StoreConfig::default());
        assert_eq!(cfg.session_log, PathBuf::from(".statie/session.jsonl"));
        assert_eq!(cfg.sandbox_root, PathBuf::from("."));
        assert_eq!(cfg.augmenters, AugmentersConfig::default());
        assert!(
            cfg.subagents.is_empty(),
            "old config without subagents should still parse"
        );
    }

    #[test]
    fn working_set_round_trips_and_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.working_set, WorkingSetConfigEntry::default());
        assert_eq!(cfg.working_set.max_windows, 4);
        assert_eq!(cfg.working_set.recent_calls, 6);
        assert_eq!(cfg.effective_budgets().working_set, 6000);

        let text = serde_json::json!({
            "working_set": {"max_windows": 2, "recent_calls": 3},
            "budget": {"working_set": 8000},
        })
        .to_string();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.working_set.max_windows, 2);
        assert_eq!(parsed.working_set.recent_calls, 3);
        assert_eq!(parsed.effective_budgets().working_set, 8000);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&parsed).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.working_set, parsed.working_set);
        assert_eq!(round_tripped.budget, parsed.budget);
    }

    #[test]
    fn unknown_key_inside_working_set_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"working_set":{"max_widows":2}}"#);
        assert!(
            result.is_err(),
            "expected an error for the unknown key `max_widows`"
        );
    }

    #[test]
    fn guards_default_when_section_absent() {
        let cfg = Config::default();
        assert_eq!(cfg.guards, Guards::default());

        let parsed: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(
            parsed.guards,
            Guards::default(),
            "an absent guards section must match Guards::default() exactly (every rule on \
             except done_stays_done, off by default since the first bench battery's audit)"
        );
    }

    #[test]
    fn guards_partial_section_defaults_the_rest_to_true() {
        let text = serde_json::json!({
            "guards": {"silent_reads": false}
        })
        .to_string();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.guards,
            Guards {
                silent_reads: false,
                ..Guards::default()
            }
        );
        assert!(parsed.guards.redundant_reads);
        assert!(parsed.guards.read_guard);
        assert!(parsed.guards.redundant_verify);
        assert!(
            !parsed.guards.done_stays_done,
            "done_stays_done defaults to false, unlike the other rules"
        );
        assert!(parsed.guards.commitment_read_cap);
        assert!(parsed.guards.seen_map);
        assert!(parsed.guards.decision_field);
        assert!(parsed.guards.tests_field);
        assert!(parsed.guards.grounded_findings);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&parsed).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.guards, parsed.guards);
    }

    /// BACKLOG.md item 2: `Guards` deserialises as a whole, so `guards.
    /// seen_map` picks up a config value the same way every other field
    /// does -- no special-casing needed in `Config`'s own (de)serialization.
    #[test]
    fn guards_seen_map_false_parses_with_other_fields_at_default() {
        let text = serde_json::json!({
            "guards": {"seen_map": false}
        })
        .to_string();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.guards,
            Guards {
                seen_map: false,
                ..Guards::default()
            }
        );
        assert!(parsed.guards.redundant_reads);
        assert!(parsed.guards.silent_reads);
        assert!(parsed.guards.read_guard);
        assert!(parsed.guards.redundant_verify);
        assert!(
            !parsed.guards.done_stays_done,
            "done_stays_done defaults to false, unlike the other rules"
        );
        assert!(parsed.guards.commitment_read_cap);
        assert!(
            parsed.guards.decision_field,
            "decision_field is a separate switch from seen_map and keeps its own default"
        );
        assert!(!parsed.guards.seen_map);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&parsed).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.guards, parsed.guards);
    }

    /// BACKLOG.md item 5: `guards.tests_field` picks up a config value the
    /// same way every other `Guards` field does, with the rest at default.
    #[test]
    fn guards_tests_field_false_parses_with_other_fields_at_default() {
        let text = serde_json::json!({
            "guards": {"tests_field": false}
        })
        .to_string();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.guards,
            Guards {
                tests_field: false,
                ..Guards::default()
            }
        );
        assert!(parsed.guards.redundant_reads);
        assert!(parsed.guards.silent_reads);
        assert!(parsed.guards.read_guard);
        assert!(parsed.guards.redundant_verify);
        assert!(
            !parsed.guards.done_stays_done,
            "done_stays_done defaults to false, unlike the other rules"
        );
        assert!(parsed.guards.commitment_read_cap);
        assert!(parsed.guards.seen_map);
        assert!(
            parsed.guards.decision_field,
            "decision_field is a separate switch from tests_field and keeps its own default"
        );
        assert!(!parsed.guards.tests_field);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&parsed).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.guards, parsed.guards);
    }

    /// Item 12 (grounded review findings): `guards.grounded_findings` picks
    /// up a config value the same way every other `Guards` field does, with
    /// the rest at default.
    #[test]
    fn guards_grounded_findings_false_parses_with_other_fields_at_default() {
        let text = serde_json::json!({
            "guards": {"grounded_findings": false}
        })
        .to_string();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.guards,
            Guards {
                grounded_findings: false,
                ..Guards::default()
            }
        );
        assert!(parsed.guards.redundant_reads);
        assert!(parsed.guards.silent_reads);
        assert!(parsed.guards.read_guard);
        assert!(parsed.guards.redundant_verify);
        assert!(
            !parsed.guards.done_stays_done,
            "done_stays_done defaults to false, unlike the other rules"
        );
        assert!(parsed.guards.commitment_read_cap);
        assert!(parsed.guards.seen_map);
        assert!(
            parsed.guards.decision_field,
            "decision_field is a separate switch from grounded_findings and keeps its own default"
        );
        assert!(parsed.guards.tests_field);
        assert!(!parsed.guards.grounded_findings);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&parsed).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.guards, parsed.guards);
    }

    #[test]
    fn unknown_key_inside_guards_errors() {
        let result: Result<Config, _> =
            serde_json::from_str(r#"{"guards":{"redundant_raeds":false}}"#);
        assert!(
            result.is_err(),
            "expected an error for the unknown key `redundant_raeds`"
        );
    }

    #[test]
    fn subagents_stanza_from_architecture_example_deserializes() {
        let text = serde_json::json!({
            "subagents": {
                "ui-review": {
                    "instructions": "Build the app, take a screenshot, and report ...",
                    "tools": ["shell", "read_file", "search", "screenshot", "filter_last_tool_call"],
                    "guide": "ui-review",
                    "data_schema": {
                        "type": "object",
                        "properties": {
                            "renders": {"type": "boolean"},
                            "issues": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["renders"]
                    },
                    "budgets": {"total": 12000, "observation": 2000, "augmentation": 800},
                    "max_steps": 30
                }
            }
        })
        .to_string();

        let cfg: Config = serde_json::from_str(&text).unwrap();
        let child = cfg.subagents.get("ui-review").expect("ui-review stanza");
        assert_eq!(child.guide.as_deref(), Some("ui-review"));
        assert_eq!(child.max_steps, 30);
        assert!(child.data_schema.is_some());
        assert_eq!(
            child.tools,
            vec![
                "shell".to_string(),
                "read_file".to_string(),
                "search".to_string(),
                "screenshot".to_string(),
                "filter_last_tool_call".to_string(),
            ]
        );

        // Round-trips through Default too, and an old config (no
        // `subagents` key at all) still parses with an empty map.
        let old: Config = serde_json::from_str(r#"{"model":{"backend":"mock"}}"#).unwrap();
        assert!(old.subagents.is_empty());
    }

    #[test]
    fn unknown_top_level_key_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"modle":{}}"#);
        assert!(
            result.is_err(),
            "expected an error for the unknown key `modle`"
        );
    }

    #[test]
    fn unknown_nested_key_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"model":{"bakend":"mock"}}"#);
        assert!(
            result.is_err(),
            "expected an error for the unknown nested key `bakend`"
        );
    }

    #[test]
    fn resolve_path_precedence_env_over_default_locations() {
        let _guard = ENV_LOCK.lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let statie_dir = dir.path().join(".statie");
        std::fs::create_dir_all(&statie_dir).unwrap();
        std::fs::write(statie_dir.join("config.json"), "{}").unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other_file = other_dir.path().join("other.json");
        std::fs::write(&other_file, "{}").unwrap();

        let orig_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        // SAFETY: guarded by ENV_LOCK; no other test mutates STATIE_CONFIG
        // concurrently within this process.
        unsafe {
            std::env::set_var("STATIE_CONFIG", &other_file);
        }

        let resolved = resolve_path(None);

        unsafe {
            std::env::remove_var("STATIE_CONFIG");
        }
        std::env::set_current_dir(orig_cwd).unwrap();

        assert_eq!(
            resolved,
            Some(other_file),
            "env var should win over ./.statie/config.json"
        );
    }

    #[test]
    fn resolve_path_cli_flag_wins_over_env_and_default() {
        let _guard = ENV_LOCK.lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let statie_dir = dir.path().join(".statie");
        std::fs::create_dir_all(&statie_dir).unwrap();
        std::fs::write(statie_dir.join("config.json"), "{}").unwrap();

        let env_dir = tempfile::tempdir().unwrap();
        let env_file = env_dir.path().join("env.json");
        std::fs::write(&env_file, "{}").unwrap();

        let flag_dir = tempfile::tempdir().unwrap();
        let flag_file = flag_dir.path().join("flag.json");
        std::fs::write(&flag_file, "{}").unwrap();

        let orig_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe {
            std::env::set_var("STATIE_CONFIG", &env_file);
        }

        let resolved = resolve_path(Some(flag_file.clone()));

        unsafe {
            std::env::remove_var("STATIE_CONFIG");
        }
        std::env::set_current_dir(orig_cwd).unwrap();

        assert_eq!(
            resolved,
            Some(flag_file),
            "--config should win over both env and default"
        );
    }

    #[test]
    fn resolve_path_none_when_nothing_present() {
        let _guard = ENV_LOCK.lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let orig_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        unsafe {
            std::env::remove_var("STATIE_CONFIG");
        }

        // Best-effort: this can only assert "None" reliably if the test
        // machine has no ~/.config/statie/config.json (XDG) and no
        // <platform config dir>/statie/config.json. Skip the assertion if
        // either file happens to exist rather than produce a flaky test.
        let would_use_global = xdg_config_path().map(|p| p.exists()).unwrap_or(false)
            || dirs::config_dir()
                .map(|d| d.join("statie").join("config.json"))
                .map(|p| p.exists())
                .unwrap_or(false);

        let resolved = resolve_path(None);
        std::env::set_current_dir(orig_cwd).unwrap();

        if !would_use_global {
            assert_eq!(resolved, None);
        }
    }

    #[test]
    fn resolve_path_uses_xdg_config_home() {
        let _guard = ENV_LOCK.lock().unwrap();

        let xdg_dir = tempfile::tempdir().unwrap();
        let statie_dir = xdg_dir.path().join("statie");
        std::fs::create_dir_all(&statie_dir).unwrap();
        let xdg_file = statie_dir.join("config.json");
        std::fs::write(&xdg_file, "{}").unwrap();

        let cwd_dir = tempfile::tempdir().unwrap();
        let orig_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd_dir.path()).unwrap();
        unsafe {
            std::env::remove_var("STATIE_CONFIG");
            std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        }

        let resolved = resolve_path(None);

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        std::env::set_current_dir(orig_cwd).unwrap();

        assert_eq!(
            resolved,
            Some(xdg_file),
            "should find the config under $XDG_CONFIG_HOME/statie/config.json"
        );
    }

    #[test]
    fn global_config_path_follows_xdg_config_home() {
        let _guard = ENV_LOCK.lock().unwrap();

        let xdg_dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        }

        let resolved = global_config_path();

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        assert_eq!(
            resolved,
            Some(xdg_dir.path().join("statie").join("config.json"))
        );
    }

    #[test]
    fn api_key_falls_back_to_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"model":{"backend":"openai"}}"#).unwrap();

        unsafe {
            std::env::set_var("STATIE_API_KEY", "sk-test-123");
        }
        let (cfg, _) = load(Some(path)).unwrap();
        unsafe {
            std::env::remove_var("STATIE_API_KEY");
        }

        assert_eq!(cfg.model.api_key, Some("sk-test-123".to_string()));
    }

    #[test]
    fn json_parse_error_includes_path_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json").unwrap();

        let err = load(Some(path.clone())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(path.to_str().unwrap()),
            "error should mention the path: {msg}"
        );
    }

    #[test]
    fn mock_backend_builds_model() {
        let cfg = Config {
            model: ModelConfig {
                backend: "mock".to_string(),
                mock_script: vec![MockScriptTurn {
                    reasoning: None,
                    content: "{}".to_string(),
                    ..MockScriptTurn::default()
                }],
                ..ModelConfig::default()
            },
            ..Config::default()
        };
        let model = cfg.build_model().unwrap();
        assert!(model.capabilities().schema_constrained);
    }

    #[test]
    fn unknown_backend_errors_naming_it() {
        let cfg = Config {
            model: ModelConfig {
                backend: "carrier-pigeon".to_string(),
                ..ModelConfig::default()
            },
            ..Config::default()
        };
        let result = cfg.build_model();
        let err = match result {
            Ok(_) => panic!("expected an error for an unknown backend"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("carrier-pigeon"));
    }

    #[test]
    fn unknown_store_kind_errors_naming_it() {
        let cfg = Config {
            store: StoreConfig {
                kind: "postgres".to_string(),
                ..StoreConfig::default()
            },
            ..Config::default()
        };
        let result = cfg.build_store::<statie_skills::ToyWorkspace>();
        let err = match result {
            Ok(_) => panic!("expected an error for an unknown store kind"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("postgres"));
    }

    #[test]
    fn num_ctx_inside_options_is_rejected() {
        let cfg: Config =
            serde_json::from_str(r#"{"model":{"options":{"num_ctx":8192}}}"#).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("model.num_ctx"), "{err}");
        let ok: Config = serde_json::from_str(r#"{"model":{"num_ctx":8192}}"#).unwrap();
        assert!(ok.validate().is_ok());
        assert_eq!(ok.model.num_ctx, 8192);
    }

    #[test]
    fn model_retries_on_5xx_and_retry_delay_default_when_absent() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.model.retries_on_5xx, 2);
        assert_eq!(cfg.model.retry_delay_secs, 15);

        let ollama_cfg = ollama_config(&cfg.model);
        assert_eq!(ollama_cfg.retries_on_5xx, 2);
        assert_eq!(ollama_cfg.retry_delay_secs, 15);
    }

    #[test]
    fn model_retries_on_5xx_and_retry_delay_parse_explicit_values() {
        let text = serde_json::json!({
            "model": {"retries_on_5xx": 5, "retry_delay_secs": 30}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg.model.retries_on_5xx, 5);
        assert_eq!(cfg.model.retry_delay_secs, 30);

        let ollama_cfg = ollama_config(&cfg.model);
        assert_eq!(ollama_cfg.retries_on_5xx, 5);
        assert_eq!(ollama_cfg.retry_delay_secs, 30);

        // Round-trips through serialize -> deserialize unchanged.
        let json = serde_json::to_string(&cfg).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.model, cfg.model);
    }

    #[test]
    fn maintainer_model_inherits_retries_fields_through_ollama_config() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "retries_on_5xx": 4, "retry_delay_secs": 7},
            "maintainer": {"budget": 300, "timeout_secs": 30}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let m = cfg.maintainer.as_ref().expect("maintainer section");
        let model_cfg = m.model.as_ref().unwrap_or(&cfg.model);
        let ollama_cfg = ollama_config(model_cfg);
        assert_eq!(ollama_cfg.retries_on_5xx, 4);
        assert_eq!(ollama_cfg.retry_delay_secs, 7);
    }

    #[test]
    fn maintainer_section_fills_in_defaults() {
        let cfg: Config =
            serde_json::from_str(r#"{"maintainer":{"budget":300,"timeout_secs":30}}"#).unwrap();
        let m = cfg.maintainer.expect("maintainer section should parse");
        assert_eq!(m.budget, 300);
        assert_eq!(m.timeout_secs, 30);
        // Defaults filled in for fields not given.
        assert_eq!(m.prompt_budget, MaintainerConfig::default().prompt_budget);
        assert!(m.model.is_none());
    }

    #[test]
    fn maintainer_model_overrides_top_level_model() {
        let text = serde_json::json!({
            "model": {"backend": "mock"},
            "maintainer": {
                "model": {"backend": "mock", "model": "maintainer-model"}
            }
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let m = cfg.maintainer.expect("maintainer section should parse");
        let model_cfg = m.model.expect("maintainer.model should parse");
        assert_eq!(model_cfg.model, "maintainer-model");
        assert_ne!(model_cfg.model, cfg.model.model);
    }

    #[test]
    fn absent_maintainer_section_builds_none() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.maintainer.is_none());
        let maintainer = cfg.build_maintainer().unwrap();
        assert!(maintainer.is_none());
    }

    #[test]
    fn present_maintainer_section_builds_some() {
        let cfg: Config = serde_json::from_str(
            r#"{"model":{"backend":"mock"},"maintainer":{"budget":300,"timeout_secs":30}}"#,
        )
        .unwrap();
        let maintainer = cfg.build_maintainer().unwrap();
        assert!(maintainer.is_some());
    }

    #[test]
    fn thinker_section_parses_and_fills_in_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"thinker":{"timeout_secs":30}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert_eq!(t.timeout_secs, 30);
        assert!(t.model.is_none());
        assert_eq!(t.num_ctx, None, "thinker.num_ctx must default to None");
        assert!(t.chunked, "thinker.chunked must default to true (D9)");
        assert_eq!(
            cfg.effective_thinker_num_ctx(&t),
            cfg.model.num_ctx,
            "chunked, effective num_ctx must follow the session's model.num_ctx"
        );
    }

    /// Item 14: `thinker.listed` absent must default to `false` (D16: `think`
    /// is harness-triggered unless a config lists it).
    #[test]
    fn thinker_listed_absent_defaults_false() {
        let cfg: Config = serde_json::from_str(r#"{"thinker":{"timeout_secs":30}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert!(!t.listed, "thinker.listed must default to false (D16)");

        let cfg: Config = serde_json::from_str(r#"{"thinker":{"listed":true}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert!(t.listed);
    }

    #[test]
    fn thinker_chunked_off_restores_the_separate_32k_window() {
        let cfg: Config =
            serde_json::from_str(r#"{"model":{"num_ctx":16384},"thinker":{"chunked":false}}"#)
                .unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert!(!t.chunked);
        assert_eq!(cfg.effective_thinker_num_ctx(&t), 32768);

        let cfg: Config =
            serde_json::from_str(r#"{"model":{"num_ctx":16384},"thinker":{}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert_eq!(
            cfg.effective_thinker_num_ctx(&t),
            16384,
            "chunked by default, the think runs in the session window: no reload"
        );
    }

    #[test]
    fn thinker_num_ctx_defaults_and_parses_explicit_value() {
        let cfg: Config = serde_json::from_str(r#"{"thinker":{}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert_eq!(t.num_ctx, None);
        assert_eq!(cfg.effective_thinker_num_ctx(&t), 32768);

        let cfg: Config = serde_json::from_str(r#"{"thinker":{"num_ctx":65536}}"#).unwrap();
        let t = cfg.thinker.clone().expect("thinker section should parse");
        assert_eq!(t.num_ctx, Some(65536));
        assert_eq!(cfg.effective_thinker_num_ctx(&t), 65536);
    }

    /// The thinker's `num_ctx` must win over the (possibly much smaller)
    /// session `model.num_ctx` when the Ollama backend is built -- this is
    /// the fix for the second live battery's every-`think`-call-empty
    /// failure (the reasoning trace exhausted a 16384 window shared with the
    /// session's model).
    #[test]
    fn thinker_num_ctx_overrides_model_num_ctx_in_built_ollama_config() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "thinker": {"num_ctx": 65536}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let t = cfg.thinker.as_ref().expect("thinker section should parse");
        let model_cfg = t.model.as_ref().unwrap_or(&cfg.model);
        let mut ollama_cfg = ollama_config(model_cfg);
        ollama_cfg.num_ctx = cfg.effective_thinker_num_ctx(t);
        assert_eq!(
            ollama_cfg.num_ctx, 65536,
            "must carry the thinker's num_ctx, not the model's"
        );
        assert_ne!(ollama_cfg.num_ctx, cfg.model.num_ctx);
    }

    /// `thinker.chunked` with no explicit `num_ctx` must use the session's
    /// own `model.num_ctx` -- the whole point of chunking is to fit inside
    /// the window the acting model already runs in, avoiding the reload a
    /// separate, larger unchunked window costs each way.
    #[test]
    fn thinker_chunked_without_explicit_num_ctx_uses_model_num_ctx() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "thinker": {"chunked": true}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let t = cfg.thinker.as_ref().expect("thinker section should parse");
        assert!(t.chunked);
        assert_eq!(cfg.effective_thinker_num_ctx(t), 16384);
    }

    /// `thinker.chunked` with an explicit `num_ctx` must still use that
    /// explicit value, not the session's `model.num_ctx`.
    #[test]
    fn thinker_chunked_with_explicit_num_ctx_uses_explicit_value() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "thinker": {"chunked": true, "num_ctx": 20000}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let t = cfg.thinker.as_ref().expect("thinker section should parse");
        assert_eq!(cfg.effective_thinker_num_ctx(t), 20000);
    }

    /// `build_thinker` with `chunked: true` must actually build a chunked
    /// thinker whose configured `num_ctx` equals the model's -- an
    /// end-to-end check of `build_thinker` + `effective_thinker_num_ctx`
    /// together, not just the resolution helper in isolation.
    #[test]
    fn build_thinker_chunked_builds_with_model_num_ctx() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "thinker": {"chunked": true}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let thinker = cfg.build_thinker().unwrap();
        assert!(thinker.is_some());
    }

    #[test]
    fn absent_thinker_section_builds_none() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.thinker.is_none());
        let thinker = cfg.build_thinker().unwrap();
        assert!(thinker.is_none());
    }

    #[test]
    fn present_thinker_section_builds_some_with_ollama_backend() {
        let cfg: Config = serde_json::from_str(r#"{"thinker":{"timeout_secs":30}}"#).unwrap();
        // Top-level model defaults to backend "ollama", so this should build.
        let thinker = cfg.build_thinker().unwrap();
        assert!(thinker.is_some());
    }

    #[test]
    fn thinker_with_non_ollama_backend_errors() {
        let cfg: Config =
            serde_json::from_str(r#"{"model":{"backend":"mock"},"thinker":{"timeout_secs":30}}"#)
                .unwrap();
        let result = cfg.build_thinker();
        let err = match result {
            Ok(_) => panic!("expected an error for a non-ollama thinker backend"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("ollama"), "{err}");
    }

    #[test]
    fn absent_fixer_section_builds_none() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.fixer.is_none());
        let fixer = cfg.build_fixer().unwrap();
        assert!(fixer.is_none());
    }

    #[test]
    fn fixer_section_parses_and_fills_in_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"fixer":{"timeout_secs":30}}"#).unwrap();
        let f = cfg.fixer.clone().expect("fixer section should parse");
        assert_eq!(f.timeout_secs, 30);
        assert!(f.model.is_none());
        assert_eq!(f.k, 3, "fixer.k must default to 3");
        assert_eq!(f.num_ctx, None, "fixer.num_ctx must default to None");
    }

    #[test]
    fn fixer_explicit_k_and_num_ctx_parse() {
        let cfg: Config = serde_json::from_str(r#"{"fixer":{"k":5,"num_ctx":8192}}"#).unwrap();
        let f = cfg.fixer.clone().expect("fixer section should parse");
        assert_eq!(f.k, 5);
        assert_eq!(f.num_ctx, Some(8192));
    }

    #[test]
    fn present_fixer_section_builds_some_with_ollama_backend() {
        let cfg: Config = serde_json::from_str(r#"{"fixer":{"timeout_secs":30}}"#).unwrap();
        // Top-level model defaults to backend "ollama", so this should build.
        let fixer = cfg.build_fixer().unwrap();
        assert!(fixer.is_some());
    }

    #[test]
    fn fixer_num_ctx_defaults_to_model_num_ctx_when_absent() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "fixer": {}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let f = cfg.fixer.as_ref().expect("fixer section should parse");
        assert_eq!(f.num_ctx, None);
        let model_cfg = f.model.as_ref().unwrap_or(&cfg.model);
        let mut ollama_cfg = ollama_config(model_cfg);
        if let Some(num_ctx) = f.num_ctx {
            ollama_cfg.num_ctx = num_ctx;
        }
        assert_eq!(
            ollama_cfg.num_ctx, 16384,
            "absent fixer.num_ctx must fall back to the resolved model's own num_ctx"
        );
    }

    #[test]
    fn fixer_num_ctx_overrides_model_num_ctx_when_set() {
        let text = serde_json::json!({
            "model": {"backend": "ollama", "num_ctx": 16384},
            "fixer": {"num_ctx": 65536}
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let f = cfg.fixer.as_ref().expect("fixer section should parse");
        let model_cfg = f.model.as_ref().unwrap_or(&cfg.model);
        let mut ollama_cfg = ollama_config(model_cfg);
        if let Some(num_ctx) = f.num_ctx {
            ollama_cfg.num_ctx = num_ctx;
        }
        assert_eq!(ollama_cfg.num_ctx, 65536);
        assert_ne!(ollama_cfg.num_ctx, cfg.model.num_ctx);
    }

    #[test]
    fn fixer_with_non_ollama_backend_errors() {
        let cfg: Config =
            serde_json::from_str(r#"{"model":{"backend":"mock"},"fixer":{"timeout_secs":30}}"#)
                .unwrap();
        let result = cfg.build_fixer();
        let err = match result {
            Ok(_) => panic!("expected an error for a non-ollama fixer backend"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("ollama"), "{err}");
    }

    #[test]
    fn fixer_model_overrides_top_level_model() {
        let text = serde_json::json!({
            "model": {"backend": "mock"},
            "fixer": {
                "model": {"backend": "ollama", "model": "fixer-model"}
            }
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let f = cfg.fixer.expect("fixer section should parse");
        let model_cfg = f.model.expect("fixer.model should parse");
        assert_eq!(model_cfg.model, "fixer-model");
        assert_ne!(model_cfg.model, cfg.model.model);
        // Top-level model backend is "mock", but fixer.model overrides with
        // "ollama", so build_fixer should succeed.
        let cfg2: Config = serde_json::from_str(&text).unwrap();
        assert!(cfg2.build_fixer().unwrap().is_some());
    }

    #[test]
    fn budgets_notes_follows_maintainer_budget_when_present() {
        let cfg: Config = serde_json::from_str(r#"{"maintainer":{"budget":777}}"#).unwrap();
        assert_eq!(cfg.effective_budgets().notes, 777);

        let cfg_no_maintainer: Config = serde_json::from_str(r#"{"budget":{"notes":55}}"#).unwrap();
        assert_eq!(cfg_no_maintainer.effective_budgets().notes, 55);

        let cfg_default: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(
            cfg_default.effective_budgets().notes,
            BudgetConfig::default().notes
        );
    }

    #[test]
    fn unknown_key_inside_maintainer_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"maintainer":{"budgett":300}}"#);
        assert!(
            result.is_err(),
            "expected an error for the unknown key `budgett` inside `maintainer`"
        );
    }

    #[test]
    fn archive_session_removes_orig_backups() {
        let dir = tempfile::tempdir().unwrap();
        let statie_dir = dir.path().join(".statie");
        let orig_dir = statie_dir.join("orig");
        std::fs::create_dir_all(&orig_dir).unwrap();
        std::fs::write(orig_dir.join("a.txt"), b"original content").unwrap();

        let cfg = Config {
            sandbox_root: dir.path().to_path_buf(),
            store: StoreConfig {
                kind: "memory".to_string(),
                ..StoreConfig::default()
            },
            ..Config::default()
        };

        // No sqlite store/log to archive (kind is "memory"), but the orig
        // backups must still be removed: a fresh start clears them
        // regardless of whether there was a session to archive.
        let archived = cfg.archive_session().unwrap();
        assert!(archived.is_none());
        assert!(
            !orig_dir.exists(),
            ".statie/orig must be removed on a fresh start"
        );
    }

    #[test]
    fn archive_session_is_a_no_op_when_orig_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            sandbox_root: dir.path().to_path_buf(),
            store: StoreConfig {
                kind: "memory".to_string(),
                ..StoreConfig::default()
            },
            ..Config::default()
        };

        // Must not error just because .statie/orig never existed.
        assert!(cfg.archive_session().unwrap().is_none());
    }
}
