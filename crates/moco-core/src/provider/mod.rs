//! Provider abstraction (docs/07-providers.md): neutral request/stream types; one adapter per wire protocol.
pub mod anthropic;
pub mod fake;
pub mod retry;

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
