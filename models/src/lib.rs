//! statie-models: model backends for the statie harness.
//!
//! `MockModel` replays scripted outputs for deterministic tests without a
//! GPU. `OllamaModel` talks to a local Ollama server's `/api/chat`
//! endpoint. `OpenAiCompatModel` talks to an OpenAI-compatible chat
//! completions endpoint (vLLM, llama.cpp server, LM Studio, ...).

mod mock;
mod ollama;
mod openai;
mod thoughts;

pub use mock::{MockModel, MockTurn, RecordedPrompt};
pub use ollama::{OllamaConfig, OllamaFormat, OllamaModel};
pub use openai::{OpenAiCompatConfig, OpenAiCompatModel};
pub use thoughts::strip_thoughts;
