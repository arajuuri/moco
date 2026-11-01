//! OpenAI-compatible Chat Completions (`POST {base}/chat/completions`, SSE). Covers OpenAI, OpenRouter,
//! DeepSeek, Groq, xAI, Together, Fireworks, Ollama, LM Studio, vLLM, ...
use super::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub struct OpenAiChat {
    pub api_key: Option<String>,
    /// Includes the version path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
    agent: ureq::Agent,
}

impl OpenAiChat {
    pub fn new(api_key: Option<String>, base_url: String, headers: BTreeMap<String, String>) -> Self {
        if let Some(k) = &api_key {
            crate::log::add_secret(k);
        }
        OpenAiChat {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            headers,
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(20)).timeout_read(Duration::from_secs(120)).build(),
        }
    }
}

pub fn build_body(req: &Request, max_completion_tokens: bool) -> Value {
    let mut messages = vec![json!({"role":"system","content":req.system})];
    for m in &req.messages {
        match m.role {
            Role::User => {
                // Tool results must directly follow the assistant tool_calls message.
                let mut text = vec![];
                let mut media: Vec<Value> = vec![];
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => messages.push(json!({"role":"tool","tool_call_id":id,"content":content})),
                        Block::Text(t) => text.push(t.as_str()),
                        Block::Image { media_type, data } => media.push(json!({"type":"image_url","image_url":{"url":format!("data:{media_type};base64,{data}")}})),
                        Block::Document { media_type, data, name } => {
                            media.push(json!({"type":"file","file":{"filename":name,"file_data":format!("data:{media_type};base64,{data}")}}))
                        }
                        Block::ToolUse { .. } | Block::Native { .. } => {}
                    }
                }
                if !media.is_empty() {
                    // Parts form: the text first, then the images and files.
                    let mut parts = vec![];
                    if !text.is_empty() {
                        parts.push(json!({"type":"text","text":text.join("\n\n")}));
                    }
                    parts.extend(media);
                    messages.push(json!({"role":"user","content":parts}));
                } else if !text.is_empty() {
                    messages.push(json!({"role":"user","content":text.join("\n\n")}));
                }
            }
            Role::Assistant => {
                let text: Vec<&str> = m.content.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
                let calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => Some(json!({"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({"role":"assistant","content": if text.is_empty() { Value::Null } else { json!(text.join("\n\n")) }});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                messages.push(msg);
            }
        }
    }
    let mut body = json!({"model":req.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    body[if max_completion_tokens { "max_completion_tokens" } else { "max_tokens" }] = json!(req.max_tokens);
    if !req.tools.is_empty() {
        body["tools"] = req.tools.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.schema}})).collect();
        if req.tools_disabled {
            body["tool_choice"] = json!("none");
        }
    }
    super::effort::shape_openai_chat(&mut body, req);
    body
}

pub fn classify_http(status: u16, body: &str, retry_after: Option<u64>) -> ProviderError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let e = if v["error"].is_object() { &v["error"] } else { &v };
    let msg = e["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}: {}", body.chars().take(300).collect::<String>()));
    let code = e["code"].as_str().or(e["type"].as_str()).unwrap_or("");
    classify(status, code, msg, retry_after.map(|s| s * 1000))
}

fn classify(status: u16, code: &str, msg: String, retry_after_ms: Option<u64>) -> ProviderError {
    let lower = msg.to_lowercase();
    let overflow = code == "context_length_exceeded"
        || lower.contains("maximum context length")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || status == 413;
    if overflow {
        return ProviderError::ContextOverflow(msg);
    }
    match status {
        401 | 403 => ProviderError::Auth(msg),
        // Out of credit is not transient.
        429 if code == "insufficient_quota" || lower.contains("insufficient_quota") || lower.contains("exceeded your current quota") => ProviderError::InvalidRequest(msg),
        429 => ProviderError::RateLimit { retry_after_ms, message: msg },
        408 | 500..=599 => ProviderError::Internal { retry_after_ms, message: msg },
        _ if code == "content_filter" => ProviderError::ContentFilter(msg),
        _ => ProviderError::InvalidRequest(msg),
    }
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    started: bool,
}

/// Incremental decoder for chat-completion chunks.
#[derive(Default)]
pub struct Parser {
    tools: BTreeMap<usize, ToolAcc>,
    finish: Option<String>,
    usage: Option<Usage>,
    emitted_calls: bool,
    finished: bool,
}

impl Parser {
    fn flush_calls(&mut self, out: &mut Vec<Event>) -> Result<(), ProviderError> {
        for (idx, t) in std::mem::take(&mut self.tools) {
            let name = t.name.clone();
            let input = if t.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&t.args).map_err(|e| ProviderError::InvalidRequest(format!("model produced invalid tool input JSON for {name}: {e}")))?
            };
            let id = if t.id.is_empty() { format!("call_{idx}") } else { t.id };
            if !t.started {
                out.push(Event::ToolInputStart { id: id.clone(), name: name.clone() });
            }
            out.push(Event::ToolCall { id, name, input });
            self.emitted_calls = true;
        }
        Ok(())
    }

    pub fn feed(&mut self, data: &str) -> Result<Vec<Event>, ProviderError> {
        let mut out = vec![];
        if data.trim() == "[DONE]" {
            self.flush_calls(&mut out)?;
            out.extend(self.finish_event());
            return Ok(out);
        }
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Internal { retry_after_ms: None, message: format!("bad SSE JSON: {e}") })?;
        if v["error"].is_object() {
            let e = &v["error"];
            return Err(classify(0, e["code"].as_str().or(e["type"].as_str()).unwrap_or(""), e["message"].as_str().unwrap_or("provider error").into(), None));
        }
        if let Some(u) = v["usage"].as_object().filter(|u| !u.is_empty()) {
            let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            let cached = u.get("prompt_tokens_details").and_then(|d| d["cached_tokens"].as_u64()).unwrap_or(0);
            self.usage = Some(Usage { input: get("prompt_tokens").saturating_sub(cached), output: get("completion_tokens"), cache_read: cached, cache_write: 0 });
        }
        let Some(choice) = v["choices"].get(0) else { return Ok(out) };
        let d = &choice["delta"];
        if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
            out.push(Event::TextDelta(t.into()));
        }
        if let Some(t) = d["reasoning_content"].as_str().or(d["reasoning"].as_str()).filter(|t| !t.is_empty()) {
            out.push(Event::ReasoningDelta(t.into()));
        }
        for tc in d["tool_calls"].as_array().into_iter().flatten() {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            let acc = self.tools.entry(idx).or_default();
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                acc.id = id.into();
            }
            if let Some(n) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                acc.name.push_str(n);
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                acc.args.push_str(a);
            }
            if !acc.started && !acc.name.is_empty() {
                acc.started = true;
                let id = if acc.id.is_empty() { format!("call_{idx}") } else { acc.id.clone() };
                out.push(Event::ToolInputStart { id, name: acc.name.clone() });
            }
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            self.flush_calls(&mut out)?;
            if r == "content_filter" {
                return Err(ProviderError::ContentFilter("response blocked by the provider's content filter".into()));
            }
            self.finish = Some(r.to_string());
        }
        Ok(out)
    }

    /// Final event once the stream ends (also called on EOF for servers that omit `[DONE]`).
    pub fn finish_event(&mut self) -> Option<Event> {
        if self.finished || self.finish.is_none() {
            return None;
        }
        self.finished = true;
        let reason = match self.finish.as_deref() {
            Some("tool_calls") | Some("function_call") => FinishReason::ToolUse,
            Some("length") => FinishReason::MaxTokens,
            Some("stop") if self.emitted_calls => FinishReason::ToolUse, // some servers report "stop" after tool calls
            Some("stop") | None => FinishReason::EndTurn,
            Some(o) => FinishReason::Other(o.into()),
        };
        Some(Event::Finish { reason, usage: self.usage.unwrap_or_default() })
    }
}

impl Provider for OpenAiChat {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut call = self.agent.post(&url).set("content-type", "application/json");
        if let Some(k) = &self.api_key {
            call = call.set("authorization", &format!("Bearer {k}"));
        }
        for (k, v) in self.headers.iter().chain(req.overlay.headers.iter().map(|(k, v)| (k, v))) {
            call = call.set(k, v);
        }
        let body = build_body(req, self.base_url.contains("api.openai.com")).to_string();
        let mut shown: Vec<(&str, &str)> = self.api_key.iter().map(|k| ("authorization", k.as_str())).collect();
        shown.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain(req.overlay.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))));
        super::trace_http("openai-chat", &url, &shown, &body);
        let resp = match call.send_string(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok());
                let text = r.into_string().unwrap_or_default();
                super::log_http_error("openai-chat", code, ra, &text);
                return Err(classify_http(code, &text, ra));
            }
            Err(ureq::Error::Transport(t)) => {
                let kind = match t.kind() {
                    ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => TransportKind::Connect,
                    _ if t.to_string().to_lowercase().contains("timed out") => TransportKind::Timeout,
                    _ => TransportKind::Other,
                };
                return Err(ProviderError::Transport { kind, message: t.to_string() });
            }
        };
        let mut parser = Parser::default();
        let mut reader = BufReader::new(resp.into_reader());
        let mut line = String::new();
        let mut finished = false;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Interrupted);
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    let kind = if e.kind() == std::io::ErrorKind::TimedOut { TransportKind::Timeout } else { TransportKind::Other };
                    return Err(ProviderError::Transport { kind, message: format!("stream read failed: {e}") });
                }
            }
            if let Some(data) = line.trim_end().strip_prefix("data:") {
                for ev in parser.feed(data.trim_start())? {
                    finished |= matches!(ev, Event::Finish { .. });
                    on_event(ev);
                }
            }
        }
        if !finished {
            if let Some(ev) = parser.finish_event() {
                finished = true;
                on_event(ev);
            }
        }
        if finished {
            Ok(())
        } else {
            Err(ProviderError::Transport { kind: TransportKind::Other, message: "stream ended before the model finished".into() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testserver::*;
    use super::*;

    fn parse(chunks: &[&str]) -> Result<Vec<Event>, ProviderError> {
        let mut p = Parser::default();
        let mut all = vec![];
        for c in chunks {
            all.extend(p.feed(c)?);
        }
        Ok(all)
    }

    const TOOL_STREAM: &[&str] = &[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Let me "}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"look."}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        r#"{"choices":[{"index":1,"delta":{}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":100}}}"#,
        "[DONE]",
    ];

    #[test]
    fn text_tool_call_usage_and_cached_tokens() {
        let ev = parse(TOOL_STREAM).unwrap();
        assert_eq!(
            ev,
            vec![
                Event::TextDelta("Let me ".into()),
                Event::TextDelta("look.".into()),
                Event::ToolInputStart { id: "call_a".into(), name: "read".into() },
                Event::ToolCall { id: "call_a".into(), name: "read".into(), input: json!({"path":"a.rs"}) },
                Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 20, output: 30, cache_read: 100, cache_write: 0 } },
            ]
        );
    }

    #[test]
    fn parallel_calls_missing_ids_and_stop_after_tools() {
        let ev = parse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"glob","arguments":"{}"}},{"index":1,"function":{"name":"grep","arguments":"{\"pattern\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
        .unwrap();
        let calls: Vec<_> = ev.iter().filter_map(|e| if let Event::ToolCall { id, name, .. } = e { Some((id.as_str(), name.as_str())) } else { None }).collect();
        assert_eq!(calls, [("call_0", "glob"), ("call_1", "grep")], "ids are synthesized when the server omits them");
        assert!(matches!(ev.last(), Some(Event::Finish { reason: FinishReason::ToolUse, .. })), "{:?}", ev.last());
    }

    #[test]
    fn reasoning_length_and_missing_done() {
        let mut p = Parser::default();
        let ev = p.feed(r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#).unwrap();
        assert_eq!(ev, [Event::ReasoningDelta("hm".into())]);
        p.feed(r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#).unwrap();
        assert!(matches!(p.finish_event(), Some(Event::Finish { reason: FinishReason::MaxTokens, .. })));
        assert_eq!(p.finish_event(), None, "only once");
    }

    #[test]
    fn errors_inside_the_stream() {
        let e = parse(&[r#"{"error":{"message":"The server had an error","type":"server_error"}}"#]).unwrap_err();
        assert!(e.retryable() || matches!(e, ProviderError::InvalidRequest(_)));
        assert!(matches!(parse(&[r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#]), Err(ProviderError::ContentFilter(_))));
        let bad = parse(&[r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"read","arguments":"{\"a\":"}}]},"finish_reason":"tool_calls"}]}"#]).unwrap_err();
        assert!(matches!(bad, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn http_error_classification() {
        let b = |code: &str, msg: &str| format!(r#"{{"error":{{"message":"{msg}","type":"x","code":"{code}"}}}}"#);
        assert!(matches!(classify_http(401, &b("invalid_api_key", "bad key"), None), ProviderError::Auth(_)));
        assert!(matches!(classify_http(429, &b("rate_limit_exceeded", "slow down"), Some(3)), ProviderError::RateLimit { retry_after_ms: Some(3000), .. }));
        assert!(matches!(classify_http(429, &b("insufficient_quota", "You exceeded your current quota"), None), ProviderError::InvalidRequest(_)), "out of credit is not retried");
        assert!(matches!(classify_http(503, "upstream down", None), ProviderError::Internal { .. }));
        assert!(matches!(classify_http(400, &b("context_length_exceeded", "This model's maximum context length is 128000 tokens"), None), ProviderError::ContextOverflow(_)));
        assert!(matches!(classify_http(400, &b("", "bad param"), None), ProviderError::InvalidRequest(_)));
    }

    fn sample_request() -> Request {
        Request {
            model: "gpt-x".into(),
            system: "sys".into(),
            messages: vec![
                ChatMessage { role: Role::User, content: vec![Block::Text("hi".into())] },
                ChatMessage {
                    role: Role::Assistant,
                    content: vec![Block::Text("ok".into()), Block::ToolUse { id: "t1".into(), name: "read".into(), input: json!({"path":"x"}) }],
                },
                ChatMessage {
                    role: Role::User,
                    content: vec![Block::ToolResult { id: "t1".into(), content: "1: x".into(), is_error: false }, Block::Text("continue".into())],
                },
            ],
            tools: vec![ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object"}) }],
            max_tokens: 100,
            tools_disabled: false,
            ..Default::default()
        }
    }

    #[test]
    fn media_becomes_content_parts_after_the_text() {
        let req = Request {
            model: "m".into(),
            system: "s".into(),
            messages: vec![
                ChatMessage { role: Role::User, content: vec![Block::Text("what is this".into()), Block::Image { media_type: "image/jpeg".into(), data: "QQ==".into() }] },
                ChatMessage { role: Role::Assistant, content: vec![Block::ToolUse { id: "t".into(), name: "read".into(), input: json!({}) }] },
                // A tool result plus the image it produced: the tool message first, then the image as a user turn.
                ChatMessage {
                    role: Role::User,
                    content: vec![
                        Block::ToolResult { id: "t".into(), content: "Image file x.png".into(), is_error: false },
                        Block::Document { media_type: "application/pdf".into(), data: "UA==".into(), name: "a.pdf".into() },
                    ],
                },
            ],
            tools: vec![],
            max_tokens: 5,
            tools_disabled: false,
            ..Default::default()
        };
        let b = build_body(&req, false);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[1]["content"][0], json!({"type":"text","text":"what is this"}));
        assert_eq!(m[1]["content"][1]["image_url"]["url"], "data:image/jpeg;base64,QQ==");
        assert_eq!((m[3]["role"].as_str(), m[4]["role"].as_str()), (Some("tool"), Some("user")));
        assert_eq!(m[4]["content"][0], json!({"type":"file","file":{"filename":"a.pdf","file_data":"data:application/pdf;base64,UA=="}}));
        assert!(m[4]["content"].as_array().unwrap().len() == 1, "no empty text part");
    }

    #[test]
    fn request_body_shape() {
        let b = build_body(&sample_request(), false);
        let m = b["messages"].as_array().unwrap();
        let roles: Vec<_> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user"], "tool result precedes the user text");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"x\"}");
        assert_eq!(m[3]["tool_call_id"], "t1");
        assert_eq!(b["tools"][0]["function"]["parameters"], json!({"type":"object"}));
        assert_eq!((b["stream_options"]["include_usage"].clone(), b["max_tokens"].clone()), (json!(true), json!(100)));
        let mut r = sample_request();
        r.tools_disabled = true;
        r.messages.truncate(1);
        let b = build_body(&r, true);
        assert_eq!((b["tool_choice"].clone(), b["max_completion_tokens"].clone(), b.get("max_tokens")), (json!("none"), json!(100), None));
        r.messages = vec![ChatMessage { role: Role::Assistant, content: vec![Block::ToolUse { id: "a".into(), name: "n".into(), input: json!({}) }] }];
        assert_eq!(build_body(&r, false)["messages"][1]["content"], Value::Null, "tool-only assistant turn has null content");
    }

    fn run(server: &Server, key: Option<&str>) -> (Result<(), ProviderError>, Vec<Event>) {
        let p = OpenAiChat::new(key.map(String::from), format!("{}/v1", server.url), BTreeMap::from([("x-team".to_string(), "moco".to_string())]));
        let mut events = vec![];
        let r = p.stream(&sample_request(), &AtomicBool::new(false), &mut |e| events.push(e));
        (r, events)
    }

    #[test]
    fn http_round_trip_headers_and_stream() {
        let body: String = TOOL_STREAM.iter().map(|c| format!("data: {c}\n\n")).collect();
        let server = serve(vec![sse(&body)]);
        let (r, events) = run(&server, Some("sk-test"));
        assert_eq!(r, Ok(()));
        assert!(matches!(events.last(), Some(Event::Finish { reason: FinishReason::ToolUse, .. })));
        let (head, req_body) = server.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /v1/chat/completions"), "{head}");
        let h = head.to_lowercase();
        assert!(h.contains("authorization: bearer sk-test") && h.contains("x-team: moco"), "{head}");
        assert_eq!(serde_json::from_str::<Value>(&req_body).unwrap()["model"], "gpt-x");
    }

    #[test]
    fn keyless_servers_send_no_authorization_header() {
        let server = serve(vec![sse("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n")]);
        let (r, events) = run(&server, None);
        assert_eq!(r, Ok(()), "no [DONE] is fine once finish_reason was seen");
        assert_eq!(events[0], Event::TextDelta("hi".into()));
        assert!(!server.requests.lock().unwrap()[0].0.to_lowercase().contains("authorization"));
    }

    #[test]
    fn http_errors_and_truncated_streams() {
        let server = serve(vec![
            error(429, r#"{"error":{"message":"slow","code":"rate_limit_exceeded"}}"#, vec![("Retry-After", "7".into())]),
            sse("data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n"),
        ]);
        assert!(matches!(run(&server, Some("k")).0, Err(ProviderError::RateLimit { retry_after_ms: Some(7000), .. })));
        let (r, events) = run(&server, Some("k"));
        assert!(matches!(r, Err(ProviderError::Transport { .. })), "connection closed mid-response: {r:?}");
        assert_eq!(events, [Event::TextDelta("par".into())]);
    }
}
