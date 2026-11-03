//! Google Gemini API (`POST {base}/models/{model}:streamGenerateContent?alt=sse`). EXPERIMENTAL: written from the
//! public API documentation and tested against a local server replaying the documented stream shapes; not verified
//! against the live service.
//!
//! Gemini identifies a function result by the function's *name* and gives calls no ids, so moco makes up ids for calls
//! and maps them back to names when building a request. Thought signatures that come with function calls are kept as
//! `Block::Native` and sent back on the same call.
use super::*;
use serde_json::{json, Map};
use std::collections::{BTreeMap, HashMap};

pub const PROVIDER: &str = "gemini";

pub struct Gemini {
    pub api_key: Option<String>,
    /// Includes the version, e.g. `https://generativelanguage.googleapis.com/v1beta`.
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
    agent: ureq::Agent,
}

impl Gemini {
    pub fn new(api_key: Option<String>, base_url: String, headers: BTreeMap<String, String>) -> Self {
        if let Some(k) = &api_key {
            crate::log::add_secret(k);
        }
        Gemini {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            headers,
            agent: new_agent(),
        }
    }
}

/// A function-parameter schema Gemini accepts: a subset of OpenAPI. JSON-schema keywords outside it make the request
/// fail ("Unknown name"), so they are dropped; `type` arrays become `nullable`; unresolved `$ref`s become plain objects.
pub fn clean_schema(v: &Value) -> Value {
    let Some(o) = v.as_object() else { return json!({"type":"string"}) };
    let mut out = Map::new();
    let mut nullable = o.get("nullable").and_then(Value::as_bool).unwrap_or(false);
    match o.get("type") {
        Some(Value::String(t)) => {
            out.insert("type".into(), json!(t));
        }
        Some(Value::Array(ts)) => {
            nullable |= ts.iter().any(|t| t == "null");
            if let Some(t) = ts.iter().find(|t| *t != "null") {
                out.insert("type".into(), t.clone());
            }
        }
        _ => {}
    }
    if o.contains_key("$ref") && !out.contains_key("type") {
        out.insert("type".into(), json!("object"));
    }
    if nullable {
        out.insert("nullable".into(), json!(true));
    }
    for k in ["description", "format", "minItems", "maxItems", "minimum", "maximum"] {
        if let Some(x) = o.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    if let Some(e) = o.get("enum").and_then(Value::as_array) {
        // Gemini only allows enums on strings.
        out.insert("enum".into(), json!(e.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>()));
        out.insert("type".into(), json!("string"));
    }
    if let Some(items) = o.get("items") {
        out.insert("items".into(), clean_schema(items));
    }
    if let Some(p) = o.get("properties").and_then(Value::as_object) {
        if !p.is_empty() {
            out.insert("properties".into(), Value::Object(p.iter().map(|(k, v)| (k.clone(), clean_schema(v))).collect()));
        }
    }
    if let Some(r) = o.get("required").and_then(Value::as_array) {
        let props = out.get("properties").and_then(Value::as_object);
        let kept: Vec<&Value> = r.iter().filter(|n| n.as_str().is_some_and(|n| props.is_some_and(|p| p.contains_key(n)))).collect();
        if !kept.is_empty() {
            out.insert("required".into(), json!(kept));
        }
    }
    for k in ["anyOf", "oneOf"] {
        if let Some(a) = o.get(k).and_then(Value::as_array) {
            out.insert("anyOf".into(), json!(a.iter().map(clean_schema).collect::<Vec<_>>()));
            break;
        }
    }
    if !out.contains_key("type") && !out.contains_key("anyOf") {
        out.insert("type".into(), json!(if out.contains_key("properties") { "object" } else { "string" }));
    }
    Value::Object(out)
}

pub fn build_body(req: &Request) -> Value {
    // Function results are matched by name; calls only carry our own ids.
    let names: HashMap<&str, &str> = req.messages.iter().flat_map(|m| &m.content).filter_map(|b| if let Block::ToolUse { id, name, .. } = b { Some((id.as_str(), name.as_str())) } else { None }).collect();
    let mut contents: Vec<Value> = vec![];
    for m in &req.messages {
        match m.role {
            Role::User => {
                let (mut responses, mut rest): (Vec<Value>, Vec<Value>) = (vec![], vec![]);
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, is_error } => {
                            let name = names.get(id.as_str()).copied().unwrap_or("unknown");
                            responses.push(json!({"functionResponse":{"name":name,"response": if *is_error { json!({"error":content}) } else { json!({"output":content}) }}}));
                        }
                        Block::Text(t) => rest.push(json!({"text":t})),
                        Block::Image { media_type, data } => rest.push(json!({"inlineData":{"mimeType":media_type,"data":data}})),
                        Block::Document { media_type, data, .. } => rest.push(json!({"inlineData":{"mimeType":media_type,"data":data}})),
                        Block::ToolUse { .. } | Block::Native { .. } => {}
                    }
                }
                responses.extend(rest);
                if !responses.is_empty() {
                    contents.push(json!({"role":"user","parts":responses}));
                }
            }
            Role::Assistant => {
                let signatures: HashMap<&str, &str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Native { provider, data } if provider == PROVIDER => Some((data["call_id"].as_str()?, data["thoughtSignature"].as_str()?)),
                        _ => None,
                    })
                    .collect();
                let mut parts: Vec<Value> = vec![];
                for b in &m.content {
                    match b {
                        Block::Text(t) if !t.is_empty() => parts.push(json!({"text":t})),
                        Block::ToolUse { id, name, input } => {
                            let mut p = json!({"functionCall":{"name":name,"args":input}});
                            if let Some(sig) = signatures.get(id.as_str()) {
                                p["thoughtSignature"] = json!(sig);
                            }
                            parts.push(p);
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
        }
    }
    let mut body = json!({"contents":contents,"generationConfig":{"maxOutputTokens":req.max_tokens}});
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({"parts":[{"text":req.system}]});
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut d = json!({"name":t.name,"description":t.description});
                let params = clean_schema(&t.schema);
                // A tool with no parameters must not declare an empty object schema.
                if params.get("properties").is_some() {
                    d["parameters"] = params;
                }
                d
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations":decls}]);
        if req.tools_disabled {
            body["toolConfig"] = json!({"functionCallingConfig":{"mode":"NONE"}});
        }
    }
    super::effort::shape_gemini(&mut body, req);
    body
}

/// Seconds from a `google.rpc.RetryInfo` detail (`"retryDelay": "37s"`).
fn retry_delay_ms(err: &Value) -> Option<u64> {
    let d = err["details"].as_array()?.iter().find_map(|d| d["retryDelay"].as_str())?;
    d.trim_end_matches('s').parse::<f64>().ok().map(|s| (s * 1000.0) as u64)
}

fn classify_error(status: u16, err: &Value, header_retry: Option<u64>) -> ProviderError {
    let msg = err["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}"));
    let lower = msg.to_lowercase();
    let st = err["status"].as_str().unwrap_or("");
    let status = if status == 0 { err["code"].as_u64().unwrap_or(0) as u16 } else { status };
    let retry = retry_delay_ms(err).or(header_retry.map(|s| s * 1000));
    if lower.contains("exceeds the maximum number of tokens") || (lower.contains("token count") && lower.contains("exceeds")) || lower.contains("context length") {
        return ProviderError::ContextOverflow(msg);
    }
    if status == 401 || status == 403 || st == "UNAUTHENTICATED" || st == "PERMISSION_DENIED" || lower.contains("api key not valid") || lower.contains("api_key_invalid") {
        return ProviderError::Auth(msg);
    }
    if status == 429 || st == "RESOURCE_EXHAUSTED" {
        return ProviderError::RateLimit { retry_after_ms: retry, message: msg };
    }
    if matches!(status, 500..=599) || matches!(st, "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED") {
        return ProviderError::Internal { retry_after_ms: retry, message: msg };
    }
    ProviderError::InvalidRequest(msg)
}

pub fn classify_http(status: u16, body: &str, retry_after: Option<u64>) -> ProviderError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    // Error bodies are an object, or (for streaming) a one-element array holding it.
    let err = if v.is_array() { v[0]["error"].clone() } else { v["error"].clone() };
    let err = if err.is_object() { err } else { json!({"message": format!("HTTP {status}: {}", body.chars().take(300).collect::<String>())}) };
    classify_error(status, &err, retry_after)
}

/// Incremental decoder: every SSE `data:` line is a (partial) `GenerateContentResponse`.
#[derive(Default)]
pub struct Parser {
    calls: bool,
    finish: Option<String>,
    usage: Usage,
    done: bool,
}

impl Parser {
    pub fn feed(&mut self, data: &str) -> Result<Vec<Event>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Internal { retry_after_ms: None, message: format!("bad SSE JSON: {e}") })?;
        if v["error"].is_object() {
            return Err(classify_error(0, &v["error"], None));
        }
        if let Some(reason) = v["promptFeedback"]["blockReason"].as_str() {
            return Err(ProviderError::ContentFilter(format!("the prompt was blocked by the provider ({reason})")));
        }
        let u = &v["usageMetadata"];
        if u.is_object() {
            let get = |k: &str| u[k].as_u64().unwrap_or(0);
            let cached = get("cachedContentTokenCount");
            // Thinking tokens are billed as output; the figures are cumulative, so the latest chunk wins.
            self.usage = Usage { input: get("promptTokenCount").saturating_sub(cached), output: get("candidatesTokenCount") + get("thoughtsTokenCount"), cache_read: cached, cache_write: 0 };
        }
        let mut out = vec![];
        let cand = &v["candidates"][0];
        for p in cand["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(fc) = p.get("functionCall").filter(|f| f.is_object()) {
                let id = format!("call_{}", crate::id::new_id("g"));
                let name = fc["name"].as_str().unwrap_or("").to_string();
                if let Some(sig) = p["thoughtSignature"].as_str() {
                    out.push(Event::Native { provider: PROVIDER.into(), data: json!({"call_id": id, "thoughtSignature": sig}) });
                }
                out.push(Event::ToolInputStart { id: id.clone(), name: name.clone() });
                out.push(Event::ToolCall { id, name, input: if fc["args"].is_object() { fc["args"].clone() } else { json!({}) } });
                self.calls = true;
            } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                out.push(if p["thought"] == true { Event::ReasoningDelta(t.into()) } else { Event::TextDelta(t.into()) });
            }
        }
        if let Some(r) = cand["finishReason"].as_str() {
            self.finish = Some(r.to_string());
        }
        Ok(out)
    }

    /// Gemini has no end marker: when the body ends, this turns the last finish reason and usage into the final event.
    pub fn finish_event(&mut self) -> Result<Option<Event>, ProviderError> {
        if self.done {
            return Ok(None);
        }
        let Some(r) = self.finish.clone() else { return Ok(None) };
        self.done = true;
        let reason = match r.as_str() {
            "STOP" if self.calls => FinishReason::ToolUse,
            "STOP" => FinishReason::EndTurn,
            "MAX_TOKENS" => FinishReason::MaxTokens,
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
                return Err(ProviderError::ContentFilter(format!("the response was blocked by the provider ({r})")))
            }
            "MALFORMED_FUNCTION_CALL" => return Err(ProviderError::Internal { retry_after_ms: None, message: "the model produced a malformed function call".into() }),
            other if self.calls => {
                let _ = other;
                FinishReason::ToolUse
            }
            other => FinishReason::Other(other.to_string()),
        };
        Ok(Some(Event::Finish { reason, usage: self.usage }))
    }
}

impl Provider for Gemini {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let model = req.model.strip_prefix("models/").unwrap_or(&req.model);
        let url = format!("{}/models/{model}:streamGenerateContent?alt=sse", self.base_url);
        let mut call = self.agent.post(&url).set("content-type", "application/json");
        if let Some(k) = &self.api_key {
            call = call.set("x-goog-api-key", k);
        }
        for (k, v) in self.headers.iter().chain(req.overlay.headers.iter().map(|(k, v)| (k, v))) {
            call = call.set(k, v);
        }
        let body = build_body(req).to_string();
        let mut shown: Vec<(&str, &str)> = self.api_key.iter().map(|k| ("x-goog-api-key", k.as_str())).collect();
        shown.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain(req.overlay.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))));
        super::trace_http("gemini", &url, &shown, &body);
        let lines = super::open_lines("gemini", call, body, cancel, classify_http)?;
        let mut parser = Parser::default();
        read_sse(lines, cancel, |data| {
            for ev in parser.feed(data)? {
                on_event(ev);
            }
            Ok(false)
        })?;
        match parser.finish_event()? {
            Some(ev) => {
                on_event(ev);
                Ok(())
            }
            None if parser.done => Ok(()),
            None => Err(ProviderError::Transport { kind: TransportKind::Other, message: "stream ended before the model finished".into() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testserver::*;
    use super::*;

    const STREAM: &[&str] = &[
        r#"{"candidates":[{"content":{"parts":[{"text":"Thinking about it","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":50,"totalTokenCount":50},"modelVersion":"gemini-x"}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"Let me "}],"role":"model"},"index":0}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"look."},{"functionCall":{"name":"read","args":{"path":"a.rs"}},"thoughtSignature":"SIG1"},{"functionCall":{"name":"glob"}}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":12,"totalTokenCount":80,"thoughtsTokenCount":18,"cachedContentTokenCount":20}}"#,
    ];

    fn parse(chunks: &[&str]) -> Result<(Vec<Event>, Option<Event>), ProviderError> {
        let mut p = Parser::default();
        let mut all = vec![];
        for c in chunks {
            all.extend(p.feed(c)?);
        }
        let fin = p.finish_event()?;
        Ok((all, fin))
    }

    #[test]
    fn a_turn_with_thoughts_text_and_two_calls() {
        let (ev, fin) = parse(STREAM).unwrap();
        assert_eq!(ev[0], Event::ReasoningDelta("Thinking about it".into()));
        assert_eq!((ev[1].clone(), ev[2].clone()), (Event::TextDelta("Let me ".into()), Event::TextDelta("look.".into())));
        let Event::Native { provider, data } = &ev[3] else { panic!("{:?}", ev[3]) };
        assert_eq!((provider.as_str(), data["thoughtSignature"].as_str()), (PROVIDER, Some("SIG1")));
        let Event::ToolCall { id, name, input } = &ev[5] else { panic!("{:?}", ev[5]) };
        assert_eq!((name.as_str(), input.clone(), data["call_id"].as_str()), ("read", json!({"path":"a.rs"}), Some(id.as_str())), "the signature is tied to its call");
        let Event::ToolCall { input: no_args, id: id2, .. } = &ev[7] else { panic!("{:?}", ev[7]) };
        assert_eq!((no_args.clone(), id2 != id), (json!({}), true), "a call without args gets an empty object; ids are unique");
        // Usage: cached is split out; thinking tokens count as output; stop after calls means tool use.
        assert_eq!(fin, Some(Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 30, output: 30, cache_read: 20, cache_write: 0 } }));
    }

    #[test]
    fn finish_reasons_and_blocks() {
        let end = |reason: &str| parse(&[&format!(r#"{{"candidates":[{{"content":{{"parts":[{{"text":"x"}}]}},"finishReason":"{reason}"}}]}}"#)]);
        assert!(matches!(end("STOP").unwrap().1, Some(Event::Finish { reason: FinishReason::EndTurn, .. })));
        assert!(matches!(end("MAX_TOKENS").unwrap().1, Some(Event::Finish { reason: FinishReason::MaxTokens, .. })));
        assert!(matches!(end("OTHER").unwrap().1, Some(Event::Finish { reason: FinishReason::Other(_), .. })));
        assert!(matches!(end("SAFETY"), Err(ProviderError::ContentFilter(_))));
        assert!(matches!(end("MALFORMED_FUNCTION_CALL"), Err(ProviderError::Internal { .. })), "retryable");
        assert!(matches!(parse(&[r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#]), Err(ProviderError::ContentFilter(_))));
        assert_eq!(parse(&[r#"{"candidates":[{"content":{"parts":[{"text":"cut off"}]}}]}"#]).unwrap().1, None, "no finish reason yet: not complete");
        assert!(matches!(parse(&[r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#]), Err(ProviderError::RateLimit { .. })));
    }

    #[test]
    fn http_error_classification() {
        let body = |code: u16, status: &str, msg: &str, extra: &str| format!(r#"{{"error":{{"code":{code},"message":"{msg}","status":"{status}"{extra}}}}}"#);
        assert!(matches!(classify_http(400, &body(400, "INVALID_ARGUMENT", "API key not valid. Please pass a valid API key.", ""), None), ProviderError::Auth(_)));
        assert!(matches!(classify_http(403, &body(403, "PERMISSION_DENIED", "denied", ""), None), ProviderError::Auth(_)));
        let rl = classify_http(429, &body(429, "RESOURCE_EXHAUSTED", "quota", r#","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"37s"}]"#), None);
        assert_eq!(rl, ProviderError::RateLimit { retry_after_ms: Some(37_000), message: "quota".into() });
        assert!(matches!(classify_http(429, &body(429, "RESOURCE_EXHAUSTED", "quota", ""), Some(5)), ProviderError::RateLimit { retry_after_ms: Some(5000), .. }));
        assert!(matches!(classify_http(503, &body(503, "UNAVAILABLE", "overloaded", ""), None), ProviderError::Internal { .. }));
        assert!(matches!(classify_http(400, &body(400, "INVALID_ARGUMENT", "The input token count (2000000) exceeds the maximum number of tokens allowed (1048576).", ""), None), ProviderError::ContextOverflow(_)));
        assert!(matches!(classify_http(400, &body(400, "INVALID_ARGUMENT", "Invalid JSON payload received.", ""), None), ProviderError::InvalidRequest(_)));
        assert!(matches!(classify_http(404, &body(404, "NOT_FOUND", "models/x is not found", ""), None), ProviderError::InvalidRequest(_)));
        assert!(matches!(classify_http(500, "not json", None), ProviderError::Internal { .. }));
        assert!(matches!(classify_http(429, &format!("[{}]", body(429, "RESOURCE_EXHAUSTED", "quota", "")), None), ProviderError::RateLimit { .. }), "array-wrapped error body");
    }

    #[test]
    fn schemas_are_cleaned_to_what_gemini_accepts() {
        let dirty = json!({
            "$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "additionalProperties": false, "title": "T",
            "properties": {
                "path": {"type": "string", "description": "a path", "default": "x", "minLength": 1},
                "n": {"type": ["integer", "null"], "minimum": 1},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "level": {"enum": [1, 2]},
                "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true, "minItems": 1},
                "opts": {"type": "object", "properties": {"deep": {"$ref": "#/definitions/D"}}, "required": ["deep", "ghost"]},
                "either": {"oneOf": [{"type": "string"}, {"type": "number"}]},
                "bare": {}
            },
            "required": ["path", "missing"]
        });
        let c = clean_schema(&dirty);
        assert!(c.get("$schema").is_none() && c.get("additionalProperties").is_none() && c.get("title").is_none());
        assert_eq!(c["required"], json!(["path"]), "required names that do not exist are dropped");
        let p = &c["properties"];
        assert_eq!(p["path"], json!({"type":"string","description":"a path"}));
        assert_eq!(p["n"], json!({"type":"integer","nullable":true,"minimum":1}));
        assert_eq!(p["level"], json!({"type":"string","enum":["1","2"]}), "enums are strings");
        assert_eq!(p["tags"], json!({"type":"array","items":{"type":"string"},"minItems":1}));
        assert_eq!(p["opts"]["properties"]["deep"], json!({"type":"object"}), "an unresolved $ref becomes a plain object");
        assert_eq!(p["opts"]["required"], json!(["deep"]));
        assert_eq!(p["either"], json!({"anyOf":[{"type":"string"},{"type":"number"}]}));
        assert_eq!(p["bare"], json!({"type":"string"}));
    }

    fn request() -> Request {
        Request {
            model: "gemini-x".into(),
            system: "sys".into(),
            messages: vec![
                ChatMessage { role: Role::User, content: vec![Block::Text("read a.rs".into()), Block::Image { media_type: "image/png".into(), data: "QQ==".into() }] },
                ChatMessage {
                    role: Role::Assistant,
                    content: vec![
                        Block::Text("Let me look.".into()),
                        Block::Native { provider: PROVIDER.into(), data: json!({"call_id":"c1","thoughtSignature":"SIG1"}) },
                        Block::ToolUse { id: "c1".into(), name: "read".into(), input: json!({"path":"a.rs"}) },
                        Block::ToolUse { id: "c2".into(), name: "glob".into(), input: json!({}) },
                        Block::Native { provider: "openai-responses".into(), data: json!({"type":"reasoning"}) },
                    ],
                },
                ChatMessage {
                    role: Role::User,
                    content: vec![
                        Block::Text("thanks".into()),
                        Block::ToolResult { id: "c1".into(), content: "1: fn a() {}".into(), is_error: false },
                        Block::ToolResult { id: "c2".into(), content: "denied".into(), is_error: true },
                    ],
                },
            ],
            tools: vec![
                ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}) },
                ToolDef { name: "glob".into(), description: "no params".into(), schema: json!({"type":"object","properties":{}}) },
            ],
            max_tokens: 100,
            tools_disabled: false,
            ..Default::default()
        }
    }

    #[test]
    fn request_body_shape() {
        let b = build_body(&request());
        assert_eq!(b["systemInstruction"], json!({"parts":[{"text":"sys"}]}));
        assert_eq!(b["generationConfig"], json!({"maxOutputTokens":100}));
        let c = b["contents"].as_array().unwrap();
        assert_eq!(c[0], json!({"role":"user","parts":[{"text":"read a.rs"},{"inlineData":{"mimeType":"image/png","data":"QQ=="}}]}));
        assert_eq!(c[1]["role"], "model");
        assert_eq!(c[1]["parts"][0], json!({"text":"Let me look."}));
        assert_eq!(c[1]["parts"][1], json!({"functionCall":{"name":"read","args":{"path":"a.rs"}},"thoughtSignature":"SIG1"}), "the signature goes back on its call");
        assert_eq!(c[1]["parts"][2], json!({"functionCall":{"name":"glob","args":{}}}));
        assert_eq!(c[1]["parts"].as_array().unwrap().len(), 3, "another provider's replay data is not sent");
        // Results are matched by function name, results first, errors marked.
        assert_eq!(c[2]["parts"][0], json!({"functionResponse":{"name":"read","response":{"output":"1: fn a() {}"}}}));
        assert_eq!(c[2]["parts"][1], json!({"functionResponse":{"name":"glob","response":{"error":"denied"}}}));
        assert_eq!(c[2]["parts"][2], json!({"text":"thanks"}));
        let decls = &b["tools"][0]["functionDeclarations"];
        assert_eq!(decls[0]["parameters"], json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}));
        assert!(decls[1].get("parameters").is_none(), "no empty object schema for a tool without parameters");
        assert!(b.get("toolConfig").is_none());
        let mut r = request();
        r.tools_disabled = true;
        assert_eq!(build_body(&r)["toolConfig"], json!({"functionCallingConfig":{"mode":"NONE"}}));
    }

    #[test]
    fn http_round_trip() {
        let body: String = STREAM.iter().map(|c| format!("data: {c}\n\n")).collect();
        let server = serve(vec![sse(&body), error(400, r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#, vec![]), sse(&format!("data: {}\n\n", STREAM[1]))]);
        let p = Gemini::new(Some("AIza-test".into()), format!("{}/v1beta", server.url), BTreeMap::new());
        let mut events = vec![];
        assert_eq!(p.stream(&request(), &AtomicBool::new(false), &mut |e| events.push(e)), Ok(()));
        assert!(matches!(events.last(), Some(Event::Finish { reason: FinishReason::ToolUse, .. })));
        let (head, req_body) = server.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /v1beta/models/gemini-x:streamGenerateContent?alt=sse"), "{head}");
        assert!(head.to_lowercase().contains("x-goog-api-key: aiza-test"), "{head}");
        assert!(!head.contains("AIza-test") || head.to_lowercase().contains("x-goog-api-key"), "the key travels in a header, never in the URL");
        assert!(!head.lines().next().unwrap().contains("key="), "{head}");
        assert_eq!(serde_json::from_str::<Value>(&req_body).unwrap()["contents"][0]["role"], "user");
        assert!(matches!(p.stream(&request(), &AtomicBool::new(false), &mut |_| {}), Err(ProviderError::Auth(_))));
        let r = p.stream(&request(), &AtomicBool::new(false), &mut |_| {});
        assert!(matches!(r, Err(ProviderError::Transport { .. })), "a stream without a finish reason is a dropped connection: {r:?}");
        // The model id may come with the `models/` prefix.
        let server = serve(vec![sse(&body)]);
        let p = Gemini::new(None, server.url.clone(), BTreeMap::new());
        let mut r = request();
        r.model = "models/gemini-y".into();
        p.stream(&r, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let (head, _) = server.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /models/gemini-y:streamGenerateContent"), "{head}");
        assert!(!head.to_lowercase().contains("x-goog-api-key"));
    }
}
