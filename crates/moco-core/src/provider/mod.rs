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
    /// Text for the end of the final user message, added by the adapter after the prompt-cache breakpoints. It may
    /// differ from step to step without invalidating the cache of `system`, `tools` and `messages`, which is why
    /// request-time reminders go here and not into `messages`.
    pub tail: Option<String>,
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
        Request { model: String::new(), system: String::new(), messages: vec![], tail: None, tools: vec![], max_tokens: 16_000, tools_disabled: false, effort: None, overlay: Overlay::default() }
    }
}

impl Request {
    /// `messages` with `tail` as a last text block of the final user message (a new user message if the history ends
    /// with the assistant). For adapters whose caching is automatic and prefix-based; Anthropic's marks the
    /// breakpoints first (see `anthropic::build_body`).
    pub fn messages_with_tail(&self) -> std::borrow::Cow<'_, [ChatMessage]> {
        let Some(tail) = &self.tail else { return std::borrow::Cow::Borrowed(&self.messages) };
        let mut messages = self.messages.clone();
        match messages.last_mut().filter(|m| m.role == Role::User) {
            Some(m) => m.content.push(Block::Text(tail.clone())),
            None => messages.push(ChatMessage { role: Role::User, content: vec![Block::Text(tail.clone())] }),
        }
        std::borrow::Cow::Owned(messages)
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
    /// A tool call whose input was not valid JSON (typically cut off at the output token limit). The engine records
    /// a tool error for it so the model can retry, instead of the whole stream failing. `raw` is the text received.
    BadToolCall { id: String, name: String, raw: String, error: String },
    Finish { reason: FinishReason, usage: Usage },
}

/// Keep at most this much of a malformed tool input; it is stored in the session and replayed to the model.
const BAD_INPUT_KEEP: usize = 2000;

/// The placeholder input stored for a malformed call: `{"_raw": <start of the text received>}`.
pub fn bad_call_input(raw: &str) -> Value {
    let mut end = raw.len().min(BAD_INPUT_KEEP);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({"_raw": &raw[..end]})
}

/// Parse streamed tool arguments: empty means no arguments, and invalid JSON becomes an `Event::BadToolCall`.
pub fn parse_tool_input(id: String, name: String, raw: &str) -> Event {
    if raw.trim().is_empty() {
        return Event::ToolCall { id, name, input: Value::Object(Default::default()) };
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(input) => Event::ToolCall { id, name, input },
        Err(e) => Event::BadToolCall { id, name, raw: raw.to_string(), error: e.to_string() },
    }
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

/// Connect timeout of the shared HTTP agent.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Read timeout of the shared HTTP agent (also bounds a stalled stream).
pub(crate) const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The HTTP agent every adapter uses, so timeouts are configured in one place.
pub(crate) fn new_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(CONNECT_TIMEOUT).timeout_read(READ_TIMEOUT).build()
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

enum Msg {
    /// Response headers arrived with a success status; lines follow.
    Head,
    Status(u16, Option<u64>, String),
    Failed(ProviderError),
    Line(String),
    Eof,
}

/// A response body read on a helper thread, so that waiting for headers or for the next line never blocks the
/// caller past `cancel`. After a cancel the helper is left to end on its own: it stops at the next line, at EOF or
/// at the agent's read timeout, and its sends fail once the receiver is gone.
pub(crate) struct Lines {
    rx: std::sync::mpsc::Receiver<Msg>,
}

const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn recv(rx: &std::sync::mpsc::Receiver<Msg>, cancel: &AtomicBool) -> Result<Msg, ProviderError> {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ProviderError::Interrupted);
        }
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(m) => return Ok(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ProviderError::Transport { kind: TransportKind::Other, message: "request thread ended unexpectedly".into() })
            }
        }
    }
}

/// Send `body` and return the response lines. Returns `Interrupted` as soon as `cancel` is set, also while the
/// request is still waiting for response headers. A non-2xx status is logged and mapped with `classify`.
pub(crate) fn open_lines(
    provider: &'static str,
    call: ureq::Request,
    body: String,
    cancel: &AtomicBool,
    classify: impl Fn(u16, &str, Option<u64>) -> ProviderError,
) -> Result<Lines, ProviderError> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let resp = match call.send_string(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok());
                let text = r.into_string().unwrap_or_default();
                let _ = tx.send(Msg::Status(code, ra, text));
                return;
            }
            Err(ureq::Error::Transport(t)) => {
                let _ = tx.send(Msg::Failed(transport_error(t)));
                return;
            }
        };
        if tx.send(Msg::Head).is_err() {
            return;
        }
        let mut reader = std::io::BufReader::new(resp.into_reader());
        loop {
            let mut line = String::new();
            let msg = match reader.read_line(&mut line) {
                Ok(0) => Msg::Eof,
                Ok(_) => Msg::Line(line),
                Err(e) => {
                    let kind = if e.kind() == std::io::ErrorKind::TimedOut { TransportKind::Timeout } else { TransportKind::Other };
                    Msg::Failed(ProviderError::Transport { kind, message: format!("stream read failed: {e}") })
                }
            };
            let last = !matches!(msg, Msg::Line(_));
            if tx.send(msg).is_err() || last {
                return;
            }
        }
    });
    match recv(&rx, cancel)? {
        Msg::Head => Ok(Lines { rx }),
        Msg::Status(code, ra, text) => {
            log_http_error(provider, code, ra, &text);
            Err(classify(code, &text, ra))
        }
        Msg::Failed(e) => Err(e),
        Msg::Line(_) | Msg::Eof => Err(ProviderError::Transport { kind: TransportKind::Other, message: "unexpected response state".into() }),
    }
}

impl Lines {
    /// The next line, `None` at the end of the body. Honors `cancel` while waiting.
    fn next(&mut self, cancel: &AtomicBool) -> Result<Option<String>, ProviderError> {
        match recv(&self.rx, cancel)? {
            Msg::Line(l) => Ok(Some(l)),
            Msg::Eof => Ok(None),
            Msg::Failed(e) => Err(e),
            Msg::Head | Msg::Status(..) => Err(ProviderError::Transport { kind: TransportKind::Other, message: "unexpected response state".into() }),
        }
    }
}

/// Read a server-sent-events body: `on_data` gets the payload of every `data:` line. It returns `Ok(true)` once the
/// stream is complete (stop reading), and may fail the stream. Stops with `Interrupted` as soon as `cancel` is set.
pub(crate) fn read_sse(mut lines: Lines, cancel: &AtomicBool, mut on_data: impl FnMut(&str) -> Result<bool, ProviderError>) -> Result<(), ProviderError> {
    loop {
        let Some(line) = lines.next(cancel)? else { return Ok(()) };
        if let Some(data) = line.trim_end().strip_prefix("data:") {
            if on_data(data.trim_start())? {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tail_tests {
    use super::*;

    fn request(tail: Option<&str>, messages: Vec<ChatMessage>) -> Request {
        Request { model: "m".into(), system: "sys".into(), messages, tail: tail.map(String::from), ..Default::default() }
    }
    fn user(t: &str) -> ChatMessage {
        ChatMessage { role: Role::User, content: vec![Block::Text(t.into())] }
    }
    fn results() -> ChatMessage {
        ChatMessage { role: Role::User, content: vec![Block::ToolResult { id: "t1".into(), content: "out".into(), is_error: false }] }
    }
    fn call() -> ChatMessage {
        ChatMessage { role: Role::Assistant, content: vec![Block::ToolUse { id: "t1".into(), name: "read".into(), input: serde_json::json!({}) }] }
    }

    #[test]
    fn the_tail_ends_the_final_user_message_and_leaves_the_history_alone() {
        let history = vec![user("go"), call(), results()];
        let r = request(Some("TAIL"), history.clone());
        let m = r.messages_with_tail();
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].content.last(), Some(&Block::Text("TAIL".into())));
        assert_eq!(&m[..2], &history[..2]);
        assert_eq!(r.messages, history, "the request itself is not changed");
        // A history that ends with the assistant gets a message of its own.
        let m = request(Some("TAIL"), vec![user("go"), call()]).messages_with_tail().into_owned();
        assert_eq!((m.len(), m[2].role), (3, Role::User));
        assert!(matches!(request(None, history).messages_with_tail(), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn every_adapter_sends_the_tail_once_and_no_tail_changes_nothing() {
        let history = vec![user("go"), call(), results()];
        let bodies = |tail: Option<&str>| {
            let r = request(tail, history.clone());
            [
                anthropic::build_body(&r, true).to_string(),
                openai_chat::build_body(&r, false).to_string(),
                openai_responses::build_body(&r).to_string(),
                gemini::build_body(&r).to_string(),
            ]
        };
        let (with, without) = (bodies(Some("TAIL-TEXT")), bodies(None));
        for (i, (w, wo)) in with.iter().zip(&without).enumerate() {
            assert_eq!(w.matches("TAIL-TEXT").count(), 1, "adapter {i}: {w}");
            assert!(!wo.contains("TAIL-TEXT"), "adapter {i}");
        }
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    #[test]
    fn shared_agent_timeouts() {
        assert_eq!(CONNECT_TIMEOUT, std::time::Duration::from_secs(20));
        assert_eq!(READ_TIMEOUT, std::time::Duration::from_secs(120));
        // ureq exposes no getters, so just check the agent builds and can prepare a request.
        let _ = new_agent().post("http://127.0.0.1:1/");
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

#[cfg(test)]
mod cancel_tests {
    use super::testserver::*;
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn adapters(url: &str) -> Vec<(&'static str, Box<dyn Provider>)> {
        vec![
            ("anthropic", Box::new(anthropic::Anthropic::new("k".into(), Some(url.to_string())))),
            ("openai-chat", Box::new(openai_chat::OpenAiChat::new(None, format!("{url}/v1"), BTreeMap::new()))),
            ("openai-responses", Box::new(openai_responses::OpenAiResponses::new(None, format!("{url}/v1"), BTreeMap::new()))),
            ("gemini", Box::new(gemini::Gemini::new(None, url.to_string(), BTreeMap::new()))),
        ]
    }

    /// Esc while the request waits for response headers, and while a started stream goes quiet: the adapter returns
    /// `Interrupted` within a moment, not after the 120 s read timeout.
    #[test]
    fn cancel_is_honored_while_waiting_for_headers_and_during_a_stalled_stream() {
        for head in [None, Some("data: {\"type\":\"ping\"}\n\n")] {
            let server = stall(head, Duration::from_secs(60));
            for (name, p) in adapters(&server.url) {
                let cancel = AtomicBool::new(false);
                let started = Instant::now();
                let r = std::thread::scope(|s| {
                    s.spawn(|| {
                        std::thread::sleep(Duration::from_millis(300));
                        cancel.store(true, Ordering::Relaxed);
                    });
                    p.stream(&Request { model: "m".into(), ..Default::default() }, &cancel, &mut |_| {})
                });
                assert_eq!(r, Err(ProviderError::Interrupted), "{name} head={head:?}");
                assert!(started.elapsed() < Duration::from_secs(5), "{name} head={head:?}: {:?}", started.elapsed());
            }
        }
    }
}
