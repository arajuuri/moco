//! OpenAI Responses API (`POST {base}/responses`, SSE), the default protocol for the built-in `openai` provider (archive#185).
//! Verified against api.openai.com (#6: plain replies, tool turns with reasoning replay, effort levels, `--continue`);
//! `docs/07-providers.md` lists what has not been tried live yet.
//!
//! Stateless (`store: false`): the conversation is sent in full every time. Reasoning items are requested with
//! `include: ["reasoning.encrypted_content"]`, kept as `Block::Native` and replayed, together with the function-call
//! items they came with, exactly as the API returned them.
use super::*;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

pub const PROVIDER: &str = "openai-responses";

pub struct OpenAiResponses {
    pub api_key: Option<String>,
    /// Includes the version path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
    agent: ureq::Agent,
}

impl OpenAiResponses {
    pub fn new(api_key: Option<String>, base_url: String, headers: BTreeMap<String, String>) -> Self {
        if let Some(k) = &api_key {
            crate::log::add_secret(k);
        }
        OpenAiResponses {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            headers,
            agent: new_agent(),
        }
    }
}

pub fn build_body(req: &Request) -> Value {
    let mut input: Vec<Value> = vec![];
    for m in req.messages_with_tail().iter() {
        match m.role {
            Role::User => {
                // Tool results are their own items and must follow the calls; the user's content comes after.
                let mut parts: Vec<Value> = vec![];
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => input.push(json!({"type":"function_call_output","call_id":id,"output":content})),
                        Block::Text(t) => parts.push(json!({"type":"input_text","text":t})),
                        Block::Image { media_type, data } => parts.push(json!({"type":"input_image","image_url":format!("data:{media_type};base64,{data}")})),
                        Block::Document { media_type, data, name } => {
                            parts.push(json!({"type":"input_file","filename":name,"file_data":format!("data:{media_type};base64,{data}")}))
                        }
                        Block::ToolUse { .. } | Block::Native { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    input.push(json!({"role":"user","content":parts}));
                }
            }
            Role::Assistant => {
                // The function_call items as the API sent them, by call id (they carry ids the API may want back).
                let originals: HashMap<&str, &Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Native { provider, data } if provider == PROVIDER && data["type"] == "function_call" => Some((data["call_id"].as_str()?, data)),
                        _ => None,
                    })
                    .collect();
                let mut text: Vec<&str> = vec![];
                let flush = |text: &mut Vec<&str>, input: &mut Vec<Value>| {
                    if !text.is_empty() {
                        input.push(json!({"role":"assistant","content":[{"type":"output_text","text":text.join("\n\n")}]}));
                        text.clear();
                    }
                };
                for b in &m.content {
                    match b {
                        Block::Text(t) => text.push(t),
                        // Reasoning items are replayed in place; function_call items are emitted with their ToolUse below.
                        Block::Native { provider, data } if provider == PROVIDER && data["type"] != "function_call" => {
                            flush(&mut text, &mut input);
                            input.push(data.clone());
                        }
                        Block::ToolUse { id, name, input: args } => {
                            flush(&mut text, &mut input);
                            input.push(originals.get(id.as_str()).map(|v| (*v).clone()).unwrap_or_else(|| json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})));
                        }
                        _ => {}
                    }
                }
                flush(&mut text, &mut input);
            }
        }
    }
    let mut body = json!({"model":req.model,"input":input,"stream":true,"store":false,"max_output_tokens":req.max_tokens,"include":["reasoning.encrypted_content"]});
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        body["tools"] = req.tools.iter().map(|t| json!({"type":"function","name":t.name,"description":t.description,"parameters":t.schema,"strict":false})).collect();
        if req.tools_disabled {
            body["tool_choice"] = json!("none");
        }
    }
    super::effort::shape_openai_responses(&mut body, req);
    body
}

/// Failures reported inside the stream (`response.failed`, `error`).
fn classify_event(code: &str, message: &str) -> ProviderError {
    let msg = if message.is_empty() { code.to_string() } else { message.to_string() };
    let lower = msg.to_lowercase();
    if code == "context_length_exceeded" || lower.contains("context window") || lower.contains("maximum context length") {
        return ProviderError::ContextOverflow(msg);
    }
    if super::openai_chat::out_of_credit(code, &msg) {
        return super::openai_chat::no_credit(msg);
    }
    match code {
        "invalid_api_key" | "authentication_error" | "permission_denied" => ProviderError::Auth(msg),
        "rate_limit_exceeded" => ProviderError::RateLimit { retry_after_ms: None, message: msg },
        "server_error" | "overloaded" | "internal_error" | "service_unavailable" | "timeout" => ProviderError::Internal { retry_after_ms: None, message: msg },
        _ => ProviderError::InvalidRequest(msg),
    }
}

/// Incremental decoder for the event stream.
#[derive(Default)]
pub struct Parser {
    calls: bool,
    finished: bool,
}

impl Parser {
    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn feed(&mut self, data: &str) -> Result<Vec<Event>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Internal { retry_after_ms: None, message: format!("bad SSE JSON: {e}") })?;
        let mut out = vec![];
        match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    out.push(Event::TextDelta(d.into()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    out.push(Event::ReasoningDelta(d.into()));
                }
            }
            "response.output_item.added" => {
                let item = &v["item"];
                if item["type"] == "function_call" {
                    out.push(Event::ToolInputStart { id: item["call_id"].as_str().unwrap_or("").into(), name: item["name"].as_str().unwrap_or("").into() });
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("function_call") => {
                        let (id, name) = (item["call_id"].as_str().unwrap_or("").to_string(), item["name"].as_str().unwrap_or("").to_string());
                        let args = item["arguments"].as_str().unwrap_or("");
                        self.calls = true;
                        let call = super::parse_tool_input(id, name, args);
                        // The item's malformed arguments must not be replayed to the service.
                        if matches!(call, Event::ToolCall { .. }) {
                            out.push(Event::Native { provider: PROVIDER.into(), data: item.clone() });
                        }
                        out.push(call);
                    }
                    Some("reasoning") => out.push(Event::Native { provider: PROVIDER.into(), data: item.clone() }),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = &v["response"];
                let u = &r["usage"];
                let get = |k: &str| u[k].as_u64().unwrap_or(0);
                let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                let usage = Usage { input: get("input_tokens").saturating_sub(cached), output: get("output_tokens"), cache_read: cached, cache_write: 0 };
                let reason = match r["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => FinishReason::MaxTokens,
                    Some("content_filter") => return Err(ProviderError::ContentFilter("response blocked by the provider's content filter".into())),
                    Some(other) => FinishReason::Other(other.into()),
                    None if self.calls => FinishReason::ToolUse,
                    None => FinishReason::EndTurn,
                };
                self.finished = true;
                out.push(Event::Finish { reason, usage });
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                return Err(classify_event(e["code"].as_str().unwrap_or(""), e["message"].as_str().unwrap_or("the response failed")));
            }
            "error" => return Err(classify_event(v["code"].as_str().unwrap_or(""), v["message"].as_str().unwrap_or("provider error"))),
            _ => {} // created, in_progress, content_part.*, *.done, function_call_arguments.delta, ...
        }
        Ok(out)
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let url = format!("{}/responses", self.base_url);
        let mut call = self.agent.post(&url).set("content-type", "application/json");
        if let Some(k) = &self.api_key {
            call = call.set("authorization", &format!("Bearer {k}"));
        }
        for (k, v) in self.headers.iter().chain(req.overlay.headers.iter().map(|(k, v)| (k, v))) {
            call = call.set(k, v);
        }
        let body = build_body(req).to_string();
        let mut shown: Vec<(&str, &str)> = self.api_key.iter().map(|k| ("authorization", k.as_str())).collect();
        shown.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain(req.overlay.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))));
        super::trace_http("openai-responses", &url, &shown, &body);
        let lines = super::open_lines("openai-responses", call, body, cancel, super::openai_chat::classify_http)?;
        let mut parser = Parser::default();
        read_sse(lines, cancel, |data| {
            for ev in parser.feed(data)? {
                on_event(ev);
            }
            Ok(parser.finished())
        })?;
        if parser.finished() {
            Ok(())
        } else {
            Err(ProviderError::Transport { kind: TransportKind::Other, message: "stream ended before the response completed".into() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testserver::*;
    use super::*;

    /// A turn the way the API streams it: a reasoning item, then a function call, then the end.
    const STREAM: &[&str] = &[
        r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Need to look"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC","summary":[{"type":"summary_text","text":"Need to look"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#,
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Let me "}"#,
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"read it."}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Let me read it."}]}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"","status":"in_progress"}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"{\"path\":"}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":2,"arguments":"{\"path\":\"a.rs\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.rs\"}","status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":150}}}"#,
    ];

    fn parse(chunks: &[&str]) -> Result<Vec<Event>, ProviderError> {
        let mut p = Parser::default();
        let mut all = vec![];
        for c in chunks {
            all.extend(p.feed(c)?);
        }
        Ok(all)
    }

    fn reasoning_item() -> Value {
        json!({"type":"reasoning","id":"rs_1","encrypted_content":"ENC","summary":[{"type":"summary_text","text":"Need to look"}]})
    }
    fn call_item() -> Value {
        json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.rs\"}","status":"completed"})
    }

    #[test]
    fn a_turn_with_reasoning_text_and_a_tool_call() {
        assert_eq!(
            parse(STREAM).unwrap(),
            vec![
                Event::ReasoningDelta("Need to look".into()),
                Event::Native { provider: PROVIDER.into(), data: reasoning_item() },
                Event::TextDelta("Let me ".into()),
                Event::TextDelta("read it.".into()),
                Event::ToolInputStart { id: "call_1".into(), name: "read".into() },
                Event::Native { provider: PROVIDER.into(), data: call_item() },
                Event::ToolCall { id: "call_1".into(), name: "read".into(), input: json!({"path":"a.rs"}) },
                Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 20, output: 30, cache_read: 100, cache_write: 0 } },
            ]
        );
    }

    #[test]
    fn ends_other_than_a_clean_completion() {
        let end = |t: &str| parse(&[t]);
        assert!(matches!(end(r#"{"type":"response.completed","response":{"status":"completed","usage":{}}}"#).unwrap()[0], Event::Finish { reason: FinishReason::EndTurn, .. }));
        assert!(matches!(end(r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":5}}}"#).unwrap()[0], Event::Finish { reason: FinishReason::MaxTokens, usage: Usage { output: 5, .. } }));
        assert!(matches!(end(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#), Err(ProviderError::ContentFilter(_))));
        let overflow = end(r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}}"#).unwrap_err();
        assert!(matches!(overflow, ProviderError::ContextOverflow(_)));
        assert!(matches!(end(r#"{"type":"error","code":"rate_limit_exceeded","message":"slow down"}"#), Err(ProviderError::RateLimit { .. })));
        assert!(matches!(end(r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"oops"}}}"#), Err(ProviderError::Internal { .. })));
        assert!(matches!(end(r#"{"type":"error","code":"invalid_api_key","message":"bad key"}"#), Err(ProviderError::Auth(_))));
        assert!(matches!(end(r#"{"type":"error","code":"insufficient_quota","message":"out of credit"}"#), Err(ProviderError::InvalidRequest(_))), "not retried");
        // Issue archive#187: the newer out-of-credit message is not retried whatever code it carries.
        let no_credits = "You have no credits remaining. Add credits to continue using the API at https://platform.openai.com/settings/organization/billing/.";
        for code in ["", "insufficient_quota", "rate_limit_exceeded"] {
            let failed = format!(r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}","message":"{no_credits}"}}}}}}"#);
            assert!(matches!(end(&failed), Err(ProviderError::InvalidRequest(m)) if m == no_credits), "response.failed code {code:?}");
            let error = format!(r#"{{"type":"error","code":"{code}","message":"{no_credits}"}}"#);
            assert!(matches!(end(&error), Err(ProviderError::InvalidRequest(_))), "error code {code:?}");
        }
        let rpm = "Rate limit reached for gpt-6.1-terra on requests per min (RPM). You can increase your rate limit by adding a payment method to your account at https://platform.openai.com/account/billing.";
        assert!(matches!(end(&format!(r#"{{"type":"error","code":"rate_limit_exceeded","message":"{rpm}"}}"#)), Err(ProviderError::RateLimit { .. })), "a real rate limit is still retried");
        let bad = end(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"read","arguments":"{\"a\":"}}"#).unwrap();
        assert!(matches!(bad.as_slice(), [Event::BadToolCall { id, name, raw, .. }] if id == "c" && name == "read" && raw == "{\"a\":"), "{bad:?}");
        assert_eq!(end(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"glob","arguments":""}}"#).unwrap()[1], Event::ToolCall { id: "c".into(), name: "glob".into(), input: json!({}) });
    }

    fn request() -> Request {
        Request {
            model: "gpt-x".into(),
            system: "sys".into(),
            messages: vec![
                ChatMessage { role: Role::User, content: vec![Block::Text("read a.rs".into()), Block::Image { media_type: "image/png".into(), data: "QQ==".into() }] },
                ChatMessage {
                    role: Role::Assistant,
                    content: vec![
                        Block::Native { provider: PROVIDER.into(), data: reasoning_item() },
                        Block::Text("Let me read it.".into()),
                        Block::Native { provider: PROVIDER.into(), data: call_item() },
                        Block::ToolUse { id: "call_1".into(), name: "read".into(), input: json!({"path":"a.rs"}) },
                        Block::Native { provider: "gemini".into(), data: json!({"call_id":"call_1","thoughtSignature":"other provider"}) },
                    ],
                },
                ChatMessage { role: Role::User, content: vec![Block::ToolResult { id: "call_1".into(), content: "1: fn a() {}".into(), is_error: false }, Block::Text("thanks".into())] },
            ],
            tools: vec![ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object"}) }],
            max_tokens: 100,
            tools_disabled: false,
            ..Default::default()
        }
    }

    #[test]
    fn request_body_replays_reasoning_and_calls_verbatim_and_statelessly() {
        let b = build_body(&request());
        assert_eq!((b["instructions"].clone(), b["store"].clone(), b["stream"].clone(), b["max_output_tokens"].clone()), (json!("sys"), json!(false), json!(true), json!(100)));
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(b["tools"][0], json!({"type":"function","name":"read","description":"d","parameters":{"type":"object"},"strict":false}));
        let i = b["input"].as_array().unwrap();
        assert_eq!(i[0], json!({"role":"user","content":[{"type":"input_text","text":"read a.rs"},{"type":"input_image","image_url":"data:image/png;base64,QQ=="}]}));
        // Reasoning first, then the text, then the original function_call item (with its fc_ id), nothing from other providers.
        assert_eq!(i[1], reasoning_item());
        assert_eq!(i[2], json!({"role":"assistant","content":[{"type":"output_text","text":"Let me read it."}]}));
        assert_eq!(i[3], call_item());
        assert_eq!(i[4], json!({"type":"function_call_output","call_id":"call_1","output":"1: fn a() {}"}));
        assert_eq!(i[5], json!({"role":"user","content":[{"type":"input_text","text":"thanks"}]}));
        assert_eq!(i.len(), 6);
        // Without replay data (history from another provider) a function_call is built from the tool use.
        let mut r = request();
        r.messages[1].content.retain(|b| !matches!(b, Block::Native { .. }));
        let i = build_body(&r)["input"].as_array().unwrap().clone();
        assert_eq!(i[2], json!({"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.rs\"}"}));
        r.tools_disabled = true;
        assert_eq!(build_body(&r)["tool_choice"], "none");
    }

    /// Sessions started before Responses became OpenAI's default (archive#185) hold Chat Completions history (`call_` ids, no
    /// replay data), and a session may have run on Anthropic first (`toolu_` ids, thinking blocks). Both must continue:
    /// every call becomes a plain `function_call` item (no `fc_` id, which the API would check) followed by its output.
    #[test]
    fn history_from_chat_completions_and_anthropic_continues_on_responses() {
        let thinking = Block::Native { provider: "anthropic".into(), data: json!({"type":"thinking","thinking":"hm","signature":"sig","model":"claude-x"}) };
        let call = |id: &str, path: &str| Block::ToolUse { id: id.into(), name: "read".into(), input: json!({"path": path}) };
        let result = |id: &str| Block::ToolResult { id: id.into(), content: format!("out {id}"), is_error: false };
        let user = |content: Vec<Block>| ChatMessage { role: Role::User, content };
        let assistant = |content: Vec<Block>| ChatMessage { role: Role::Assistant, content };
        let r = Request {
            model: "gpt-6-luna".into(),
            messages: vec![
                user(vec![Block::Text("read both".into())]),
                // Anthropic: thinking, text and two parallel calls.
                assistant(vec![thinking.clone(), Block::Text("Reading.".into()), call("toolu_01A", "a.rs"), call("toolu_01B", "b.rs")]),
                user(vec![result("toolu_01A"), result("toolu_01B")]),
                // A message that was only Anthropic replay data plus text.
                assistant(vec![thinking, Block::Text("Both read.".into())]),
                user(vec![Block::Text("again".into())]),
                // Chat Completions: a call with no text, then the answer.
                assistant(vec![call("call_abc123", "a.rs")]),
                user(vec![result("call_abc123")]),
                assistant(vec![Block::Text("Same.".into())]),
                user(vec![Block::Text("next".into())]),
            ],
            tools: vec![ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object"}) }],
            max_tokens: 100,
            ..Default::default()
        };
        let b = build_body(&r);
        let fc = |id: &str, path: &str| json!({"type":"function_call","call_id":id,"name":"read","arguments":json!({"path": path}).to_string()});
        let out = |id: &str| json!({"type":"function_call_output","call_id":id,"output":format!("out {id}")});
        let said = |role: &str, kind: &str, t: &str| json!({"role":role,"content":[{"type":kind,"text":t}]});
        let expected = vec![
            said("user", "input_text", "read both"),
            said("assistant", "output_text", "Reading."),
            fc("toolu_01A", "a.rs"),
            fc("toolu_01B", "b.rs"),
            out("toolu_01A"),
            out("toolu_01B"),
            said("assistant", "output_text", "Both read."),
            said("user", "input_text", "again"),
            fc("call_abc123", "a.rs"),
            out("call_abc123"),
            said("assistant", "output_text", "Same."),
            said("user", "input_text", "next"),
        ];
        assert_eq!(b["input"], json!(expected));
        assert!(!b.to_string().contains("signature"), "no Anthropic replay data leaks into the request");
    }

    fn sse_body(chunks: &[&str]) -> String {
        chunks.iter().map(|c| format!("event: x\ndata: {c}\n\n")).collect()
    }

    #[test]
    fn http_round_trip() {
        let server = serve(vec![sse(&sse_body(STREAM)), error(429, r#"{"error":{"message":"slow","code":"rate_limit_exceeded"}}"#, vec![("Retry-After", "4".into())]), sse(&sse_body(&STREAM[..8]))]);
        let p = OpenAiResponses::new(Some("sk-test".into()), format!("{}/v1", server.url), BTreeMap::from([("x-team".to_string(), "moco".to_string())]));
        let mut events = vec![];
        assert_eq!(p.stream(&request(), &AtomicBool::new(false), &mut |e| events.push(e)), Ok(()));
        assert!(matches!(events.last(), Some(Event::Finish { reason: FinishReason::ToolUse, .. })));
        let (head, body) = server.requests.lock().unwrap()[0].clone();
        let h = head.to_lowercase();
        assert!(head.starts_with("POST /v1/responses") && h.contains("authorization: bearer sk-test") && h.contains("x-team: moco"), "{head}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["model"], "gpt-x");
        // HTTP errors share the chat-completions classification.
        assert!(matches!(p.stream(&request(), &AtomicBool::new(false), &mut |_| {}), Err(ProviderError::RateLimit { retry_after_ms: Some(4000), .. })));
        // A stream that stops before response.completed is a dropped connection, not a success.
        let mut partial = vec![];
        let r = p.stream(&request(), &AtomicBool::new(false), &mut |e| partial.push(e));
        assert!(matches!(r, Err(ProviderError::Transport { .. })), "{r:?}");
        assert!(partial.iter().any(|e| matches!(e, Event::TextDelta(_))));
    }
}
