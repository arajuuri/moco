//! Anthropic Messages API (`POST {base}/v1/messages`, SSE).
use super::*;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub struct Anthropic {
    /// Mark the prompt prefix for Anthropic's prompt cache (`cache_control`). On by default.
    pub prompt_cache: bool,
    pub api_key: String,
    /// Without trailing `/v1` (default `https://api.anthropic.com`).
    pub base_url: String,
    agent: ureq::Agent,
}

impl Anthropic {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Anthropic {
            prompt_cache: true,
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()).trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(20)).timeout_read(Duration::from_secs(120)).build(),
        }
    }
}

const EPHEMERAL: &str = "ephemeral";

fn mark(block: &mut Value) {
    block["cache_control"] = json!({"type": EPHEMERAL});
}

/// Request body. With `cache` on, up to four cache breakpoints are set: the last tool (covers all tool
/// definitions), the system prompt, the last block of the final message, and the last block of the message that
/// ended the previous request of an agent loop (so the previous step's cache entry is hit exactly, even if a
/// step added more content blocks than the cache lookback covers).
pub fn build_body(req: &Request, cache: bool) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let content: Vec<Value> = m
                .content
                .iter()
                .map(|b| match b {
                    Block::Text(t) => json!({"type":"text","text":t}),
                    Block::Image { media_type, data } => json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),
                    Block::Document { media_type, data, name } => {
                        json!({"type":"document","title":name,"source":{"type":"base64","media_type":media_type,"data":data}})
                    }
                    Block::ToolUse { id, name, input } => json!({"type":"tool_use","id":id,"name":name,"input":input}),
                    Block::ToolResult { id, content, is_error } => {
                        json!({"type":"tool_result","tool_use_id":id,"content":content,"is_error":is_error})
                    }
                })
                .collect();
            json!({"role": if m.role == Role::User {"user"} else {"assistant"}, "content": content})
        })
        .collect();
    let mut messages = messages;
    if cache {
        let n = messages.len();
        for idx in [n.checked_sub(1), n.checked_sub(3)].into_iter().flatten() {
            if let Some(last) = messages[idx]["content"].as_array_mut().and_then(|c| c.last_mut()) {
                mark(last);
            }
        }
    }
    let mut body = json!({"model":req.model,"max_tokens":req.max_tokens,"stream":true,"messages":messages});
    if !req.system.is_empty() {
        body["system"] = if cache {
            json!([{"type":"text","text":req.system,"cache_control":{"type":EPHEMERAL}}])
        } else {
            json!(req.system)
        };
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.schema})).collect();
        if let (true, Some(last)) = (cache, tools.last_mut()) {
            mark(last);
        }
        body["tools"] = json!(tools);
        if req.tools_disabled {
            body["tool_choice"] = json!({"type":"none"});
        }
    }
    body
}

/// Map an HTTP failure to a neutral error. `retry_after` is the `retry-after` header in seconds.
pub fn classify_http(status: u16, body: &str, retry_after: Option<u64>) -> ProviderError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let kind = v["error"]["type"].as_str().unwrap_or("");
    let msg = v["error"]["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}: {}", body.chars().take(300).collect::<String>()));
    classify(status, kind, msg, retry_after.map(|s| s * 1000))
}

fn classify(status: u16, kind: &str, msg: String, retry_after_ms: Option<u64>) -> ProviderError {
    let lower = msg.to_lowercase();
    let overflow = lower.contains("prompt is too long") || lower.contains("exceed") && lower.contains("context") || kind == "request_too_large" || status == 413;
    match (status, kind) {
        _ if overflow => ProviderError::ContextOverflow(msg),
        (401 | 403, _) | (_, "authentication_error" | "permission_error") => ProviderError::Auth(msg),
        (429, _) | (_, "rate_limit_error") => ProviderError::RateLimit { retry_after_ms, message: msg },
        (500..=599, _) | (_, "api_error" | "overloaded_error") => ProviderError::Internal { retry_after_ms, message: msg },
        _ if lower.contains("content") && lower.contains("filter") => ProviderError::ContentFilter(msg),
        _ => ProviderError::InvalidRequest(msg),
    }
}

enum BlockState {
    Text,
    Thinking,
    Tool { id: String, name: String, json: String },
}

/// Incremental SSE decoder: feed `(event name, data)` pairs, get neutral events.
#[derive(Default)]
pub struct Parser {
    blocks: HashMap<u64, BlockState>,
    usage: Usage,
    stop: Option<String>,
}

impl Parser {
    pub fn feed(&mut self, data: &str) -> Result<Vec<Event>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Internal { retry_after_ms: None, message: format!("bad SSE JSON: {e}") })?;
        let idx = v["index"].as_u64().unwrap_or(0);
        let mut out = vec![];
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.usage.output = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let b = &v["content_block"];
                let state = match b["type"].as_str().unwrap_or("") {
                    "tool_use" => {
                        let (id, name) = (b["id"].as_str().unwrap_or("").to_string(), b["name"].as_str().unwrap_or("").to_string());
                        out.push(Event::ToolInputStart { id: id.clone(), name: name.clone() });
                        BlockState::Tool { id, name, json: String::new() }
                    }
                    "thinking" | "redacted_thinking" => BlockState::Thinking,
                    _ => BlockState::Text,
                };
                self.blocks.insert(idx, state);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => out.push(Event::TextDelta(d["text"].as_str().unwrap_or("").into())),
                    "thinking_delta" => out.push(Event::ReasoningDelta(d["thinking"].as_str().unwrap_or("").into())),
                    "input_json_delta" => {
                        if let Some(BlockState::Tool { json, .. }) = self.blocks.get_mut(&idx) {
                            json.push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {} // signature_delta etc.
                }
            }
            "content_block_stop" => {
                if let Some(BlockState::Tool { id, name, json }) = self.blocks.remove(&idx) {
                    let input = if json.trim().is_empty() { Value::Object(Default::default()) } else {
                        serde_json::from_str(&json).map_err(|e| ProviderError::InvalidRequest(format!("model produced invalid tool input JSON for {name}: {e}")))?
                    };
                    out.push(Event::ToolCall { id, name, input });
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(r.to_string());
                }
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output = o;
                }
            }
            "message_stop" => {
                let reason = match self.stop.as_deref() {
                    Some("end_turn") | Some("stop_sequence") | None => FinishReason::EndTurn,
                    Some("tool_use") => FinishReason::ToolUse,
                    Some("max_tokens") => FinishReason::MaxTokens,
                    Some(o) => FinishReason::Other(o.into()),
                };
                out.push(Event::Finish { reason, usage: self.usage });
            }
            "error" => {
                let e = &v["error"];
                let msg = e["message"].as_str().unwrap_or("provider error").to_string();
                return Err(classify(0, e["type"].as_str().unwrap_or(""), msg, None));
            }
            _ => {} // ping
        }
        Ok(out)
    }
}

fn transport(e: ureq::Transport) -> ProviderError {
    let kind = match e.kind() {
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => TransportKind::Connect,
        _ => {
            if e.to_string().to_lowercase().contains("timed out") { TransportKind::Timeout } else { TransportKind::Other }
        }
    };
    ProviderError::Transport { kind, message: e.to_string() }
}

impl Provider for Anthropic {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let resp = self
            .agent
            .post(&format!("{}/v1/messages", self.base_url))
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_string(&build_body(req, self.prompt_cache).to_string());
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok());
                let body = r.into_string().unwrap_or_default();
                return Err(classify_http(code, &body, ra));
            }
            Err(ureq::Error::Transport(t)) => return Err(transport(t)),
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
        // The stream must end with message_stop; a silent close means a dropped connection.
        if finished {
            Ok(())
        } else {
            Err(ProviderError::Transport { kind: TransportKind::Other, message: "stream ended before message_stop".into() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOL_STREAM: &[&str] = &[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"look."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.rs\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        r#"{"type":"message_stop"}"#,
    ];

    fn parse(lines: &[&str]) -> Result<Vec<Event>, ProviderError> {
        let mut p = Parser::default();
        let mut all = vec![];
        for l in lines {
            all.extend(p.feed(l)?);
        }
        Ok(all)
    }

    #[test]
    fn text_and_tool_call_stream() {
        let ev = parse(TOOL_STREAM).unwrap();
        assert_eq!(
            ev,
            vec![
                Event::TextDelta("Let me ".into()),
                Event::TextDelta("look.".into()),
                Event::ToolInputStart { id: "toolu_1".into(), name: "read".into() },
                Event::ToolCall { id: "toolu_1".into(), name: "read".into(), input: json!({"path":"a.rs"}) },
                Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 25, output: 42, cache_read: 5, cache_write: 0 } },
            ]
        );
    }

    #[test]
    fn empty_tool_input_is_an_object_and_thinking_is_reasoning() {
        let ev = parse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"glob"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"ping"}"#,
        ])
        .unwrap();
        assert_eq!(ev[0], Event::ReasoningDelta("hm".into()));
        assert_eq!(ev[2], Event::ToolCall { id: "t".into(), name: "glob".into(), input: json!({}) });
    }

    #[test]
    fn invalid_tool_json_is_a_non_retryable_error() {
        let e = parse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"read"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ])
        .unwrap_err();
        assert!(matches!(e, ProviderError::InvalidRequest(_)) && !e.retryable());
    }

    #[test]
    fn in_stream_error_event() {
        let e = parse(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]).unwrap_err();
        assert!(matches!(e, ProviderError::Internal { .. }) && e.retryable(), "{e:?}");
    }

    #[test]
    fn http_error_classification() {
        let body = |t: &str, m: &str| format!(r#"{{"type":"error","error":{{"type":"{t}","message":"{m}"}}}}"#);
        assert!(matches!(classify_http(401, &body("authentication_error", "bad key"), None), ProviderError::Auth(_)));
        assert!(matches!(classify_http(429, &body("rate_limit_error", "slow"), Some(7)), ProviderError::RateLimit { retry_after_ms: Some(7000), .. }));
        assert!(matches!(classify_http(529, &body("overloaded_error", "busy"), None), ProviderError::Internal { .. }));
        assert!(matches!(classify_http(500, "not json", None), ProviderError::Internal { .. }));
        assert!(matches!(classify_http(400, &body("invalid_request_error", "prompt is too long: 250000 tokens > 200000 maximum"), None), ProviderError::ContextOverflow(_)));
        assert!(matches!(classify_http(413, &body("request_too_large", "Request exceeds the maximum size"), None), ProviderError::ContextOverflow(_)));
        assert!(matches!(classify_http(400, &body("invalid_request_error", "messages: bad"), None), ProviderError::InvalidRequest(_)));
    }

    fn marked(v: &Value) -> bool {
        v.get("cache_control") == Some(&json!({"type":"ephemeral"}))
    }

    fn agent_loop_request(extra_turns: usize) -> Request {
        let mut messages = vec![ChatMessage { role: Role::User, content: vec![Block::Text("task".into())] }];
        for i in 0..extra_turns {
            messages.push(ChatMessage { role: Role::Assistant, content: vec![Block::Text(format!("a{i}")), Block::ToolUse { id: format!("t{i}"), name: "read".into(), input: json!({}) }] });
            messages.push(ChatMessage { role: Role::User, content: vec![Block::ToolResult { id: format!("t{i}"), content: "r".into(), is_error: false }] });
        }
        Request {
            model: "m".into(),
            system: "sys".into(),
            messages,
            tools: vec![
                ToolDef { name: "read".into(), description: "d".into(), schema: json!({}) },
                ToolDef { name: "shell".into(), description: "d".into(), schema: json!({}) },
            ],
            max_tokens: 10,
            tools_disabled: false,
        }
    }

    #[test]
    fn cache_breakpoints_are_placed_and_capped_at_four() {
        let b = build_body(&agent_loop_request(2), true);
        // messages: user, assistant, user(results), assistant, user(results)
        let m = b["messages"].as_array().unwrap();
        let marked_msgs: Vec<usize> = (0..m.len()).filter(|i| m[*i]["content"].as_array().unwrap().iter().any(marked)).collect();
        assert_eq!(marked_msgs, [2, 4], "the final message and the one that ended the previous request");
        assert!(marked(&m[4]["content"][0]) && marked(&b["system"][0]) && marked(&b["tools"][1]) && !marked(&b["tools"][0]));
        assert_eq!(b["system"][0]["text"], "sys");
        assert_eq!(b.to_string().matches("cache_control").count(), 4, "Anthropic allows at most four");
        // Short conversations: just the last message; no system or tools means fewer markers.
        let b = build_body(&agent_loop_request(0), true);
        assert_eq!(b.to_string().matches("cache_control").count(), 3);
        let mut r = agent_loop_request(1);
        r.system.clear();
        r.tools.clear();
        let b = build_body(&r, true);
        assert!(b.get("system").is_none() && b.get("tools").is_none());
        assert_eq!(b.to_string().matches("cache_control").count(), 2, "three messages: the final one, and the task message where the first step's request ended");
        r.messages.truncate(2);
        assert_eq!(build_body(&r, true).to_string().matches("cache_control").count(), 1, "two messages: only the final one");
    }

    #[test]
    fn consecutive_agent_loop_requests_share_their_marked_prefix() {
        // What the cache sees: everything up to a marker must be byte-identical between steps.
        let strip = |mut v: Value| {
            fn walk(v: &mut Value) {
                match v {
                    Value::Object(o) => {
                        o.remove("cache_control");
                        o.values_mut().for_each(walk);
                    }
                    Value::Array(a) => a.iter_mut().for_each(walk),
                    _ => {}
                }
            }
            walk(&mut v);
            v
        };
        let (a, b) = (build_body(&agent_loop_request(1), true), build_body(&agent_loop_request(2), true));
        assert_eq!(strip(a["tools"].clone()), strip(b["tools"].clone()));
        assert_eq!(strip(a["system"].clone()), strip(b["system"].clone()));
        let (ma, mb) = (a["messages"].as_array().unwrap(), b["messages"].as_array().unwrap());
        assert_eq!(strip(json!(ma)), strip(json!(&mb[..ma.len()])), "the new request extends the old one");
        // The old request's final message is marked in the new request too: the previous cache entry is hit exactly.
        assert!(marked(&mb[ma.len() - 1]["content"][0]));
    }

    #[test]
    fn images_and_documents_use_base64_sources_and_can_carry_cache_markers() {
        let req = Request {
            model: "m".into(),
            system: String::new(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![
                    Block::Text("look".into()),
                    Block::Image { media_type: "image/png".into(), data: "AAAA".into() },
                    Block::Document { media_type: "application/pdf".into(), data: "BBBB".into(), name: "spec.pdf".into() },
                ],
            }],
            tools: vec![],
            max_tokens: 10,
            tools_disabled: false,
        };
        let b = build_body(&req, true);
        let c = &b["messages"][0]["content"];
        assert_eq!(c[1], json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}));
        assert_eq!(c[2]["type"], "document");
        assert_eq!((c[2]["title"].clone(), c[2]["source"]["media_type"].clone()), (json!("spec.pdf"), json!("application/pdf")));
        assert!(marked(&c[2]) && !marked(&c[1]), "the breakpoint goes on the last block, here the document");
    }

    #[test]
    fn request_body_shape() {
        let req = Request {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![
                ChatMessage { role: Role::User, content: vec![Block::Text("hi".into())] },
                ChatMessage { role: Role::Assistant, content: vec![Block::ToolUse { id: "t1".into(), name: "read".into(), input: json!({"path":"x"}) }] },
                ChatMessage { role: Role::User, content: vec![Block::ToolResult { id: "t1".into(), content: "ok".into(), is_error: false }] },
            ],
            tools: vec![ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object"}) }],
            max_tokens: 100,
            tools_disabled: true,
        };
        let b = build_body(&req, false);
        assert_eq!(b["stream"], true);
        assert_eq!(b["system"], "sys");
        assert!(!b.to_string().contains("cache_control"), "no markers when caching is off");
        assert_eq!(b["tool_choice"], json!({"type":"none"}));
        assert_eq!(b["tools"][0]["input_schema"], json!({"type":"object"}));
        assert_eq!(b["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(b["messages"][2]["content"][0]["tool_use_id"], "t1");
    }
}
