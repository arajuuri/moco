//! Provider abstraction (docs/07-providers.md): neutral request/stream types; one adapter per wire protocol.
pub mod anthropic;
pub mod fake;
pub mod gemini;
pub mod openai_chat;
pub mod openai_responses;
pub mod registry;
pub mod retry;
#[cfg(test)]
pub(crate) mod testserver;

use serde_json::Value;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    /// Base64 image data.
    Image { media_type: String, data: String },
    /// A PDF (base64) with the name it was attached under.
    Document { media_type: String, data: String, name: String },
    /// Opaque data a provider needs echoed back on later turns (OpenAI's encrypted reasoning items, Gemini's thought
    /// signatures). Stored with the assistant message; only the adapter named in `provider` ever reads it.
    Native { provider: String, data: Value },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Keep tool definitions (prompt-cache prefix) but forbid calling them.
    pub tools_disabled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolInputStart { id: String, name: String },
    /// Provider-private data to store and replay (see `Block::Native`). Arrives before the content it belongs to.
    Native { provider: String, data: Value },
    /// A complete tool call; the loop starts executing it immediately.
    ToolCall { id: String, name: String, input: Value },
    Finish { reason: FinishReason, usage: Usage },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Timeout,
    Connect,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderError {
    Auth(String),
    RateLimit { retry_after_ms: Option<u64>, message: String },
    Internal { retry_after_ms: Option<u64>, message: String },
    InvalidRequest(String),
    ContextOverflow(String),
    ContentFilter(String),
    Transport { kind: TransportKind, message: String },
    Interrupted,
}

impl ProviderError {
    pub fn retryable(&self) -> bool {
        matches!(self, ProviderError::RateLimit { .. } | ProviderError::Internal { .. } | ProviderError::Transport { .. })
    }
    pub fn message(&self) -> String {
        match self {
            ProviderError::Auth(m) | ProviderError::InvalidRequest(m) | ProviderError::ContextOverflow(m) | ProviderError::ContentFilter(m) => m.clone(),
            ProviderError::RateLimit { message, .. } | ProviderError::Internal { message, .. } | ProviderError::Transport { message, .. } => message.clone(),
            ProviderError::Interrupted => "interrupted".into(),
        }
    }
}

pub trait Provider: Send + Sync {
    /// Stream one response, calling `on_event` for each neutral event. Must stop promptly (returning
    /// `Interrupted`) once `cancel` is set. Errors may occur after some events were delivered.
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError>;
}

/// Map a transport failure from `ureq` to a neutral error (shared by the adapters).
pub(crate) fn transport_error(t: ureq::Transport) -> ProviderError {
    let kind = match t.kind() {
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => TransportKind::Connect,
        _ if t.to_string().to_lowercase().contains("timed out") => TransportKind::Timeout,
        _ => TransportKind::Other,
    };
    ProviderError::Transport { kind, message: t.to_string() }
}

/// Read a server-sent-events body: `on_data` gets the payload of every `data:` line. It returns `Ok(true)` once the
/// stream is complete (stop reading), and may fail the stream. Stops with `Interrupted` as soon as `cancel` is set.
pub(crate) fn read_sse(reader: impl std::io::Read, cancel: &AtomicBool, mut on_data: impl FnMut(&str) -> Result<bool, ProviderError>) -> Result<(), ProviderError> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ProviderError::Interrupted);
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => {
                let kind = if e.kind() == std::io::ErrorKind::TimedOut { TransportKind::Timeout } else { TransportKind::Other };
                return Err(ProviderError::Transport { kind, message: format!("stream read failed: {e}") });
            }
        }
        if let Some(data) = line.trim_end().strip_prefix("data:") {
            if on_data(data.trim_start())? {
                return Ok(());
            }
        }
    }
}
