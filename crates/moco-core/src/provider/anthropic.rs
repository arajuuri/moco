//! Anthropic Messages API (`POST {base}/v1/messages`, SSE).
use super::*;
use serde_json::json;
use std::collections::HashMap;

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
        crate::log::add_secret(&api_key);
        Anthropic {
            prompt_cache: true,
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()).trim_end_matches('/').to_string(),
            agent: new_agent(),
        }
    }
}

const EPHEMERAL: &str = "ephemeral";
const PROVIDER: &str = "anthropic";

fn mark(block: &mut Value) {
    block["cache_control"] = json!({"type": EPHEMERAL});
}

/// Request body. With `cache` on, up to four cache breakpoints are set (`Request::tail` goes after them): the last tool (covers all tool
/// definitions), the system prompt, the last block of the final message, and the last block of the message that
/// ended the previous request of an agent loop (so the previous step's cache entry is hit exactly, even if a
/// step added more content blocks than the cache lookback covers).
pub fn build_body(req: &Request, cache: bool) -> Value {
    // Thinking blocks go back only within the tool-use chain that is still running (after the last message the
    // user wrote): earlier ones are not needed, and replaying them after the history was edited (compaction,
    // undo, fork) would make newer API versions refuse the request.
    let chain_from = req
        .messages
        .iter()
        .rposition(|m| m.role == Role::User && m.content.iter().any(|b| matches!(b, Block::Text(_) | Block::Image { .. } | Block::Document { .. })))
        .map_or(0, |i| i + 1);
    let messages: Vec<Value> = req
        .messages
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let content: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| Some(match b {
                    Block::Text(t) => json!({"type":"text","text":t}),
                    Block::Image { media_type, data } => json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),
                    Block::Document { media_type, data, name } => {
                        json!({"type":"document","title":name,"source":{"type":"base64","media_type":media_type,"data":data}})
                    }
                    Block::ToolUse { id, name, input } => json!({"type":"tool_use","id":id,"name":name,"input":input}),
                    Block::ToolResult { id, content, is_error } => {
                        json!({"type":"tool_result","tool_use_id":id,"content":content,"is_error":is_error})
                    }
                    // The producing model's own thinking blocks, for the running tool-use chain only.
                    Block::Native { provider, data } if provider == PROVIDER && mi >= chain_from && data["model"].as_str().is_none_or(|x| x == req.model) => {
                        let mut d = data.clone();
                        d.as_object_mut().map(|o| o.remove("model"));
                        d
                    }
                    Block::Native { .. } => return None, // another provider's (or an older turn's) replay data
                }))
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
    // After the breakpoints, so a tail that changes from step to step never touches what the cache has seen.
    if let Some(tail) = &req.tail {
        let block = json!({"type":"text","text":tail});
        match messages.last_mut().filter(|m| m["role"] == "user").and_then(|m| m["content"].as_array_mut()) {
            Some(content) => content.push(block),
            None => messages.push(json!({"role":"user","content":[block]})),
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
    super::effort::shape_anthropic(&mut body, req);
    // Without thinking on, thinking blocks have no place in the history.
    let thinking_on = matches!(body["thinking"]["type"].as_str(), Some("adaptive") | Some("enabled"));
    if !thinking_on {
        if let Some(ms) = body["messages"].as_array_mut() {
            for m in ms {
                if let Some(c) = m["content"].as_array_mut() {
                    c.retain(|b| !matches!(b["type"].as_str(), Some("thinking") | Some("redacted_thinking")));
                }
            }
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
    /// A thinking block: its text, and the signature the API needs back (kept as replay data).
    Thinking { text: String, signature: String },
    /// A thinking block whose content the API encrypted; it must be passed back as it came.
    Redacted { data: String },
    Tool { id: String, name: String, json: String },
}

/// Incremental SSE decoder: feed `(event name, data)` pairs, get neutral events.
#[derive(Default)]
pub struct Parser {
    /// The model that produced the turn; stored with thinking blocks so they are only replayed to it.
    pub model: String,
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
                    "thinking" => BlockState::Thinking { text: String::new(), signature: b["signature"].as_str().unwrap_or("").to_string() },
                    "redacted_thinking" => BlockState::Redacted { data: b["data"].as_str().unwrap_or("").to_string() },
                    _ => BlockState::Text,
                };
                self.blocks.insert(idx, state);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => out.push(Event::TextDelta(d["text"].as_str().unwrap_or("").into())),
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        if let Some(BlockState::Thinking { text, .. }) = self.blocks.get_mut(&idx) {
                            text.push_str(t);
                        }
                        out.push(Event::ReasoningDelta(t.into()));
                    }
                    "signature_delta" => {
                        if let Some(BlockState::Thinking { signature, .. }) = self.blocks.get_mut(&idx) {
                            signature.push_str(d["signature"].as_str().unwrap_or(""));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(BlockState::Tool { json, .. }) = self.blocks.get_mut(&idx) {
                            json.push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                match self.blocks.remove(&idx) {
                    Some(BlockState::Thinking { text, signature }) if !signature.is_empty() => {
                        out.push(Event::Native { provider: PROVIDER.into(), data: json!({"type": "thinking", "thinking": text, "signature": signature, "model": self.model}) });
                    }
                    Some(BlockState::Redacted { data }) => {
                        out.push(Event::Native { provider: PROVIDER.into(), data: json!({"type": "redacted_thinking", "data": data, "model": self.model}) });
                    }
                    Some(BlockState::Tool { id, name, json }) => {
                        out.push(super::parse_tool_input(id, name, &json));
                    }
                    _ => {}
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

impl Provider for Anthropic {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let resp = self.agent.post(&url).set("x-api-key", &self.api_key).set("anthropic-version", "2023-06-01").set("content-type", "application/json");
        let body = build_body(req, self.prompt_cache).to_string();
        let mut shown = vec![("x-api-key", self.api_key.as_str()), ("anthropic-version", "2023-06-01")];
        shown.extend(req.overlay.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        super::trace_http(PROVIDER, &url, &shown, &body);
        let call = req.overlay.headers.iter().fold(resp, |r, (k, v)| r.set(k, v));
        let lines = super::open_lines(PROVIDER, call, body, cancel, classify_http)?;
        let mut parser = Parser { model: req.model.clone(), ..Default::default() };
        let mut finished = false;
        super::read_sse(lines, cancel, |data| {
            for ev in parser.feed(data)? {
                finished |= matches!(ev, Event::Finish { .. });
                on_event(ev);
            }
            Ok(false)
        })?;
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
        // The signature is replay data: stored with the turn, before the content it belongs to.
        assert_eq!(ev[1], Event::Native { provider: "anthropic".into(), data: json!({"type": "thinking", "thinking": "hm", "signature": "x", "model": ""}) });
        assert_eq!(ev[3], Event::ToolCall { id: "t".into(), name: "glob".into(), input: json!({}) });
    }

    #[test]
    fn truncated_tool_input_is_a_bad_call_and_earlier_calls_survive() {
        let ev = parse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"read"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"b","name":"write"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"y\",\"content\":\"abc"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .expect("a cut-off tool input is not a stream error");
        assert!(ev.contains(&Event::ToolCall { id: "a".into(), name: "read".into(), input: json!({"path":"x"}) }));
        assert!(ev.iter().any(|e| matches!(e, Event::BadToolCall { id, name, raw, .. } if id == "b" && name == "write" && raw.ends_with("\"abc"))), "{ev:?}");
        assert!(matches!(ev.last(), Some(Event::Finish { reason: FinishReason::MaxTokens, .. })));
    }

    #[test]
    fn redacted_thinking_and_unsigned_thinking() {
        let ev = parse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"ENC"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"no signature"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ])
        .unwrap();
        assert_eq!(ev[0], Event::Native { provider: "anthropic".into(), data: json!({"type": "redacted_thinking", "data": "ENC", "model": ""}) });
        assert_eq!(ev.len(), 2, "a thinking block without a signature cannot be replayed, so nothing is stored: {ev:?}");
    }

    fn thinking(model: &str, text: &str) -> Block {
        Block::Native { provider: "anthropic".into(), data: json!({"type": "thinking", "thinking": text, "signature": format!("sig-{text}"), "model": model}) }
    }

    fn turn(model: &str, think: &str, call: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage { role: Role::Assistant, content: vec![thinking(model, think), Block::ToolUse { id: call.into(), name: "read".into(), input: json!({}) }] },
            ChatMessage { role: Role::User, content: vec![Block::ToolResult { id: call.into(), content: "ok".into(), is_error: false }] },
        ]
    }

    fn types(b: &Value, i: usize) -> Vec<String> {
        b["messages"][i]["content"].as_array().unwrap().iter().map(|c| c["type"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn thinking_blocks_are_replayed_for_the_running_tool_chain_only() {
        let m = "claude-opus-5-5";
        let mut messages = vec![ChatMessage { role: Role::User, content: vec![Block::Text("first task".into())] }];
        messages.extend(turn(m, "one", "c1"));
        messages.push(ChatMessage { role: Role::Assistant, content: vec![thinking(m, "two"), Block::Text("done".into())] });
        messages.push(ChatMessage { role: Role::User, content: vec![Block::Text("second task".into())] });
        messages.extend(turn(m, "three", "c2"));
        let req = Request { model: m.into(), messages, effort: Some("high".into()), ..Default::default() };
        let b = build_body(&req, false);
        // Turn one (before the last thing the user wrote): thinking is left out. Turn two's chain keeps it, first in its message.
        assert_eq!(types(&b, 1), ["tool_use"]);
        assert_eq!(types(&b, 3), ["text"]);
        assert_eq!(types(&b, 5), ["thinking", "tool_use"]);
        assert_eq!(b["messages"][5]["content"][0], json!({"type": "thinking", "thinking": "three", "signature": "sig-three"}), "the model tag is not sent");
        assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(b["output_config"], json!({"effort": "high"}));
    }

    #[test]
    fn thinking_blocks_of_other_models_or_without_thinking_are_not_sent() {
        let mut messages = vec![ChatMessage { role: Role::User, content: vec![Block::Text("task".into())] }];
        messages.extend(turn("claude-opus-5-5", "old model", "c1"));
        let ask = |model: &str, effort: Option<&str>| {
            let r = Request { model: model.into(), messages: messages.clone(), effort: effort.map(String::from), ..Default::default() };
            types(&build_body(&r, false), 1)
        };
        assert_eq!(ask("claude-opus-5-5", None), ["thinking", "tool_use"]);
        assert_eq!(ask("claude-sonnet-5-5", None), ["tool_use"], "the model changed in the middle of the chain");
        // Thinking switched off (or never on): blocks would be out of place.
        assert_eq!(ask("claude-opus-4-8", None), ["tool_use"]);
        let mut messages2 = messages.clone();
        messages2[1].content[0] = thinking("claude-opus-4-8", "x");
        let r = Request { model: "claude-opus-4-8".into(), messages: messages2, effort: Some("none".into()), ..Default::default() };
        assert_eq!(types(&build_body(&r, false), 1), ["tool_use"]);
        // Redacted blocks go back as they came.
        let mut m3 = messages.clone();
        m3[1].content[0] = Block::Native { provider: "anthropic".into(), data: json!({"type": "redacted_thinking", "data": "ENC", "model": "claude-opus-5-5"}) };
        let r = Request { model: "claude-opus-5-5".into(), messages: m3, ..Default::default() };
        assert_eq!(build_body(&r, false)["messages"][1]["content"][0], json!({"type": "redacted_thinking", "data": "ENC"}));
    }

    #[test]
    fn invalid_tool_json_becomes_a_bad_tool_call() {
        let ev = parse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"read"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ])
        .unwrap();
        assert!(ev.iter().any(|e| matches!(e, Event::BadToolCall { id, name, raw, .. } if id == "t" && name == "read" && raw == "{\"a\":")), "{ev:?}");
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
            ..Default::default()
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
    fn the_tail_goes_after_the_breakpoints_and_never_disturbs_the_cached_prefix() {
        let plain = build_body(&agent_loop_request(2), true);
        let mut req = agent_loop_request(2);
        req.tail = Some("<system-reminder>todo</system-reminder>".into());
        let with = build_body(&req, true);
        let (mp, mw) = (plain["messages"].as_array().unwrap(), with["messages"].as_array().unwrap());
        assert_eq!(mp.len(), mw.len(), "added to the final user message, not as a new one");
        let last = mw.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(last.last().unwrap(), &json!({"type":"text","text":"<system-reminder>todo</system-reminder>"}));
        assert!(!marked(last.last().unwrap()) && marked(&last[last.len() - 2]), "the breakpoint stays on the last block of the history");
        // Everything but the tail block is what it would be without it, so a changing tail never misses the cache.
        let mut stripped = with.clone();
        stripped["messages"].as_array_mut().unwrap().last_mut().unwrap()["content"].as_array_mut().unwrap().pop();
        assert_eq!(stripped, plain);
        assert_eq!(with.to_string().matches("cache_control").count(), 4);
        // A history that ends with the assistant gets a user message of its own.
        req.messages.push(ChatMessage { role: Role::Assistant, content: vec![Block::Text("hm".into())] });
        let b = build_body(&req, true);
        let m = b["messages"].as_array().unwrap();
        assert_eq!((m.last().unwrap()["role"].clone(), m.len()), (json!("user"), mp.len() + 2));
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
            ..Default::default()
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
            ..Default::default()
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
