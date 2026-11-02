//! MCP "Streamable HTTP" transport. EXPERIMENTAL: tested against a local fake server; not verified against real
//! services.
//!
//! Every JSON-RPC message is POSTed to the one MCP endpoint. For a request the server answers with a JSON body or with an
//! SSE stream that ends with the response (and may carry notifications and server requests first). The server may hand
//! out a session id (`Mcp-Session-Id`) that every later request must carry; a 404 means the session is gone and the client
//! initializes again. An optional GET stream delivers server-initiated notifications. A 401 triggers one token refresh;
//! signing in is `oauth::login`.
use super::client::{supported, McpConn, NotifyFn, RpcError, OFFERED};
use super::oauth;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    /// The server wants a sign-in (`/mcp auth <name>`).
    NeedsAuth(String),
    Failed(String),
}

pub struct HttpOptions {
    pub url: String,
    /// Sent with every request (for example a static `Authorization` header).
    pub headers: BTreeMap<String, String>,
    /// Use stored OAuth tokens (false when the entry says `oauth: false`).
    pub oauth: bool,
    pub app_dir: PathBuf,
    pub notify: NotifyFn,
}

struct Inner {
    url: String,
    headers: BTreeMap<String, String>,
    oauth: bool,
    app_dir: PathBuf,
    agent: ureq::Agent,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    next: AtomicU64,
    stop: AtomicBool,
    info: Mutex<Value>,
    notify: NotifyFn,
}

pub struct HttpClient {
    inner: Arc<Inner>,
}

enum PostErr {
    Rpc(RpcError),
    /// 404 on a request that carried a session id.
    SessionExpired,
}

struct Reply {
    resp: ureq::Response,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Complete SSE events (`data:` lines joined, blank-line terminated), including a final unterminated one.
fn read_events(reader: impl Read, mut on_event: impl FnMut(&str) -> bool) {
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(reader).lines();
    let mut data: Vec<String> = vec![];
    while let Some(Ok(line)) = lines.next() {
        if line.is_empty() {
            if !data.is_empty() && on_event(&data.join("\n")) {
                return;
            }
            data.clear();
        } else if let Some(d) = line.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
        }
    }
    if !data.is_empty() {
        on_event(&data.join("\n"));
    }
}

impl Inner {
    fn build(&self, method: &str, timeout: Option<Duration>) -> ureq::Request {
        let mut req = self.agent.request(method, &self.url);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        req = match method {
            "POST" => req.set("Accept", "application/json, text/event-stream").set("Content-Type", "application/json"),
            "GET" => req.set("Accept", "text/event-stream"),
            _ => req,
        };
        if let Some(v) = lock(&self.version).clone() {
            req = req.set("MCP-Protocol-Version", &v);
        }
        if let Some(s) = lock(&self.session).clone() {
            req = req.set("Mcp-Session-Id", &s);
        }
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        if self.oauth && !self.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")) {
            if let Some(t) = oauth::bearer(&self.app_dir, &self.url) {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
        }
        req
    }

    fn post_raw(&self, body: &Value, timeout: Duration) -> Result<Reply, PostErr> {
        let had_session = lock(&self.session).is_some();
        match self.build("POST", Some(timeout)).send_string(&body.to_string()) {
            Ok(resp) => {
                if let Some(s) = resp.header("mcp-session-id") {
                    *lock(&self.session) = Some(s.to_string());
                }
                Ok(Reply { resp })
            }
            Err(ureq::Error::Status(401, r)) => Err(PostErr::Rpc(RpcError::Unauthorized(r.header("www-authenticate").unwrap_or("").to_string()))),
            Err(ureq::Error::Status(404, _)) if had_session => Err(PostErr::SessionExpired),
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["message"].as_str().map(String::from)).unwrap_or_else(|| format!("HTTP {code}: {}", text.chars().take(200).collect::<String>()));
                Err(PostErr::Rpc(RpcError::Server { code: code as i64, message }))
            }
            Err(ureq::Error::Transport(t)) if t.to_string().to_lowercase().contains("timed out") => Err(PostErr::Rpc(RpcError::Timeout)),
            Err(ureq::Error::Transport(t)) => Err(PostErr::Rpc(RpcError::Closed(t.to_string()))),
        }
    }

    /// Answer or announce one message from the server.
    fn dispatch(&self, msg: &Value, timeout: Duration) {
        match (msg.get("id"), msg["method"].as_str()) {
            (Some(id), Some(method)) => {
                let reply = if method == "ping" {
                    json!({"jsonrpc":"2.0","id":id,"result":{}})
                } else {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not supported by this client: {method}")}})
                };
                let _ = self.post_raw(&reply, timeout);
            }
            (None, Some(method)) => (self.notify)(method, &msg["params"]),
            _ => {}
        }
    }

    /// The response to request `id` from a reply, handling whatever else arrives on the way.
    fn read_response(&self, reply: Reply, id: u64, timeout: Duration) -> Result<Value, RpcError> {
        let ctype = reply.resp.header("content-type").unwrap_or("").to_lowercase();
        let status = reply.resp.status();
        let mut answer: Option<Result<Value, RpcError>> = None;
        let take = |this: &Inner, msg: &Value, answer: &mut Option<Result<Value, RpcError>>| -> bool {
            if msg["id"].as_u64() == Some(id) && msg.get("method").is_none() {
                *answer = Some(match msg.get("error") {
                    Some(e) => Err(RpcError::Server { code: e["code"].as_i64().unwrap_or(0), message: e["message"].as_str().unwrap_or("server error").to_string() }),
                    None => Ok(msg["result"].clone()),
                });
                true
            } else {
                this.dispatch(msg, timeout);
                false
            }
        };
        if ctype.contains("text/event-stream") {
            read_events(reply.resp.into_reader(), |data| {
                let Ok(v) = serde_json::from_str::<Value>(data) else { return false };
                match v.as_array() {
                    // Every message must be handled, so no short-circuiting `any`.
                    #[allow(clippy::unnecessary_fold)]
                    Some(batch) => batch.iter().map(|m| take(self, m, &mut answer)).fold(false, |a, b| a || b),
                    None => take(self, &v, &mut answer),
                }
            });
        } else if ctype.contains("application/json") {
            let mut text = String::new();
            reply.resp.into_reader().take(64 * 1024 * 1024).read_to_string(&mut text).map_err(|e| RpcError::Closed(format!("reading the response failed: {e}")))?;
            let v: Value = serde_json::from_str(&text).map_err(|e| RpcError::Closed(format!("the server answered with invalid JSON: {e}")))?;
            match v.as_array() {
                Some(batch) => batch.iter().for_each(|m| {
                    take(self, m, &mut answer);
                }),
                None => {
                    take(self, &v, &mut answer);
                }
            }
        } else {
            return Err(RpcError::Closed(format!("unexpected answer (HTTP {status}, content type '{ctype}') to a request")));
        }
        answer.unwrap_or_else(|| Err(RpcError::Closed("the response stream ended without an answer".into())))
    }

    /// One request/response, with a token refresh after a 401 and a fresh session after a 404.
    fn exchange(&self, id: u64, body: &Value, timeout: Duration) -> Result<Value, RpcError> {
        let (mut refreshed, mut reinit) = (false, false);
        let is_initialize = body["method"] == "initialize";
        loop {
            match self.post_raw(body, timeout) {
                Ok(reply) => return self.read_response(reply, id, timeout),
                Err(PostErr::Rpc(RpcError::Unauthorized(www))) => {
                    if self.oauth && !refreshed && oauth::load(&self.app_dir, &self.url).is_some_and(|s| s.refresh_token.is_some()) {
                        refreshed = true;
                        crate::log_info!("mcp", "{} answered 401; trying the refresh token", self.url);
                        if oauth::refresh(&self.app_dir, &self.url).is_ok() {
                            continue;
                        }
                    }
                    crate::log_warn!("mcp", "{} answered 401 and there is no way to renew the credentials", self.url);
                    return Err(RpcError::Unauthorized(www));
                }
                Err(PostErr::SessionExpired) if !reinit && !is_initialize => {
                    crate::log_info!("mcp", "{} dropped the session; starting a new one", self.url);
                    reinit = true;
                    *lock(&self.session) = None;
                    self.handshake(timeout)?;
                }
                Err(PostErr::SessionExpired) => return Err(RpcError::Closed("the server dropped the session".into())),
                Err(PostErr::Rpc(e)) => return Err(e),
            }
        }
    }

    fn handshake(&self, timeout: Duration) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocolVersion":OFFERED,"capabilities":{},"clientInfo":{"name":"moco","version":env!("CARGO_PKG_VERSION")}}});
        *lock(&self.version) = None;
        let res = self.exchange(id, &body, timeout)?;
        let version = res["protocolVersion"].as_str().unwrap_or("").to_string();
        if !supported(&version) {
            return Err(RpcError::Closed(format!("the server speaks protocol revision '{version}', which this client does not support")));
        }
        *lock(&self.version) = Some(version);
        *lock(&self.info) = res.clone();
        let _ = self.post_raw(&json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}), timeout);
        Ok(res)
    }

    /// The optional server-to-client stream. Servers that do not offer one answer 405 and this ends quietly.
    fn listen(self: Arc<Self>) {
        let mut backoff = 1u64;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            match self.build("GET", None).call() {
                Ok(resp) => {
                    backoff = 1;
                    read_events(resp.into_reader(), |data| {
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            self.dispatch(&v, Duration::from_secs(10));
                        }
                        self.stop.load(Ordering::Relaxed)
                    });
                }
                Err(ureq::Error::Status(400 | 401 | 403 | 404 | 405 | 406 | 501, _)) => return,
                Err(_) => {}
            }
            let end = Instant::now() + Duration::from_secs(backoff);
            while Instant::now() < end {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            backoff = (backoff * 2).min(30);
        }
    }
}

impl HttpClient {
    /// Open the connection: the `initialize` handshake, then the notification stream.
    pub fn connect(opts: HttpOptions, startup: Duration) -> Result<Arc<HttpClient>, ConnectError> {
        let inner = Arc::new(Inner {
            url: opts.url,
            headers: opts.headers,
            oauth: opts.oauth,
            app_dir: opts.app_dir,
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).redirects(3).build(),
            session: Mutex::new(None),
            version: Mutex::new(None),
            next: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            info: Mutex::new(Value::Null),
            notify: opts.notify,
        });
        match inner.handshake(startup) {
            Ok(_) => {}
            Err(RpcError::Unauthorized(w)) => {
                let how = if inner.oauth { "run /mcp auth <name> to sign in" } else { "check the headers configured for this server" };
                return Err(ConnectError::NeedsAuth(format!("the server requires authorization; {how}{}", if w.is_empty() { String::new() } else { format!(" ({w})") })));
            }
            Err(RpcError::Timeout) => return Err(ConnectError::Failed(format!("no answer to initialize within {} s", startup.as_secs()))),
            Err(e) => return Err(ConnectError::Failed(format!("initialize failed: {e}"))),
        }
        let l = inner.clone();
        std::thread::spawn(move || l.listen());
        Ok(Arc::new(HttpClient { inner }))
    }
}

impl McpConn for HttpClient {
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, RpcError> {
        if self.inner.stop.load(Ordering::Relaxed) {
            return Err(RpcError::Closed("the connection was closed".into()));
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let (tx, rx) = channel();
        let inner = self.inner.clone();
        // The exchange blocks on the network; it runs on its own thread so a cancel or a deadline can end the wait.
        std::thread::spawn(move || {
            let _ = tx.send(inner.exchange(id, &body, timeout + Duration::from_secs(2)));
        });
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(r) => return r,
                Err(RecvTimeoutError::Disconnected) => return Err(RpcError::Closed("the request thread ended".into())),
                Err(RecvTimeoutError::Timeout) => {
                    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));
                    if cancelled || Instant::now() >= deadline {
                        self.notify("notifications/cancelled", json!({"requestId": id, "reason": if cancelled { "cancelled by the user" } else { "timed out" }}));
                        return Err(if cancelled { RpcError::Cancelled } else { RpcError::Timeout });
                    }
                }
            }
        }
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.inner.post_raw(&json!({"jsonrpc":"2.0","method":method,"params":params}), Duration::from_secs(10));
    }

    fn is_alive(&self) -> bool {
        !self.inner.stop.load(Ordering::Relaxed)
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    fn info(&self) -> Value {
        lock(&self.inner.info).clone()
    }

    fn shutdown(&self) {
        if self.inner.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        // End the session politely; servers may answer 405 and that is fine.
        if lock(&self.inner.session).is_some() {
            let _ = self.inner.build("DELETE", Some(Duration::from_secs(2))).call();
        }
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OAuthConfig;
    use crate::mcp::oauth::{self, LoginIo};
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{channel, Receiver};

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp_http.py");

    struct Fake {
        child: Child,
        port: u16,
        log: PathBuf,
    }
    impl Fake {
        fn start(flags: &[&str]) -> Option<Fake> {
            Command::new("python3").arg("--version").output().ok().filter(|o| o.status.success())?;
            let log = std::env::temp_dir().join(format!("{}.log", crate::id::new_id("fakehttp")));
            let mut child = Command::new("python3").arg(FAKE).arg(&log).args(flags).stdout(Stdio::piped()).spawn().unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
            Some(Fake { child, port: line.trim().parse().unwrap(), log })
        }
        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/mcp", self.port)
        }
        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
        fn count(&self, needle: &str) -> usize {
            self.log().lines().filter(|l| l.contains(needle)).count()
        }
        fn clean(&self) {
            assert!(!self.log().contains("VIOLATION"), "the client broke the protocol:\n{}", self.log());
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = std::fs::remove_file(&self.log);
        }
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn opts(fake: &Fake, oauth: bool, app_dir: &std::path::Path, headers: &[(&str, &str)]) -> (HttpOptions, Receiver<String>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let notify: NotifyFn = Arc::new(move |m, _| {
            let _ = tx.lock().unwrap().send(m.to_string());
        });
        (HttpOptions { url: fake.url(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), oauth, app_dir: app_dir.to_path_buf(), notify }, rx)
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id("http-app"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(c: &HttpClient, name: &str, args: Value) -> Result<Value, RpcError> {
        c.request("tools/call", json!({"name": name, "arguments": args}), Duration::from_secs(10), None)
    }

    #[test]
    fn json_and_sse_answers_sessions_server_requests_and_the_notification_stream() {
        let Some(fake) = Fake::start(&[]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        let (o, rx) = opts(&fake, false, &app, &[("X-Custom", "hello")]);
        let c = HttpClient::connect(o, Duration::from_secs(10)).unwrap();
        assert_eq!(c.info()["serverInfo"]["name"], "fake-http");
        assert_eq!(c.request("tools/list", json!({}), Duration::from_secs(5), None).unwrap()["tools"].as_array().unwrap().len(), 6);
        // A plain JSON answer, then an SSE answer that carries a notification and a ping to the client first.
        assert_eq!(call(&c, "echo", json!({"text": "hi"})).unwrap()["content"][0]["text"], "echo: hi");
        assert_eq!(call(&c, "sse_echo", json!({"text": "hi"})).unwrap()["content"][0]["text"], "sse: hi");
        wait_for("the server to receive our pong", || fake.log().contains("pong-ok"));
        let seen: Vec<String> = rx.try_iter().collect();
        assert!(seen.contains(&"notifications/message".to_string()), "{seen:?}");
        assert_eq!(c.request("prompts/get", json!({"name":"greet","arguments":{"who":"Ann"}}), Duration::from_secs(5), None).unwrap()["messages"][0]["content"]["text"], "Say hi to Ann.");
        // The notification stream: a tool added on the server arrives as list_changed.
        wait_for("the GET stream", || fake.log().contains("GET stream opened"));
        call(&c, "add_tool", json!({})).unwrap();
        wait_for("list_changed over the GET stream", || rx.try_iter().any(|m| m == "notifications/tools/list_changed"));
        assert!(fake.log().contains("custom=hello"), "configured headers are sent");
        fake.clean();
        c.shutdown();
        wait_for("the session to be closed", || fake.log().contains("DELETE session=sess-1"));
        assert!(!c.is_alive());
        assert!(matches!(c.request("tools/list", json!({}), Duration::from_secs(1), None), Err(RpcError::Closed(_))));
        let _ = std::fs::remove_dir_all(app);
    }

    #[test]
    fn a_dropped_session_is_replaced_transparently() {
        let Some(fake) = Fake::start(&["--expire-session-once"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        let (o, _rx) = opts(&fake, false, &app, &[]);
        let c = HttpClient::connect(o, Duration::from_secs(10)).unwrap();
        let tools = c.request("tools/list", json!({}), Duration::from_secs(5), None).unwrap();
        assert_eq!(tools["tools"].as_array().unwrap().len(), 6, "the request succeeded after the client started a new session");
        assert_eq!((fake.count("POST initialize"), fake.count("session expired")), (2, 1));
        assert_eq!(call(&c, "echo", json!({"text": "after"})).unwrap()["content"][0]["text"], "echo: after");
        fake.clean();
        let _ = std::fs::remove_dir_all(app);
    }

    #[test]
    fn errors_timeouts_cancellation_and_servers_without_a_stream() {
        let Some(fake) = Fake::start(&["--no-get"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        let (o, _rx) = opts(&fake, false, &app, &[]);
        let c = HttpClient::connect(o, Duration::from_secs(10)).unwrap();
        assert_eq!(call(&c, "echo", json!({"text":"works without a GET stream"})).unwrap()["content"][0]["text"], "echo: works without a GET stream");
        assert_eq!(call(&c, "nope", json!({})).unwrap_err(), RpcError::Server { code: -32602, message: "Unknown tool: nope".into() });
        assert_eq!(call(&c, "boom", json!({})).unwrap_err(), RpcError::Server { code: 500, message: "HTTP 500: oops".into() });
        let t = Instant::now();
        assert_eq!(c.request("tools/call", json!({"name":"slow","arguments":{}}), Duration::from_millis(300), None), Err(RpcError::Timeout));
        assert!(t.elapsed() < Duration::from_secs(3));
        wait_for("the server to hear about the cancellation", || fake.log().contains("cancelled:"));
        let cancel = AtomicBool::new(false);
        let t = Instant::now();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Ordering::Relaxed);
            });
            c.request("tools/call", json!({"name":"slow","arguments":{}}), Duration::from_secs(10), Some(&cancel))
        });
        assert_eq!(r, Err(RpcError::Cancelled));
        assert!(t.elapsed() < Duration::from_secs(3));
        // Nothing listening: a clear failure, not a hang.
        let dead = Fake::start(&[]).map(|f| f.url()).unwrap();
        let (o2, _) = opts(&fake, false, &app, &[]);
        let o2 = HttpOptions { url: dead, ..o2 };
        let e = HttpClient::connect(o2, Duration::from_secs(5)).err().unwrap();
        assert!(matches!(e, ConnectError::Failed(m) if m.starts_with("initialize failed")));
        let _ = std::fs::remove_dir_all(app);
    }

    // ---- OAuth -------------------------------------------------------------------------------------------

    /// What the user's browser does: open the authorization URL, follow the redirect to the local listener.
    fn browser_redirect(url: &str) -> String {
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let r = agent.get(url).call().expect("the authorization endpoint redirects");
        assert_eq!(r.status(), 302, "authorize answered {}", r.status());
        r.header("location").unwrap().to_string()
    }

    fn sign_in(app: &std::path::Path, fake: &Fake, cfg: &OAuthConfig, how: &str) -> Result<(), String> {
        let cancel = AtomicBool::new(false);
        let shown = Mutex::new(vec![]);
        let show = |u: &str| shown.lock().unwrap().push(u.to_string());
        let open = |u: &str| {
            if how == "browser" {
                let u = u.to_string();
                std::thread::spawn(move || {
                    let _ = ureq::get(&browser_redirect(&u)).call();
                });
            }
        };
        let paste = || -> Option<String> {
            match how {
                "paste" => Some(browser_redirect(&shown.lock().unwrap()[0])),
                "paste-wrong-state" => Some(browser_redirect(&shown.lock().unwrap()[0]).replace("state=", "state=x")),
                _ => None,
            }
        };
        let io = LoginIo { show: &show, open: &open, paste: &paste, cancel: &cancel };
        oauth::login(app, &fake.url(), &Default::default(), cfg, Duration::from_secs(20), &io)
    }

    #[test]
    fn signing_in_connecting_and_reusing_the_registration() {
        let Some(fake) = Fake::start(&["--auth"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        let (o, _) = opts(&fake, true, &app, &[]);
        let Err(ConnectError::NeedsAuth(m)) = HttpClient::connect(o, Duration::from_secs(5)) else { panic!("an unauthenticated connection must ask for a sign-in") };
        assert!(m.contains("/mcp auth") && m.contains("resource_metadata"), "{m}");

        sign_in(&app, &fake, &OAuthConfig { scope: None, ..Default::default() }, "browser").unwrap();
        let stored = oauth::load(&app, &fake.url()).unwrap();
        assert_eq!((stored.client_id.as_str(), stored.access_token.as_deref(), stored.refresh_token.as_deref()), ("cid-1", Some("tok-1"), Some("ref-1")));
        let (o, _) = opts(&fake, true, &app, &[]);
        let c = HttpClient::connect(o, Duration::from_secs(5)).expect("connects with the stored token");
        assert_eq!(call(&c, "echo", json!({"text":"signed in"})).unwrap()["content"][0]["text"], "echo: signed in");
        // A second sign-in reuses the registered client instead of registering again.
        sign_in(&app, &fake, &OAuthConfig::default(), "browser").unwrap();
        assert_eq!((fake.count("registered"), fake.count("token issued")), (1, 2));
        assert!(fake.log().contains("authorize ok scope=mcp.read"), "the scope advertised by the resource metadata is requested: {}", fake.log());
        fake.clean(); // PKCE verified, resource, redirect uri, client id
        // A pre-registered client id skips registration altogether.
        let app2 = tmp();
        sign_in(&app2, &fake, &OAuthConfig { client_id: Some("preset-client".into()), scope: Some("custom".into()), ..Default::default() }, "browser").unwrap();
        assert_eq!((fake.count("registered"), oauth::load(&app2, &fake.url()).unwrap().client_id.as_str()), (1, "preset-client"));
        assert!(fake.log().contains("scope=custom"));
        fake.clean();
        let _ = std::fs::remove_dir_all(app);
        let _ = std::fs::remove_dir_all(app2);
    }

    #[test]
    fn expired_tokens_are_refreshed_and_dead_credentials_ask_for_a_new_sign_in() {
        let Some(fake) = Fake::start(&["--auth", "--expires", "3600"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        sign_in(&app, &fake, &OAuthConfig::default(), "browser").unwrap();
        // Pretend the access token ran out.
        let mut s = oauth::load(&app, &fake.url()).unwrap();
        s.expires_at = Some(1);
        oauth::save(&app, &fake.url(), &s).unwrap();
        let (o, _) = opts(&fake, true, &app, &[]);
        let c = HttpClient::connect(o, Duration::from_secs(5)).expect("refreshed on the way");
        assert!(fake.log().contains("refreshed -> tok-2"));
        let s = oauth::load(&app, &fake.url()).unwrap();
        assert_eq!((s.access_token.as_deref(), s.refresh_token.as_deref()), (Some("tok-2"), Some("ref-2")), "rotated refresh tokens are kept");
        assert!(call(&c, "echo", json!({"text":"x"})).is_ok());
        // Revoked on the server side and no way to refresh: the user must sign in again.
        let mut dead = s;
        dead.access_token = Some("bogus".into());
        dead.refresh_token = Some("bogus".into());
        dead.expires_at = None;
        oauth::save(&app, &fake.url(), &dead).unwrap();
        let (o, _) = opts(&fake, true, &app, &[]);
        assert!(matches!(HttpClient::connect(o, Duration::from_secs(5)), Err(ConnectError::NeedsAuth(_))));
        assert!(fake.log().contains("refresh refused"));
        // With `oauth: false` stored tokens are ignored and the message says what to check instead.
        let (o, _) = opts(&fake, false, &app, &[]);
        let Err(ConnectError::NeedsAuth(m)) = HttpClient::connect(o, Duration::from_secs(5)) else { panic!() };
        assert!(m.contains("headers"), "{m}");
        let _ = std::fs::remove_dir_all(app);
    }

    #[test]
    fn a_static_authorization_header_is_enough_when_it_is_valid() {
        let Some(fake) = Fake::start(&["--auth"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        // The fake accepts only tokens it issued, so a made-up header must be refused with a clear message.
        let (o, _) = opts(&fake, false, &app, &[("Authorization", "Bearer made-up")]);
        assert!(matches!(HttpClient::connect(o, Duration::from_secs(5)), Err(ConnectError::NeedsAuth(_))));
        sign_in(&app, &fake, &OAuthConfig::default(), "browser").unwrap();
        let token = oauth::load(&app, &fake.url()).unwrap().access_token.unwrap();
        let other_app = tmp(); // no stored tokens here: only the header can authenticate
        let (o, _) = opts(&fake, false, &other_app, &[("Authorization", &format!("Bearer {token}"))]);
        let c = HttpClient::connect(o, Duration::from_secs(5)).expect("the header carries it");
        assert!(call(&c, "echo", json!({"text":"x"})).is_ok());
        let _ = std::fs::remove_dir_all(app);
        let _ = std::fs::remove_dir_all(other_app);
    }

    #[test]
    fn sign_in_variants_and_failures() {
        let Some(fake) = Fake::start(&["--auth"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        // The browser could not reach the listener: the user pastes the redirect URL.
        sign_in(&app, &fake, &OAuthConfig::default(), "paste").unwrap();
        assert!(oauth::load(&app, &fake.url()).unwrap().access_token.is_some());
        // A redirect that does not belong to this attempt is refused.
        let e = sign_in(&tmp(), &fake, &OAuthConfig::default(), "paste-wrong-state").unwrap_err();
        assert!(e.contains("state mismatch"), "{e}");
        fake.clean();
        // The user says no.
        let deny = Fake::start(&["--auth", "--deny"]).unwrap();
        let e = sign_in(&tmp(), &deny, &OAuthConfig::default(), "browser").unwrap_err();
        assert!(e.contains("access_denied") && e.contains("user said no"), "{e}");
        // No metadata documents anywhere: the conventional /authorize, /token, /register are used.
        let bare = Fake::start(&["--auth", "--no-metadata"]).unwrap();
        let app3 = tmp();
        sign_in(&app3, &bare, &OAuthConfig::default(), "browser").unwrap();
        assert!(oauth::load(&app3, &bare.url()).is_some());
        bare.clean();
        // The user never comes back: cancelling ends the wait.
        let cancel = AtomicBool::new(true);
        let (show, open, paste) = (|_: &str| {}, |_: &str| {}, || -> Option<String> { None });
        let io = LoginIo { show: &show, open: &open, paste: &paste, cancel: &cancel };
        assert_eq!(oauth::login(&tmp(), &fake.url(), &Default::default(), &OAuthConfig::default(), Duration::from_secs(20), &io), Err("cancelled".to_string()));
        let _ = std::fs::remove_dir_all(app);
        let _ = std::fs::remove_dir_all(app3);
    }

    #[test]
    fn the_manager_reports_needs_auth_signs_in_and_signs_out() {
        use crate::config::McpServerConfig;
        use crate::mcp::{Manager, ServerSpec, Status};
        let Some(fake) = Fake::start(&["--auth"]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        let spec = ServerSpec { name: "cloud".into(), config: McpServerConfig { url: Some(fake.url()), ..Default::default() }, cwd: app.clone() };
        let m = Manager::start(vec![spec], &Default::default(), &app);
        assert!(m.wait_ready(Duration::from_secs(15)));
        assert!(matches!(&m.status()[0].status, Status::NeedsAuth(msg) if msg.contains("/mcp auth cloud")), "{:?}", m.status()[0].status);
        assert!(m.tools().is_empty());

        let cancel = AtomicBool::new(false);
        let (show, paste) = (|_: &str| {}, || -> Option<String> { None });
        let open = |u: &str| {
            let u = u.to_string();
            std::thread::spawn(move || {
                let _ = ureq::get(&browser_redirect(&u)).call();
            });
        };
        let io = LoginIo { show: &show, open: &open, paste: &paste, cancel: &cancel };
        m.authorize("cloud", &io).unwrap();
        assert_eq!(m.status()[0].status, Status::Connected);
        assert_eq!(m.status()[0].tools, 6);
        assert!(m.tools().iter().any(|t| t.name() == "cloud_echo"));
        assert_eq!(m.get_prompt("cloud:greet", "Bo").unwrap(), "Say hi to Bo.");
        assert_eq!(m.read_resource("cloud", "mem://note").unwrap(), "remote note text");

        assert_eq!(m.logout("cloud"), Ok(true));
        assert!(m.wait_ready(Duration::from_secs(15)));
        assert!(matches!(m.status()[0].status, Status::NeedsAuth(_)));
        assert_eq!(m.logout("cloud"), Ok(false), "nothing left to forget");
        assert!(m.authorize("nope", &io).is_err() && m.logout("nope").is_err());
        m.shutdown();
        fake.clean();
        let _ = std::fs::remove_dir_all(app);
    }
}
