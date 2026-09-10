//! statie-models: model backends for the statie harness.
//!
//! `MockModel` replays scripted outputs for deterministic tests without a
//! GPU. `OllamaModel` talks to a local Ollama server's `/api/chat`
//! endpoint. `OpenAiCompatModel` talks to an OpenAI-compatible chat
//! completions endpoint (vLLM, llama.cpp server, LM Studio, ...).

mod fixer;
mod maintainer;
mod mock;
mod ollama;
mod openai;
mod thinker;
mod thoughts;

pub use fixer::ModelFixer;
pub use maintainer::ModelMaintainer;
pub use mock::{MockModel, MockTurn, RecordedPrompt};
pub use ollama::{FreeReply, OllamaConfig, OllamaFormat, OllamaModel};
pub use openai::{OpenAiCompatConfig, OpenAiCompatModel};
pub use thinker::{ChunkConfig, ModelThinker};
pub use thoughts::strip_thoughts;
