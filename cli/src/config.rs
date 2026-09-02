//! Configuration loading for the `statie` binary (ARCHITECTURE.md
//! "Configuration"). Nothing about the model endpoint is hardcoded
//! anywhere else: this module is the single source of truth, and every
//! field has a serde default so a partial (or absent) file is valid.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use statie_core::{SessionLog, StateStore};
use statie_models::{MockModel, MockTurn, OllamaConfig, OllamaFormat, OllamaModel, OpenAiCompatConfig, OpenAiCompatModel};
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
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            total: 24000,
            observation: 3000,
            augmentation: 2000,
        }
    }
}

impl From<BudgetConfig> for statie_core::Budgets {
    fn from(b: BudgetConfig) -> Self {
        statie_core::Budgets {
            total: b.total,
            observation: b.observation,
            augmentation: b.augmentation,
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
/// 4. `~/.config/statie/config.json`
/// 5. (none — built-in defaults)
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
    if let Some(home_config) = dirs::config_dir().map(|d| d.join("statie").join("config.json"))
        && home_config.exists()
    {
        return Some(home_config);
    }
    None
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
    dirs::config_dir().map(|d| d.join("statie").join("config.json"))
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

    pub fn ollama(&self) -> OllamaConfig {
        OllamaConfig {
            base_url: self.model.base_url.clone(),
            model: self.model.model.clone(),
            think: self.model.think,
            num_ctx: self.model.num_ctx,
            options: self.model.options.clone(),
            timeout_secs: self.model.timeout_secs,
            format: self.model.format,
            schema_constrained: self.model.schema_constrained,
        }
    }

    pub fn openai(&self) -> OpenAiCompatConfig {
        let mut cfg = OpenAiCompatConfig {
            base_url: self.model.base_url.clone(),
            model: self.model.model.clone(),
            api_key: self.model.api_key.clone(),
            extra_body: self.model.options.clone(),
            timeout_secs: self.model.timeout_secs,
            ..OpenAiCompatConfig::default()
        };
        // Preserve the crate default for reasoning_field/use_json_schema_format
        // unless a future config surface wants to override them explicitly.
        cfg.reasoning_field = Some("reasoning_content".to_string());
        cfg.use_json_schema_format = self.model.schema_constrained.unwrap_or(true);
        cfg
    }

    /// Builds the configured `Model` backend. Unknown `backend` values are a
    /// named error (never silently default to a backend).
    pub fn build_model(&self) -> anyhow::Result<Box<dyn statie_core::Model>> {
        match self.model.backend.as_str() {
            "ollama" => {
                let model = OllamaModel::new(self.ollama())?;
                Ok(Box::new(model))
            }
            "openai" => {
                let model = OpenAiCompatModel::new(self.openai())?;
                Ok(Box::new(model))
            }
            "mock" => {
                let turns: Vec<MockTurn> = self
                    .model
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
                let model = MockModel::new(turns).repeat_last(true);
                Ok(Box::new(model))
            }
            other => anyhow::bail!("unknown model backend `{other}` (expected one of: ollama, openai, mock)"),
        }
    }

    /// Resolves `self.store.path` and `self.session_log` against the
    /// sandbox root, so relative paths in the config are anchored there
    /// rather than the process cwd.
    fn resolved_store_path(&self) -> PathBuf {
        resolve_under(&self.sandbox_root, &self.store.path)
    }

    fn resolved_session_log_path(&self) -> PathBuf {
        resolve_under(&self.sandbox_root, &self.session_log)
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
            other => anyhow::bail!("unknown store kind `{other}` (expected one of: memory, sqlite)"),
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
        assert_eq!(cfg.budget, BudgetConfig::default());
        assert_eq!(cfg.retries, 3);
        assert_eq!(cfg.store, StoreConfig::default());
        assert_eq!(cfg.session_log, PathBuf::from(".statie/session.jsonl"));
        assert_eq!(cfg.sandbox_root, PathBuf::from("."));
        assert_eq!(cfg.augmenters, AugmentersConfig::default());
    }

    #[test]
    fn unknown_top_level_key_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"modle":{}}"#);
        assert!(result.is_err(), "expected an error for the unknown key `modle`");
    }

    #[test]
    fn unknown_nested_key_errors() {
        let result: Result<Config, _> = serde_json::from_str(r#"{"model":{"bakend":"mock"}}"#);
        assert!(result.is_err(), "expected an error for the unknown nested key `bakend`");
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

        assert_eq!(resolved, Some(other_file), "env var should win over ./.statie/config.json");
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

        assert_eq!(resolved, Some(flag_file), "--config should win over both env and default");
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
        // machine has no ~/.config/statie/config.json. Skip the assertion
        // if that file happens to exist rather than produce a flaky test.
        let would_use_global = dirs::config_dir()
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
        assert!(msg.contains(path.to_str().unwrap()), "error should mention the path: {msg}");
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
}
