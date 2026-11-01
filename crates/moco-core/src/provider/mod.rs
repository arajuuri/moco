//! Provider abstraction (docs/07-providers.md): neutral request/stream types; one adapter per wire protocol.
pub mod anthropic;
pub mod effort;
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
    /// Reasoning effort (a "variant": `low`, `high`, ...); each adapter translates it for its protocol.
    /// `None` leaves the provider's default.
    pub effort: Option<String>,
    /// Extra headers and body fields from config, applied last (see `effort::merge_json`).
    pub overlay: Overlay,
}

impl Default for Request {
    fn default() -> Self {
        Request { model: String::new(), system: String::new(), messages: vec![], tools: vec![], max_tokens: 16_000, tools_disabled: false, effort: None, overlay: Overlay::default() }
    }
}

/// Per-model request customization from config: headers added to the HTTP request and a JSON object merged
/// into the request body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
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

/// Wraps a provider to log every request: start, time to first event, finish reason, usage and the classified error.
/// At `trace` it also logs each event. The adapters log the wire-level details (URL, headers, bodies) themselves.
pub struct Logged {
    inner: Box<dyn Provider>,
    id: String,
}

impl Logged {
    pub fn new(id: &str, inner: Box<dyn Provider>) -> Self {
        Logged { inner, id: id.to_string() }
    }
}

fn error_kind(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Auth(_) => "auth",
        ProviderError::RateLimit { .. } => "rate-limit",
        ProviderError::Internal { .. } => "server-error",
        ProviderError::InvalidRequest(_) => "invalid-request",
        ProviderError::ContextOverflow(_) => "context-overflow",
        ProviderError::ContentFilter(_) => "content-filter",
        ProviderError::Transport { .. } => "transport",
        ProviderError::Interrupted => "interrupted",
    }
}

impl Provider for Logged {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        use crate::log::{enabled, Level};
        if !enabled(Level::Warn, "provider") {
            return self.inner.stream(req, cancel, on_event);
        }
        let started = std::time::Instant::now();
        crate::log_debug!(
            "provider",
            "request provider={} model={} messages={} tools={} approx_tokens={} effort={}",
            self.id,
            req.model,
            req.messages.len(),
            req.tools.len(),
            crate::compaction::estimate_request(req),
            req.effort.as_deref().unwrap_or("-")
        );
        let (mut first, mut finish) = (None, None);
        let trace = enabled(Level::Trace, "provider");
        let result = self.inner.stream(req, cancel, &mut |ev| {
            first.get_or_insert_with(|| started.elapsed());
            if trace {
                crate::log_trace!("provider", "event {ev:?}");
            }
            if let Event::Finish { reason, usage } = &ev {
                finish = Some((reason.clone(), *usage));
            }
            on_event(ev);
        });
        let ms = |d: Option<std::time::Duration>| d.map_or("-".to_string(), |d| d.as_millis().to_string());
        let total = started.elapsed().as_millis();
        match (&result, finish) {
            (Ok(()), Some((reason, u))) => crate::log_debug!(
                "provider",
                "done model={} first_byte_ms={} total_ms={total} finish={reason:?} input={} output={} cache_read={} cache_write={}",
                req.model,
                ms(first),
                u.input,
                u.output,
                u.cache_read,
                u.cache_write
            ),
            (Ok(()), None) => crate::log_debug!("provider", "done model={} first_byte_ms={} total_ms={total} (no finish event)", req.model, ms(first)),
            (Err(ProviderError::Interrupted), _) => crate::log_debug!("provider", "interrupted model={} total_ms={total}", req.model),
            (Err(e), _) => {
                // A retry is expected to be survivable (warn); anything else ends the step (error).
                let level = if e.retryable() { Level::Warn } else { Level::Error };
                crate::log_at!(level, "provider", "failed model={} kind={} first_byte_ms={} total_ms={total}: {}", req.model, error_kind(e), ms(first), e.message());
            }
        }
        result
    }
}

/// Log the request an adapter is about to send. Header values that carry credentials are hidden, and the whole
/// line goes through `log::redact` as well. The body is logged at `trace` only.
pub(crate) fn trace_http(provider: &str, url: &str, headers: &[(&str, &str)], body: &str) {
    use crate::log::{enabled, sensitive_header, Level};
    if !enabled(Level::Trace, "provider") {
        return;
    }
    let shown: Vec<String> = headers.iter().map(|(k, v)| format!("{k}: {}", if sensitive_header(k) { "[redacted]" } else { v })).collect();
    crate::log_trace!("provider", "http-request {provider} POST {url} headers=[{}] body={body}", shown.join("; "));
}

/// Log a non-2xx response with the status and (at debug) the start of its body.
pub(crate) fn log_http_error(provider: &str, status: u16, retry_after: Option<u64>, body: &str) {
    let mut cut = body.len().min(2000);
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    crate::log_debug!("provider", "http-error {provider} status={status} retry_after={} body={}", retry_after.map_or("-".into(), |s| format!("{s}s")), &body[..cut]);
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

#[cfg(test)]
mod log_tests {
    use super::testserver::*;
    use super::*;
    use std::collections::BTreeMap;

    const KEY: &str = "sk-test-SECRETKEY0123456789abcdef";

    fn request() -> Request {
        Request {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![ChatMessage { role: Role::User, content: vec![Block::Text("hello".into())] }],
            overlay: Overlay { headers: vec![("x-custom-token".into(), "TOKEN-VALUE-12345".into())], body: None },
            ..Default::default()
        }
    }

    fn logged(server: &Server) -> Logged {
        Logged::new("fake", Box::new(openai_chat::OpenAiChat::new(Some(KEY.into()), format!("{}/v1", server.url), BTreeMap::new())))
    }

    #[test]
    fn keys_never_reach_the_log_even_at_trace() {
        let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
        let server = serve(vec![sse(ok), error(401, &format!("{{\"error\":{{\"message\":\"Incorrect API key provided: {KEY}\"}}}}"), vec![])]);
        let p = logged(&server);
        let (results, lines) = crate::log::capture::with("trace", || {
            let first = p.stream(&request(), &AtomicBool::new(false), &mut |_| {});
            let second = p.stream(&request(), &AtomicBool::new(false), &mut |_| {});
            (first, second)
        });
        assert_eq!(results.0, Ok(()));
        assert!(matches!(results.1, Err(ProviderError::Auth(_))), "{:?}", results.1);
        let all = lines.join("\n");
        for secret in [KEY, "SECRETKEY0123456789abcdef", "TOKEN-VALUE-12345"] {
            assert!(!all.contains(secret), "{secret} leaked into the log:\n{all}");
        }
        assert!(all.contains("authorization: [redacted]") && all.contains("x-custom-token: [redacted]"), "headers are named, values hidden:\n{all}");
        // What is useful is there: the request, the body, the outcome, the status and the classified error.
        assert!(all.contains("request provider=fake model=m messages=1 tools=0"), "{all}");
        assert!(all.contains("\"content\":\"hello\""), "the body is logged at trace:\n{all}");
        assert!(all.contains("done model=m") && all.contains("input=3 output=1"), "{all}");
        assert!(all.contains("http-error openai-chat status=401") && all.contains("kind=auth"), "{all}");
        // The server really did receive the key: the redaction is in the log, not in the request.
        assert!(server.requests.lock().unwrap()[0].0.to_lowercase().contains(&format!("bearer {}", KEY.to_lowercase())));
    }

    #[test]
    fn the_default_level_logs_failures_only_and_debug_leaves_out_bodies() {
        let server = serve(vec![sse("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"), error(500, "overloaded", vec![])]);
        let p = logged(&server);
        let (_, quiet) = crate::log::capture::with("warn", || {
            let _ = p.stream(&request(), &AtomicBool::new(false), &mut |_| {});
        });
        assert!(quiet.is_empty(), "a good request is silent at warn: {quiet:?}");
        let (_, loud) = crate::log::capture::with("warn", || {
            let _ = p.stream(&request(), &AtomicBool::new(false), &mut |_| {});
        });
        assert_eq!(loud.len(), 1, "{loud:?}");
        assert!(loud[0].starts_with("WARN provider: failed model=m kind=server-error"), "{loud:?}");
        let server = serve(vec![sse("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")]);
        let (_, debug) = crate::log::capture::with("debug", || {
            let _ = logged(&server).stream(&request(), &AtomicBool::new(false), &mut |_| {});
        });
        assert!(debug.iter().all(|l| !l.contains("hello")), "no bodies below trace: {debug:?}");
        assert_eq!(debug.len(), 2, "request and done: {debug:?}");
    }
}
