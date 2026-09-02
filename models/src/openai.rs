//! `OpenAiCompatModel`: a `Model` backend for OpenAI-compatible chat
//! completions servers (vLLM, llama.cpp server, LM Studio, ...).

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use statie_core::{
    BoundedPrompt, Capabilities, Model, ModelError, RawOutput, Reasoning, Schema, StepSink, Usage,
};
use tokio_util::sync::CancellationToken;

use crate::thoughts::strip_thoughts;

/// Configuration for `OpenAiCompatModel`. Nothing about the endpoint is
/// hardcoded anywhere else in the crate — this struct is the only source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Merged into the request body's top level (e.g. vLLM `guided_json`).
    pub extra_body: Value,
    pub timeout_secs: u64,
    /// Field name on `delta` that carries native reasoning text, if the
    /// server sends one (e.g. `reasoning_content`). `None` disables looking
    /// for it.
    pub reasoning_field: Option<String>,
    pub use_json_schema_format: bool,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        OpenAiCompatConfig {
            base_url: "http://localhost:8000/v1".to_string(),
            model: String::new(),
            api_key: None,
            extra_body: json!({}),
            timeout_secs: 600,
            reasoning_field: Some("reasoning_content".to_string()),
            use_json_schema_format: true,
        }
    }
}

pub struct OpenAiCompatModel {
    cfg: OpenAiCompatConfig,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    error: Option<ChatError>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ChatError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
}

#[derive(Deserialize, Default)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

impl OpenAiCompatModel {
    pub fn new(cfg: OpenAiCompatConfig) -> Result<Self, ModelError> {
        let api_key = cfg
            .api_key
            .clone()
            .or_else(|| std::env::var("STATIE_API_KEY").ok());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| ModelError::Backend(e.to_string()))?;
        Ok(OpenAiCompatModel { cfg, api_key, client })
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }
}

#[async_trait::async_trait]
impl Model for OpenAiCompatModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: false,
            schema_constrained: self.cfg.use_json_schema_format,
            prefix_cache: true,
        }
    }

    async fn step(
        &self,
        prompt: BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        let mut body = json!({
            "model": self.cfg.model,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [
                { "role": "system", "content": prompt.system() },
                { "role": "user", "content": prompt.user() },
            ],
        });

        if self.cfg.use_json_schema_format {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "step_output",
                    "schema": schema.as_value(),
                    "strict": true,
                }
            });
        }

        if let Value::Object(extra) = &self.cfg.extra_body
            && let Value::Object(ref mut map) = body
        {
            for (k, v) in extra {
                map.insert(k.clone(), v.clone());
            }
        }

        let request_text = serde_json::to_string(&body).map_err(|e| ModelError::Io(e.to_string()))?;
        tracing::debug!(chars = request_text.chars().count(), "openai-compat request");

        tokio::select! {
            result = self.run_request(&body, sink) => result,
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
        }
        .map(|(reasoning_native, content, usage)| {
            let (reasoning, content) = if reasoning_native.is_some() {
                (reasoning_native, content)
            } else {
                strip_thoughts(&content)
            };
            let mut out = RawOutput::new(prompt.version(), reasoning, content);
            if let Some(u) = usage {
                out = out.with_usage(u);
            }
            out
        })
    }
}

impl OpenAiCompatModel {
    async fn run_request(
        &self,
        body: &Value,
        sink: &mut dyn StepSink,
    ) -> Result<(Option<Reasoning>, String, Option<Usage>), ModelError> {
        let mut req = self.client.post(self.chat_url()).json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let response = req.send().await.map_err(|e| ModelError::Io(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(ModelError::Backend(format!("{status}: {text}")));
        }

        let mut reasoning_buf = String::new();
        let mut content_buf = String::new();
        let mut line_buf = String::new();
        let mut usage: Option<Usage> = None;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ModelError::Io(e.to_string()))?;
            let text = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&text);

            loop {
                let Some(newline_idx) = line_buf.find('\n') else {
                    break;
                };
                let line: String = line_buf.drain(..=newline_idx).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match self.process_line(line, &mut reasoning_buf, &mut content_buf, &mut usage, sink)? {
                    LineOutcome::Done => {
                        return Ok((
                            (!reasoning_buf.is_empty()).then(|| Reasoning::new(reasoning_buf)),
                            content_buf,
                            usage,
                        ));
                    }
                    LineOutcome::Continue => {}
                }
            }
        }

        Ok((
            (!reasoning_buf.is_empty()).then(|| Reasoning::new(reasoning_buf)),
            content_buf,
            usage,
        ))
    }

    /// Parses one SSE line. Returns `Ok(LineOutcome::Done)` on `data:
    /// [DONE]`. Updates `*usage` when a chunk carries a `usage` object
    /// (the final chunk before `[DONE]` when `stream_options.include_usage`
    /// is set).
    fn process_line(
        &self,
        line: &str,
        reasoning_buf: &mut String,
        content_buf: &mut String,
        usage: &mut Option<Usage>,
        sink: &mut dyn StepSink,
    ) -> Result<LineOutcome, ModelError> {
        tracing::trace!(line, "openai-compat sse line");

        let Some(data) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) else {
            // Not a data line (e.g. an SSE comment or event field); ignore.
            return Ok(LineOutcome::Continue);
        };
        let data = data.trim();

        if data == "[DONE]" {
            return Ok(LineOutcome::Done);
        }

        let parsed: ChatChunk =
            serde_json::from_str(data).map_err(|e| ModelError::Backend(format!("invalid sse chunk: {e}")))?;

        if let Some(err) = parsed.error {
            return Err(ModelError::Backend(err.message));
        }

        if let Some(u) = parsed.usage
            && let Some(prompt_tokens) = u.prompt_tokens
        {
            *usage = Some(Usage {
                prompt_tokens,
                completion_tokens: u.completion_tokens.unwrap_or(0),
            });
        }

        if let Some(choice) = parsed.choices.into_iter().next() {
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                content_buf.push_str(&content);
                sink.content_delta(&content);
            }

            if let Some(field) = &self.cfg.reasoning_field
                && let Some(value) = choice.delta.extra.get(field)
                && let Some(reasoning) = value.as_str()
                && !reasoning.is_empty()
            {
                reasoning_buf.push_str(reasoning);
                sink.reasoning_delta(reasoning);
            }
        }

        Ok(LineOutcome::Continue)
    }
}

enum LineOutcome {
    Continue,
    Done,
}
