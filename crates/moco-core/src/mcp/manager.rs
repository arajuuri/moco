//! Runs the configured MCP servers and exposes what they offer: tools (as `Tool`s named `<server>_<tool>`), prompts
//! (as slash commands) and resources (an `mcp-resource` tool and `@` mentions).
use super::client::{Client, McpConn, NotifyFn, RpcError};
use super::http::{ConnectError, HttpClient, HttpOptions};
use super::oauth;
use crate::config::{McpServerConfig, McpTimeouts, OAuthSetting};
use crate::tools::{MediaCaps, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    pub config: McpServerConfig,
    /// Working directory of the server (the project, unless the entry sets `cwd`).
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pending,
    Connected,
    Disabled,
    Failed(String),
    NeedsAuth(String),
}

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub status: Status,
    pub tools: usize,
    pub prompts: usize,
    pub resources: usize,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// A server prompt, offered as the slash command `/<command>` (`server:prompt`).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptInfo {
    pub command: String,
    pub server: String,
    pub prompt: String,
    pub description: String,
    pub arguments: Vec<PromptArg>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceInfo {
    pub server: String,
    pub name: String,
    pub uri: String,
    pub description: String,
}

#[derive(Clone, Default)]
struct RemoteTool {
    name: String,
    description: String,
    schema: Value,
}

#[derive(Clone, Default)]
struct Catalog {
    tools: Vec<RemoteTool>,
    prompts: Vec<PromptInfo>,
    resources: Vec<ResourceInfo>,
}

struct Server {
    spec: ServerSpec,
    status: Status,
    client: Option<Arc<dyn McpConn>>,
    catalog: Catalog,
}

#[derive(Clone, Copy)]
struct Timeouts {
    startup: Duration,
    catalog: Duration,
    execution: Duration,
}

pub struct Manager {
    servers: Mutex<Vec<Server>>,
    timeouts: Timeouts,
    /// Where OAuth tokens for remote servers are kept.
    app_dir: PathBuf,
    me: Mutex<Weak<Manager>>,
}

/// Letters, digits, `_` and `-` only (what model APIs accept in tool names); everything else becomes `_`.
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// `<server>_<tool>`, at most 64 characters: long names are cut and tagged with a short hash of the full name.
pub fn tool_name(server: &str, tool: &str) -> String {
    let full = format!("{}_{}", sanitize(server), sanitize(tool));
    if full.len() <= 64 {
        return full;
    }
    let h: String = Sha256::digest(full.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{}_{h}", &full[..55])
}

impl Manager {
    /// Start every server in the background and return at once; use `wait_ready` to wait for them.
    pub fn start(specs: Vec<ServerSpec>, timeouts: &McpTimeouts, app_dir: &std::path::Path) -> Arc<Manager> {
        let t = Timeouts {
            startup: Duration::from_millis(timeouts.startup.unwrap_or(30_000)),
            catalog: Duration::from_millis(timeouts.catalog.unwrap_or(30_000)),
            execution: Duration::from_millis(timeouts.execution.unwrap_or(300_000)),
        };
        let servers = specs
            .into_iter()
            .map(|spec| {
                let status = if spec.config.disabled { Status::Disabled } else { Status::Pending };
                Server { spec, status, client: None, catalog: Catalog::default() }
            })
            .collect();
        let m = Arc::new(Manager { servers: Mutex::new(servers), timeouts: t, app_dir: app_dir.to_path_buf(), me: Mutex::new(Weak::new()) });
        *m.me.lock().unwrap() = Arc::downgrade(&m);
        let names: Vec<String> = m.servers.lock().unwrap().iter().filter(|s| s.status == Status::Pending).map(|s| s.spec.name.clone()).collect();
        for name in names {
            m.connect_in_background(name);
        }
        m
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Server>> {
        self.servers.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, name: &str, status: Status) {
        if let Some(s) = self.lock().iter_mut().find(|s| s.spec.name == name) {
            s.status = status;
        }
    }

    fn connect_in_background(self: &Arc<Self>, name: String) {
        let me = self.clone();
        std::thread::spawn(move || me.connect(&name));
    }

    fn connect(self: &Arc<Self>, name: &str) {
        let Some(spec) = self.lock().iter().find(|s| s.spec.name == name).map(|s| s.spec.clone()) else { return };
        let weak = Arc::downgrade(self);
        let server = name.to_string();
        let notify: NotifyFn = Arc::new(move |method, _| {
            // The server's lists changed: re-read them (once, off the reader thread).
            if matches!(method, "notifications/tools/list_changed" | "notifications/prompts/list_changed" | "notifications/resources/list_changed") {
                if let Some(m) = weak.upgrade() {
                    let server = server.clone();
                    std::thread::spawn(move || m.refresh(&server));
                }
            }
        });
        let started = Instant::now();
        let client: Arc<dyn McpConn> = if spec.config.is_remote() {
            let Some(url) = spec.config.url.clone() else { return self.set_status(name, Status::Failed("a remote server needs a `url`".into())) };
            let opts = HttpOptions { url, headers: spec.config.headers.clone(), oauth: OAuthSetting::resolve(&spec.config.oauth).is_some(), app_dir: self.app_dir.clone(), notify };
            match HttpClient::connect(opts, self.timeouts.startup) {
                Ok(c) => c,
                Err(ConnectError::NeedsAuth(m)) => return self.set_status(name, Status::NeedsAuth(m.replace("<name>", name))),
                Err(ConnectError::Failed(m)) => return self.set_status(name, Status::Failed(m)),
            }
        } else {
            let cwd = match &spec.config.cwd {
                Some(c) => {
                    let p = PathBuf::from(c);
                    if p.is_absolute() { p } else { spec.cwd.join(p) }
                }
                None => spec.cwd.clone(),
            };
            let client = match Client::spawn(&spec.config.command, &cwd, &spec.config.environment, notify) {
                Ok(c) => c,
                Err(e) => return self.set_status(name, Status::Failed(e)),
            };
            let left = self.timeouts.startup.saturating_sub(started.elapsed());
            if let Err(e) = client.initialize(left.max(Duration::from_millis(200))) {
                self.set_status(name, Status::Failed(e));
                return; // dropping the client stops the process
            }
            client
        };
        if let Some(s) = self.lock().iter_mut().find(|s| s.spec.name == name) {
            s.client = Some(client);
        }
        match self.read_catalog(name) {
            Ok(c) => {
                if let Some(s) = self.lock().iter_mut().find(|s| s.spec.name == name) {
                    s.catalog = c;
                    s.status = Status::Connected;
                }
            }
            Err(e) => self.set_status(name, Status::Failed(format!("connected, but listing failed: {e}"))),
        }
    }

    /// Sign in to a remote server with OAuth, then reconnect it. Blocks while the user completes the browser step.
    pub fn authorize(self: &Arc<Self>, name: &str, io: &oauth::LoginIo) -> Result<(), String> {
        let spec = self.lock().iter().find(|s| s.spec.name == name).map(|s| s.spec.clone()).ok_or_else(|| format!("no MCP server '{name}'"))?;
        if !spec.config.is_remote() {
            return Err(format!("'{name}' is a local server; only remote servers sign in"));
        }
        let cfg = OAuthSetting::resolve(&spec.config.oauth).ok_or_else(|| format!("OAuth is switched off for '{name}' (`oauth: false`); use `headers` for a static token"))?;
        let url = spec.config.url.clone().ok_or("the server has no url")?;
        oauth::login(&self.app_dir, &url, &spec.config.headers, &cfg, Duration::from_secs(300), io)?;
        self.reconnect(name);
        self.wait_ready(self.timeouts.startup + Duration::from_secs(2));
        Ok(())
    }

    /// Forget the stored tokens of a remote server and disconnect it.
    pub fn logout(self: &Arc<Self>, name: &str) -> Result<bool, String> {
        let url = self.lock().iter().find(|s| s.spec.name == name).and_then(|s| s.spec.config.url.clone()).ok_or_else(|| format!("no remote MCP server '{name}'"))?;
        let removed = oauth::remove(&self.app_dir, &url);
        self.reconnect(name);
        Ok(removed)
    }

    /// Re-read one server's tools, prompts and resources (after it announced a change).
    pub fn refresh(&self, name: &str) {
        if let Ok(c) = self.read_catalog(name) {
            if let Some(s) = self.lock().iter_mut().find(|s| s.spec.name == name) {
                s.catalog = c;
            }
        }
    }

    fn client_of(&self, name: &str) -> Option<Arc<dyn McpConn>> {
        self.lock().iter().find(|s| s.spec.name == name).and_then(|s| s.client.clone())
    }

    /// `tools/list`, `prompts/list` and `resources/list` for what the server declared support for, following pagination.
    fn read_catalog(&self, name: &str) -> Result<Catalog, String> {
        let client = self.client_of(name).ok_or("not connected")?;
        let caps = client.info()["capabilities"].clone();
        let list = |method: &str, key: &str| -> Result<Vec<Value>, String> {
            let (mut out, mut cursor) = (vec![], None::<String>);
            for _ in 0..50 {
                let params = cursor.as_ref().map(|c| json!({"cursor": c})).unwrap_or_else(|| json!({}));
                let r = client.request(method, params, self.timeouts.catalog, None).map_err(|e| format!("{method}: {e}"))?;
                out.extend(r[key].as_array().cloned().unwrap_or_default());
                match r["nextCursor"].as_str() {
                    Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                    _ => break,
                }
            }
            Ok(out)
        };
        let mut cat = Catalog::default();
        if caps.get("tools").is_some() {
            for t in list("tools/list", "tools")? {
                let Some(n) = t["name"].as_str() else { continue };
                let mut schema = t["inputSchema"].clone();
                if !schema.is_object() {
                    schema = json!({"type": "object", "properties": {}});
                }
                cat.tools.push(RemoteTool { name: n.to_string(), description: t["description"].as_str().unwrap_or("").to_string(), schema });
            }
        }
        if caps.get("prompts").is_some() {
            for p in list("prompts/list", "prompts")? {
                let Some(n) = p["name"].as_str() else { continue };
                cat.prompts.push(PromptInfo {
                    command: format!("{}:{}", sanitize(name), sanitize(n)),
                    server: name.to_string(),
                    prompt: n.to_string(),
                    description: p["description"].as_str().unwrap_or("").to_string(),
                    arguments: p["arguments"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| Some(PromptArg { name: a["name"].as_str()?.to_string(), description: a["description"].as_str().unwrap_or("").to_string(), required: a["required"].as_bool().unwrap_or(false) }))
                        .collect(),
                });
            }
        }
        if caps.get("resources").is_some() {
            for r in list("resources/list", "resources")? {
                let Some(uri) = r["uri"].as_str() else { continue };
                cat.resources.push(ResourceInfo {
                    server: name.to_string(),
                    name: r["name"].as_str().unwrap_or(uri).to_string(),
                    uri: uri.to_string(),
                    description: r["description"].as_str().unwrap_or("").to_string(),
                });
            }
        }
        Ok(cat)
    }

    /// Wait until no server is still starting, or `timeout` passes. Returns whether everything settled.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if !self.lock().iter().any(|s| s.status == Status::Pending) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn status(&self) -> Vec<ServerInfo> {
        self.lock()
            .iter()
            .map(|s| ServerInfo {
                name: s.spec.name.clone(),
                // A server whose process has gone away is not "connected" any more, whatever it was.
                status: match (&s.status, &s.client) {
                    (Status::Connected, Some(c)) if !c.is_alive() => Status::Failed(format!("the server exited{}", tail_note(&c.stderr_tail()))),
                    _ => s.status.clone(),
                },
                tools: s.catalog.tools.len(),
                prompts: s.catalog.prompts.len(),
                resources: s.catalog.resources.len(),
                command: if s.spec.config.is_remote() { s.spec.config.url.clone().unwrap_or_default() } else { s.spec.config.command.join(" ") },
            })
            .collect()
    }

    /// Output of a server's stderr, for explaining a failure.
    pub fn stderr_tail(&self, name: &str) -> String {
        self.client_of(name).map(|c| c.stderr_tail()).unwrap_or_default()
    }

    /// Restart one server.
    pub fn reconnect(self: &Arc<Self>, name: &str) -> bool {
        let found = {
            let mut servers = self.lock();
            match servers.iter_mut().find(|s| s.spec.name == name && !s.spec.config.disabled) {
                Some(s) => {
                    s.client = None; // drops (and stops) the old process
                    s.catalog = Catalog::default();
                    s.status = Status::Pending;
                    true
                }
                None => false,
            }
        };
        if found {
            self.connect_in_background(name.to_string());
        }
        found
    }

    pub fn shutdown(&self) {
        for s in self.lock().iter_mut() {
            s.client = None;
            if s.status == Status::Connected || s.status == Status::Pending {
                s.status = Status::Failed("stopped".into());
            }
        }
    }

    // ---- what the servers offer ----------------------------------------------------------------

    /// Every tool of every connected server, plus `mcp-resource` when any server has resources.
    /// Names are unique: `<server>_<tool>`, with `_2`, `_3` ... appended on a clash.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = vec![];
        let mut used: Vec<String> = vec![];
        let mut resources = vec![];
        for s in self.lock().iter().filter(|s| s.status == Status::Connected) {
            let Some(client) = s.client.clone().filter(|c| c.is_alive()) else { continue };
            for t in &s.catalog.tools {
                let base = tool_name(&s.spec.name, &t.name);
                let mut name = base.clone();
                let mut n = 2;
                while used.contains(&name) {
                    name = format!("{base}_{n}");
                    n += 1;
                }
                used.push(name.clone());
                out.push(Arc::new(McpTool {
                    full_name: name,
                    server: s.spec.name.clone(),
                    remote_name: t.name.clone(),
                    description: if t.description.is_empty() { format!("Tool '{}' of the MCP server '{}'.", t.name, s.spec.name) } else { t.description.clone() },
                    schema: t.schema.clone(),
                    client: client.clone(),
                    timeout: s.spec.config.timeout.map(Duration::from_millis).unwrap_or(self.timeouts.execution),
                }));
            }
            resources.extend(s.catalog.resources.iter().cloned());
        }
        if !resources.is_empty() {
            if let Some(me) = self.me.lock().unwrap().upgrade() {
                out.push(Arc::new(ResourceTool { manager: me, resources }));
            }
        }
        out
    }

    pub fn prompts(&self) -> Vec<PromptInfo> {
        self.lock().iter().filter(|s| s.status == Status::Connected).flat_map(|s| s.catalog.prompts.clone()).collect()
    }

    pub fn resources(&self) -> Vec<ResourceInfo> {
        self.lock().iter().filter(|s| s.status == Status::Connected).flat_map(|s| s.catalog.resources.clone()).collect()
    }

    /// Run the prompt behind a slash command and return its text. `args` is what the user typed after the command:
    /// words fill the prompt's arguments in order, `name=value` sets one by name (quotes group words).
    pub fn get_prompt(&self, command: &str, args: &str) -> Result<String, String> {
        let p = self.prompts().into_iter().find(|p| p.command == command).ok_or_else(|| format!("no MCP prompt '{command}'"))?;
        let arguments = parse_prompt_args(&p.arguments, args).map_err(|e| format!("{e}\nusage: /{}", usage(&p)))?;
        let client = self.client_of(&p.server).ok_or("the server is not connected")?;
        let r = client
            .request("prompts/get", json!({"name": p.prompt, "arguments": arguments}), self.timeouts.execution, None)
            .map_err(|e| format!("prompts/get failed: {e}"))?;
        let mut parts = vec![];
        for m in r["messages"].as_array().into_iter().flatten() {
            let c = &m["content"];
            let text = match c["type"].as_str() {
                Some("text") => c["text"].as_str().unwrap_or("").to_string(),
                Some("resource") => c["resource"]["text"].as_str().map(|t| format!("<resource uri=\"{}\">\n{t}\n</resource>", c["resource"]["uri"].as_str().unwrap_or(""))).unwrap_or_default(),
                _ => String::new(),
            };
            if text.is_empty() {
                continue;
            }
            parts.push(if m["role"] == "assistant" { format!("(assistant) {text}") } else { text });
        }
        if parts.is_empty() {
            return Err("the prompt returned no text".into());
        }
        Ok(parts.join("\n\n"))
    }

    /// Text of a resource (binary resources are reported, not returned).
    pub fn read_resource(&self, server: &str, uri: &str) -> Result<String, String> {
        let client = self.client_of(server).ok_or_else(|| format!("server '{server}' is not connected"))?;
        let r = client.request("resources/read", json!({"uri": uri}), self.timeouts.execution, None).map_err(|e| format!("resources/read failed: {e}"))?;
        let mut out = vec![];
        for c in r["contents"].as_array().into_iter().flatten() {
            if let Some(t) = c["text"].as_str() {
                out.push(t.to_string());
            } else if let Some(b) = c["blob"].as_str() {
                out.push(format!("[binary resource {} ({} bytes encoded) omitted]", c["mimeType"].as_str().unwrap_or("unknown type"), b.len()));
            }
        }
        if out.is_empty() {
            return Err("the resource has no content".into());
        }
        Ok(out.join("\n"))
    }

    /// The resource a mention token names: `server:name`, a unique `name`, or the exact URI.
    pub fn find_resource(&self, token: &str) -> Option<ResourceInfo> {
        let all = self.resources();
        let matches: Vec<&ResourceInfo> = all.iter().filter(|r| r.uri == token || format!("{}:{}", r.server, r.name) == token || r.name == token).collect();
        match matches.as_slice() {
            [one] => Some((*one).clone()),
            _ => None, // none, or ambiguous
        }
    }
}

fn tail_note(t: &str) -> String {
    if t.is_empty() { String::new() } else { format!(" ({t})") }
}

fn fmt_dur(d: Duration) -> String {
    if d < Duration::from_secs(1) { format!("{} ms", d.as_millis()) } else { format!("{} s", d.as_secs()) }
}

fn usage(p: &PromptInfo) -> String {
    let args: Vec<String> = p.arguments.iter().map(|a| if a.required { format!("<{}>", a.name) } else { format!("[{}]", a.name) }).collect();
    format!("{} {}", p.command, args.join(" ")).trim_end().to_string()
}

/// Shell-like words: whitespace separates, single or double quotes group.
fn words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (vec![], String::new(), None::<char>, false);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn parse_prompt_args(spec: &[PromptArg], input: &str) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    let mut positional = vec![];
    for w in words(input) {
        match w.split_once('=') {
            Some((k, v)) if spec.iter().any(|a| a.name == k) => {
                map.insert(k.to_string(), json!(v));
            }
            _ => positional.push(w),
        }
    }
    let free: Vec<&PromptArg> = spec.iter().filter(|a| !map.contains_key(&a.name)).collect();
    let mut it = positional.into_iter();
    for (i, a) in free.iter().enumerate() {
        // The last free argument takes whatever words are left.
        let v = if i + 1 == free.len() { it.by_ref().collect::<Vec<_>>().join(" ") } else { it.next().unwrap_or_default() };
        if !v.is_empty() {
            map.insert(a.name.clone(), json!(v));
        }
    }
    let missing: Vec<&str> = spec.iter().filter(|a| a.required && !map.contains_key(&a.name)).map(|a| a.name.as_str()).collect();
    if !missing.is_empty() {
        return Err(format!("missing required argument{}: {}", if missing.len() > 1 { "s" } else { "" }, missing.join(", ")));
    }
    Ok(map)
}

// ---- tool adapters -------------------------------------------------------------------------------

struct McpTool {
    full_name: String,
    server: String,
    remote_name: String,
    description: String,
    schema: Value,
    client: Arc<dyn McpConn>,
    timeout: Duration,
}

/// Turn a `tools/call` result into tool output: text joined, images attached, errors raised.
pub fn map_call_result(res: &Value, caps: MediaCaps) -> Result<ToolOutput, ToolError> {
    use base64::Engine as _;
    let mut text: Vec<String> = vec![];
    let mut media: Vec<(String, Vec<u8>)> = vec![];
    for c in res["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => text.push(c["text"].as_str().unwrap_or("").to_string()),
            Some("image") => {
                let mt = c["mimeType"].as_str().unwrap_or("image/png").to_string();
                match base64::engine::general_purpose::STANDARD.decode(c["data"].as_str().unwrap_or("")) {
                    Ok(bytes) if caps.image && crate::media::sniff(&bytes).is_some_and(|s| crate::media::check(&s, bytes.len()).is_ok()) => media.push((mt, bytes)),
                    Ok(_) if !caps.image => text.push("[image omitted: the current model does not accept images]".into()),
                    _ => text.push("[an image in the result could not be used]".into()),
                }
            }
            Some("resource") => {
                let r = &c["resource"];
                match r["text"].as_str() {
                    Some(t) => text.push(format!("[resource {}]\n{t}", r["uri"].as_str().unwrap_or(""))),
                    None => text.push(format!("[binary resource {} omitted]", r["uri"].as_str().unwrap_or(""))),
                }
            }
            Some("resource_link") => text.push(format!("[link: {} ({})]", c["uri"].as_str().unwrap_or(""), c["name"].as_str().unwrap_or(""))),
            Some("audio") => text.push("[audio omitted]".into()),
            _ => {}
        }
    }
    if text.is_empty() && media.is_empty() && !res["structuredContent"].is_null() {
        text.push(serde_json::to_string_pretty(&res["structuredContent"]).unwrap_or_default());
    }
    let body = text.join("\n");
    if res["isError"].as_bool().unwrap_or(false) {
        return Err(ToolError::fail(if body.is_empty() { "the tool reported an error".to_string() } else { body }));
    }
    let mut out = ToolOutput::text(if body.is_empty() && media.is_empty() { "(no output)".to_string() } else { body });
    for (i, (mt, bytes)) in media.iter().enumerate() {
        out.add_media(mt, &format!("result-{}.{}", i + 1, mt.rsplit('/').next().unwrap_or("bin")), bytes);
    }
    Ok(out)
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        ctx.permit(&self.full_name, vec!["*".into()], vec!["*".into()])?;
        let args = if input.is_object() { input.clone() } else { json!({}) };
        match self.client.request("tools/call", json!({"name": self.remote_name, "arguments": args}), self.timeout, Some(ctx.cancel)) {
            Ok(res) => map_call_result(&res, ctx.media),
            Err(RpcError::Cancelled) => Err(ToolError::Interrupted),
            Err(RpcError::Timeout) => Err(ToolError::fail(format!("MCP tool {} did not answer within {}", self.full_name, fmt_dur(self.timeout)))),
            Err(RpcError::Unauthorized(_)) => Err(ToolError::fail(format!("The MCP server '{}' needs you to sign in again. The user can do that with /mcp auth {}.", self.server, self.server))),
            Err(RpcError::Closed(m)) => Err(ToolError::fail(format!("The MCP server '{}' stopped{}. The user can restart it with /mcp reconnect {}.", self.server, if m.is_empty() { String::new() } else { format!(" ({m})") }, self.server))),
            Err(RpcError::Server { message, .. }) => Err(ToolError::fail(message)),
        }
    }
}

struct ResourceTool {
    manager: Arc<Manager>,
    resources: Vec<ResourceInfo>,
}

impl Tool for ResourceTool {
    fn name(&self) -> &str {
        "mcp-resource"
    }
    fn description(&self) -> String {
        let mut list: Vec<String> = self.resources.iter().take(50).map(|r| format!("- {}: {} ({}){}", r.server, r.uri, r.name, if r.description.is_empty() { String::new() } else { format!(" - {}", r.description) })).collect();
        if self.resources.len() > 50 {
            list.push(format!("... and {} more", self.resources.len() - 50));
        }
        format!("Read a resource offered by an MCP server, by URI.\n\nAvailable resources:\n{}", list.join("\n"))
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"server":{"type":"string","description":"Server name (optional when the URI is unique)"},"uri":{"type":"string"}},"required":["uri"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let uri = input["uri"].as_str().ok_or_else(|| ToolError::fail("`uri` is required"))?;
        let server = match input["server"].as_str().filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => {
                let owners: Vec<&ResourceInfo> = self.resources.iter().filter(|r| r.uri == uri).collect();
                match owners.as_slice() {
                    [one] => one.server.clone(),
                    [] => return Err(ToolError::fail(format!("Unknown resource '{uri}'. See the list in this tool's description."))),
                    _ => return Err(ToolError::fail("Several servers offer that URI; pass `server`.")),
                }
            }
        };
        ctx.permit("mcp-resource", vec![format!("{server}:{uri}")], vec!["*".into()])?;
        let text = self.manager.read_resource(&server, uri).map_err(ToolError::fail)?;
        Ok(ToolOutput::text(text))
    }
}

/// Used by the engine to tell the UI about problems.
pub fn describe(info: &ServerInfo) -> String {
    match &info.status {
        Status::Connected => format!("connected: {} tool(s), {} prompt(s), {} resource(s)", info.tools, info.prompts, info.resources),
        Status::Pending => "starting".into(),
        Status::Disabled => "disabled".into(),
        Status::Failed(e) => format!("failed: {e}"),
        Status::NeedsAuth(e) => format!("needs authorization: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Env;
    use std::sync::atomic::Ordering;

    pub const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp.py");

    /// The fake server needs python3; where it is missing the MCP tests are skipped (and say so).
    pub fn python() -> Option<&'static str> {
        std::process::Command::new("python3").arg("--version").output().ok().filter(|o| o.status.success()).map(|_| "python3")
    }

    fn spec(name: &str, env: &[(&str, &str)], cwd: &std::path::Path) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            config: McpServerConfig { command: vec!["python3".into(), FAKE.into()], environment: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() },
            cwd: cwd.to_path_buf(),
        }
    }

    fn start(specs: Vec<ServerSpec>, timeouts: McpTimeouts) -> Arc<Manager> {
        let m = Manager::start(specs, &timeouts, &std::env::temp_dir());
        assert!(m.wait_ready(Duration::from_secs(20)), "servers did not settle: {:?}", m.status().iter().map(|s| describe(s)).collect::<Vec<_>>());
        m
    }

    fn tool(m: &Manager, name: &str) -> Arc<dyn Tool> {
        m.tools().into_iter().find(|t| t.name() == name).unwrap_or_else(|| panic!("no tool {name}; have {:?}", m.tools().iter().map(|t| t.name().to_string()).collect::<Vec<_>>()))
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn names_are_sanitized_short_and_unique() {
        assert_eq!(tool_name("my server", "do.thing"), "my_server_do_thing");
        let long = tool_name("srv", &"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert_eq!(long, tool_name("srv", &"x".repeat(100)), "stable");
        assert_ne!(long, tool_name("srv", &format!("{}y", "x".repeat(100))), "different long names stay different");
        assert!(long.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn prompt_arguments_from_what_the_user_typed() {
        let spec = vec![
            PromptArg { name: "who".into(), description: String::new(), required: true },
            PromptArg { name: "tone".into(), description: String::new(), required: false },
        ];
        let get = |s: &str| parse_prompt_args(&spec, s).map(|m| m.into_iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap())).collect::<Vec<_>>());
        assert_eq!(get("Ann warm").unwrap(), ["tone=warm", "who=Ann"]);
        assert_eq!(get("Ann very warm and kind").unwrap(), ["tone=very warm and kind", "who=Ann"], "the last argument takes the rest");
        assert_eq!(get("\"Ann Lee\" warm").unwrap(), ["tone=warm", "who=Ann Lee"], "quotes group words");
        assert_eq!(get("tone=dry who=Bo").unwrap(), ["tone=dry", "who=Bo"]);
        assert_eq!(get("Cy").unwrap(), ["who=Cy"], "optional arguments may be left out");
        assert_eq!(get("tone=dry").unwrap_err(), "missing required argument: who");
        assert_eq!(parse_prompt_args(&[], "ignored words").unwrap().len(), 0);
    }

    #[test]
    fn call_results_map_to_tool_output() {
        let caps = MediaCaps::default();
        let png = base64_png();
        let ok = map_call_result(&json!({"content":[{"type":"text","text":"a"},{"type":"text","text":"b"},{"type":"resource","resource":{"uri":"u://x","text":"inside"}},{"type":"resource_link","uri":"u://y","name":"why"},{"type":"audio","data":"x"},{"type":"image","data":png,"mimeType":"image/png"}]}), caps).unwrap();
        let mut ok = ok;
        assert_eq!(ok.text, "a\nb\n[resource u://x]\ninside\n[link: u://y (why)]\n[audio omitted]");
        assert_eq!(ok.take_media().len(), 1);
        assert_eq!(map_call_result(&json!({"content":[]}), caps).unwrap().text, "(no output)");
        assert!(map_call_result(&json!({"content":[],"structuredContent":{"a":1}}), caps).unwrap().text.contains("\"a\": 1"));
        assert_eq!(map_call_result(&json!({"isError":true,"content":[{"type":"text","text":"nope"}]}), caps).unwrap_err(), ToolError::Failure("nope".into()));
        assert_eq!(map_call_result(&json!({"isError":true,"content":[]}), caps).unwrap_err(), ToolError::Failure("the tool reported an error".into()));
        let mut no_img = map_call_result(&json!({"content":[{"type":"image","data":png,"mimeType":"image/png"}]}), MediaCaps { image: false, pdf: true }).unwrap();
        assert!(no_img.text.contains("does not accept images") && no_img.take_media().is_empty());
        let bad = map_call_result(&json!({"content":[{"type":"image","data":"!!!","mimeType":"image/png"}]}), caps).unwrap();
        assert!(bad.text.contains("could not be used"));
    }

    fn base64_png() -> String {
        use base64::Engine as _;
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&[0, 0, 0, 32, 0, 0, 0, 16, 8, 6, 0, 0, 0, 0, 0, 0, 0]);
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn startup_statuses_names_schemas_and_calls() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-mgr"));
        std::fs::create_dir_all(&dir).unwrap();
        let broken = ServerSpec { name: "broken".into(), config: McpServerConfig { command: vec!["/no/such/program".into()], ..Default::default() }, cwd: dir.clone() };
        let off = ServerSpec { name: "off".into(), config: McpServerConfig { command: vec!["x".into()], disabled: true, ..Default::default() }, cwd: dir.clone() };
        let remote = ServerSpec { name: "cloud".into(), config: McpServerConfig { kind: Some("remote".into()), url: None, ..Default::default() }, cwd: dir.clone() };
        let m = start(vec![spec("fake", &[("MCP_TEST_VAR", "hello")], &dir), broken, off, remote], McpTimeouts::default());

        let status: Vec<(String, String)> = m.status().iter().map(|s| (s.name.clone(), describe(s))).collect();
        assert_eq!(status[0], ("fake".into(), "connected: 9 tool(s), 1 prompt(s), 1 resource(s)".into()), "pagination found the 9th tool");
        assert!(status[1].1.starts_with("failed: cannot start '/no/such/program'"), "{status:?}");
        assert_eq!(status[2].1, "disabled");
        assert_eq!(status[3].1, "failed: a remote server needs a `url`");

        let names: Vec<String> = m.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, ["fake_echo", "fake_fail", "fake_image", "fake_slow", "fake_add_tool", "fake_crash", "fake_structured", "fake_env", "fake_page_two_tool_", "mcp-resource"]);
        let echo = tool(&m, "fake_echo");
        assert_eq!(echo.schema()["required"], json!(["text"]), "the server's own schema is passed through");
        assert_eq!(echo.description(), "Echo the text back");

        let env = Env::new();
        // A normal call, checked for permission under the tool's full name.
        assert_eq!(echo.run(&json!({"text":"hi"}), &env.ctx()).unwrap().text, "echo: hi");
        let asked = env.perm.asked();
        assert_eq!((asked[0].action.as_str(), asked[0].resources.clone()), ("fake_echo", vec!["*".to_string()]));
        // A declined permission never reaches the server.
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(echo.run(&json!({"text":"no"}), &env.ctx()).unwrap_err(), ToolError::Declined);
        *env.perm.reply.lock().unwrap() = Ok(());
        // Errors, images, structured content, environment and working directory.
        assert_eq!(tool(&m, "fake_fail").run(&json!({}), &env.ctx()).unwrap_err(), ToolError::Failure("boom".into()));
        let mut img = tool(&m, "fake_image").run(&json!({}), &env.ctx()).unwrap();
        let caption = img.text.clone();
        assert_eq!((caption.as_str(), img.take_media().len()), ("a caption", 1));
        assert!(tool(&m, "fake_structured").run(&json!({}), &env.ctx()).unwrap().text.contains("\"answer\": 42"));
        let e = tool(&m, "fake_env").run(&json!({}), &env.ctx()).unwrap().text;
        assert!(e.starts_with("hello|") && e.ends_with(dir.file_name().unwrap().to_str().unwrap()), "{e}");
        m.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timeouts_cancellation_crashes_and_reconnecting() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-life"));
        std::fs::create_dir_all(&dir).unwrap();
        let m = start(vec![spec("fake", &[], &dir)], McpTimeouts { execution: Some(300), ..Default::default() });
        let env = Env::new();

        // Slower than the execution timeout.
        let t = Instant::now();
        let e = tool(&m, "fake_slow").run(&json!({}), &env.ctx()).unwrap_err().message();
        assert!(e.contains("fake_slow did not answer within 300 ms") && t.elapsed() < Duration::from_secs(3), "{e}");
        wait_for("the server to be told about the cancellation", || m.stderr_tail("fake").contains("cancelled:"));

        // The user interrupts a call that would have worked.
        let m2 = start(vec![spec("fake", &[], &dir)], McpTimeouts::default());
        let t = Instant::now();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                env.cancel.store(true, Ordering::Relaxed);
            });
            tool(&m2, "fake_slow").run(&json!({}), &env.ctx())
        });
        assert_eq!(r.unwrap_err(), ToolError::Interrupted);
        assert!(t.elapsed() < Duration::from_secs(3));
        env.cancel.store(false, Ordering::Relaxed);
        m2.shutdown();

        // The server dies mid-call: the model is told, the status reflects it, /mcp reconnect fixes it.
        let e = tool(&m, "fake_crash").run(&json!({}), &env.ctx()).unwrap_err().message();
        assert!(e.contains("The MCP server 'fake' stopped") && e.contains("about to crash") && e.contains("/mcp reconnect fake"), "{e}");
        wait_for("the status to show the exit", || m.status()[0].status != Status::Connected);
        assert!(matches!(&m.status()[0].status, Status::Failed(f) if f.contains("exited")), "{:?}", m.status()[0].status);
        assert!(m.tools().is_empty(), "a dead server offers no tools");
        assert!(m.reconnect("fake") && !m.reconnect("nope"));
        assert!(m.wait_ready(Duration::from_secs(20)));
        assert_eq!(m.status()[0].status, Status::Connected);
        assert_eq!(tool(&m, "fake_echo").run(&json!({"text":"again"}), &env.ctx()).unwrap().text, "echo: again");
        m.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn list_changed_notifications_refresh_the_tool_list() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-chg"));
        std::fs::create_dir_all(&dir).unwrap();
        let m = start(vec![spec("fake", &[], &dir)], McpTimeouts::default());
        assert!(!m.tools().iter().any(|t| t.name() == "fake_added"));
        let env = Env::new();
        tool(&m, "fake_add_tool").run(&json!({}), &env.ctx()).unwrap();
        wait_for("the new tool", || m.tools().iter().any(|t| t.name() == "fake_added"));
        assert_eq!(tool(&m, "fake_added").run(&json!({}), &env.ctx()).unwrap().text, "the added tool works");
        m.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompts_and_resources() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-pr"));
        std::fs::create_dir_all(&dir).unwrap();
        let m = start(vec![spec("fake", &[], &dir)], McpTimeouts::default());
        let p = m.prompts();
        assert_eq!((p[0].command.as_str(), p[0].description.as_str(), p[0].arguments.len()), ("fake:greet", "Greets someone", 2));
        assert_eq!(m.get_prompt("fake:greet", "Ann warm").unwrap(), "Say hello to Ann in a warm tone.");
        assert_eq!(m.get_prompt("fake:greet", "who=Bob").unwrap(), "Say hello to Bob in a neutral tone.");
        let e = m.get_prompt("fake:greet", "").unwrap_err();
        assert!(e.contains("missing required argument: who") && e.contains("usage: /fake:greet <who> [tone]"), "{e}");
        assert!(m.get_prompt("fake:nope", "").unwrap_err().contains("no MCP prompt"));

        for token in ["notes", "fake:notes", "file:///notes.txt"] {
            assert_eq!(m.find_resource(token).map(|r| r.uri), Some("file:///notes.txt".to_string()), "{token}");
        }
        assert_eq!(m.find_resource("nothing"), None);
        assert_eq!(m.read_resource("fake", "file:///notes.txt").unwrap(), "remember the milk");
        assert!(m.read_resource("fake", "file:///other").unwrap_err().contains("Resource not found"));
        let env = Env::new();
        let rt = tool(&m, "mcp-resource");
        assert!(rt.description().contains("- fake: file:///notes.txt (notes) - My notes"));
        assert_eq!(rt.run(&json!({"uri":"file:///notes.txt"}), &env.ctx()).unwrap().text, "remember the milk");
        assert_eq!(rt.run(&json!({"uri":"file:///notes.txt","server":"fake"}), &env.ctx()).unwrap().text, "remember the milk");
        assert!(rt.run(&json!({"uri":"file:///zzz"}), &env.ctx()).unwrap_err().message().contains("Unknown resource"));
        assert_eq!(env.perm.asked()[0].action, "mcp-resource");
        m.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn handshake_failures_are_reported_with_a_reason() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-bad"));
        std::fs::create_dir_all(&dir).unwrap();
        let m = start(
            vec![spec("old", &[("FAKE_MCP_VERSION", "1999-01-01")], &dir), spec("mute", &[("FAKE_MCP_NO_INIT", "1")], &dir)],
            McpTimeouts { startup: Some(600), ..Default::default() },
        );
        let s = m.status();
        assert!(matches!(&s[0].status, Status::Failed(f) if f.contains("'1999-01-01'") && f.contains("does not support")), "{:?}", s[0].status);
        assert!(matches!(&s[1].status, Status::Failed(f) if f.contains("no answer to initialize")), "{:?}", s[1].status);
        assert!(m.tools().is_empty());
        m.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_client_answers_server_pings_and_ignores_stdout_noise() {
        if python().is_none() {
            return eprintln!("skipped: python3 not available");
        }
        let dir = std::env::temp_dir();
        let c = Client::spawn(&["python3".into(), FAKE.into()], &dir, &Default::default(), Arc::new(|_, _| {})).unwrap();
        // The server prints a non-JSON line first, and pings the client right after the handshake.
        let info = c.initialize(Duration::from_secs(10)).unwrap();
        assert_eq!(info["serverInfo"]["name"], "fake");
        wait_for("the server to receive our pong", || c.stderr_tail().contains("pong-ok"));
        let r = c.request("tools/list", json!({}), Duration::from_secs(5), None).unwrap();
        assert_eq!(r["nextCursor"], "p2");
        let e = c.request("no/such/method", json!({}), Duration::from_secs(5), None).unwrap_err();
        assert_eq!(e, RpcError::Server { code: -32601, message: "Method not found: no/such/method".into() });
        assert!(Client::spawn(&["/definitely/not/here".into()], &dir, &Default::default(), Arc::new(|_, _| {})).err().unwrap().contains("cannot start"));
        assert_eq!(Client::spawn(&[], &dir, &Default::default(), Arc::new(|_, _| {})).err().unwrap(), "no command configured");
    }
}
