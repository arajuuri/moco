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

/// The largest JSON body, SSE line or SSE event accepted from a server.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// Redirects followed per request, and only within the same origin (`Inner::send`).
const MAX_REDIRECTS: usize = 3;

fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 { format!("{} MiB", bytes >> 20) } else { format!("{bytes} bytes") }
}

/// Complete SSE events (`data:` lines joined, blank-line terminated), including a final unterminated one. A line or an
/// event longer than `limit` bytes ends the stream with an error, so a server cannot make the client buffer without
/// bound. A read error ends the stream quietly.
fn read_events(reader: impl Read, limit: usize, mut on_event: impl FnMut(&str) -> bool) -> Result<(), String> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(reader);
    let (mut line, mut data, mut have_data) = (Vec::new(), String::new(), false);
    loop {
        line.clear();
        // Room for the line ending after a line of exactly `limit` bytes.
        match reader.by_ref().take(limit as u64 + 2).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let bytes = line.strip_suffix(b"\n").unwrap_or(&line);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        if bytes.len() > limit {
            return Err(format!("the server sent an SSE line larger than {}", size(limit)));
        }
        let text = String::from_utf8_lossy(bytes);
        if text.is_empty() {
            if have_data && on_event(&data) {
                return Ok(());
            }
            data.clear();
            have_data = false;
        } else if let Some(d) = text.strip_prefix("data:") {
            let d = d.strip_prefix(' ').unwrap_or(d);
            if data.len() + d.len() + 1 > limit {
                return Err(format!("the server sent an SSE event larger than {}", size(limit)));
            }
            if have_data {
                data.push('\n');
            }
            data.push_str(d);
            have_data = true;
        }
    }
    if have_data {
        on_event(&data);
    }
    Ok(())
}

/// `location` resolved against `from`, if it stays on the same origin (scheme, host and port).
fn same_origin(from: &str, location: &str) -> Option<String> {
    let base = ureq::get(from).request_url().ok()?;
    let next = base.as_url().join(location).ok()?;
    (next.origin() == base.as_url().origin()).then(|| next.to_string())
}

/// Why a 3xx answer that `Inner::send` returned unfollowed ends the request.
fn redirect_refused(resp: &ureq::Response) -> String {
    format!(
        "the server answered with a redirect (HTTP {} to {}) that moco does not follow: it follows at most {MAX_REDIRECTS}, and only on the same host, so that the configured headers go nowhere else. Set the server's `url` to the final address.",
        resp.status(),
        resp.header("location").unwrap_or("?")
    )
}

impl Inner {
    /// Send one request, following up to `MAX_REDIRECTS` redirects within the same origin by sending the same request
    /// (method and body included) to the new address. The agent itself follows none: ureq strips only `authorization`
    /// and `cookie`, so the configured headers and the session id would reach any host (archive#163). A redirect to another
    /// origin, or one too many, is returned unfollowed and the caller refuses it (`redirect_refused`).
    #[allow(clippy::result_large_err)] // the same `Result` as ureq's own `call`, which the callers match on
    fn send(&self, method: &str, timeout: Option<Duration>, body: Option<&str>) -> Result<ureq::Response, ureq::Error> {
        let mut url = self.url.clone();
        for hop in 0..=MAX_REDIRECTS {
            let req = self.build(method, &url, timeout);
            let resp = match body {
                Some(b) => req.send_string(b)?,
                None => req.call()?,
            };
            let next = resp.header("location").and_then(|l| same_origin(&url, l));
            match next {
                Some(next) if (300..400).contains(&resp.status()) && hop < MAX_REDIRECTS => url = next,
                _ => return Ok(resp),
            }
        }
        unreachable!("the last hop returns")
    }

    fn build(&self, method: &str, url: &str, timeout: Option<Duration>) -> ureq::Request {
        let mut req = self.agent.request(method, url);
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
        match self.send("POST", Some(timeout), Some(&body.to_string())) {
            Ok(resp) if (300..400).contains(&resp.status()) => Err(PostErr::Rpc(RpcError::Closed(redirect_refused(&resp)))),
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
            read_events(reply.resp.into_reader(), MAX_MESSAGE, |data| {
                let Ok(v) = serde_json::from_str::<Value>(data) else { return false };
                match v.as_array() {
                    // Every message must be handled, so no short-circuiting `any`.
                    #[allow(clippy::unnecessary_fold)]
                    Some(batch) => batch.iter().map(|m| take(self, m, &mut answer)).fold(false, |a, b| a || b),
                    None => take(self, &v, &mut answer),
                }
            })
            .map_err(RpcError::Closed)?;
        } else if ctype.contains("application/json") {
            let mut text = String::new();
            reply.resp.into_reader().take(MAX_MESSAGE as u64 + 1).read_to_string(&mut text).map_err(|e| RpcError::Closed(format!("reading the response failed: {e}")))?;
            if text.len() > MAX_MESSAGE {
                return Err(RpcError::Closed(format!("the server sent a JSON answer larger than {}", size(MAX_MESSAGE))));
            }
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
        let body = json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocolVersion":OFFERED,"capabilities":{},"clientInfo":{"name":"moco","version":crate::version::RELEASE}}});
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
            match self.send("GET", None, None) {
                Ok(resp) if (300..400).contains(&resp.status()) => {
                    crate::log_warn!("mcp", "{}: notification stream: {}", self.url, redirect_refused(&resp));
                    return;
                }
                Ok(resp) => {
                    backoff = 1;
                    let read = read_events(resp.into_reader(), MAX_MESSAGE, |data| {
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            self.dispatch(&v, Duration::from_secs(10));
                        }
                        self.stop.load(Ordering::Relaxed)
                    });
                    if let Err(e) = read {
                        crate::log_warn!("mcp", "{}: notification stream: {e}; reconnecting", self.url);
                    }
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
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).redirects(0).build(),
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
            let _ = self.inner.send("DELETE", Some(Duration::from_secs(2)), None);
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

    #[test]
    fn redirects_are_followed_on_the_same_host_only() {
        use crate::provider::testserver::{error, serve};
        let Some(fake) = Fake::start(&[]) else { return eprintln!("skipped: python3 not available") };
        let app = tmp();
        // The same origin: /old answers 307 to /mcp, and the same request goes there with its headers.
        let (o, _rx) = opts(&fake, false, &app, &[("X-Custom", "hello")]);
        let c = HttpClient::connect(HttpOptions { url: fake.url().replace("/mcp", "/old"), ..o }, Duration::from_secs(10)).unwrap();
        assert_eq!(call(&c, "echo", json!({"text": "moved"})).unwrap()["content"][0]["text"], "echo: moved");
        assert!(fake.count("redirected POST /old") >= 2 && fake.count("custom=hello") >= 2, "{}", fake.log());
        wait_for("the GET stream through the redirect", || fake.log().contains("GET stream opened"));
        fake.clean();
        c.shutdown();
        wait_for("the session to be closed", || fake.log().contains("DELETE session=sess-1"));
        // Another origin (here another port) is not followed, so the configured headers stay with the configured server.
        let away = serve(vec![error(307, "", vec![("Location", fake.url())])]);
        let (o, _rx) = opts(&fake, false, &app, &[("X-Custom", "secret")]);
        let e = HttpClient::connect(HttpOptions { url: format!("{}/mcp", away.url), ..o }, Duration::from_secs(5)).err().unwrap();
        assert!(matches!(&e, ConnectError::Failed(m) if m.contains(&format!("redirect (HTTP 307 to {})", fake.url())) && m.contains("same host")), "{e:?}");
        assert!(away.requests.lock().unwrap()[0].0.contains("X-Custom: secret"));
        assert!(!fake.log().contains("custom=secret"), "the redirect target never saw the header:\n{}", fake.log());
        let _ = std::fs::remove_dir_all(app);
    }

    #[test]
    fn same_origin_resolves_relative_locations_and_compares_scheme_host_and_port() {
        assert_eq!(same_origin("http://h:8/a/mcp", "/mcp/").as_deref(), Some("http://h:8/mcp/"));
        assert_eq!(same_origin("http://h:8/a/mcp", "next").as_deref(), Some("http://h:8/a/next"));
        assert_eq!(same_origin("https://h/mcp", "https://h:443/x").as_deref(), Some("https://h/x"));
        for other in ["http://h:9/mcp", "https://h:8/mcp", "http://evil:8/mcp", "//evil/mcp"] {
            assert_eq!(same_origin("http://h:8/mcp", other), None, "{other}");
        }
    }

    #[test]
    fn pkce_verifiers_come_from_the_system_rng() {
        // 32 random bytes as unpadded base64url, different every time (`oauth::random_b64` uses getrandom).
        let (a, b) = (oauth::pkce().0, oauth::pkce().0);
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "{a}");
        assert_ne!(a, b);
    }

    fn events(input: &str, limit: usize) -> (Vec<String>, Result<(), String>) {
        let mut seen = vec![];
        let r = read_events(input.as_bytes(), limit, |d| {
            seen.push(d.to_string());
            d == "stop"
        });
        (seen, r)
    }

    #[test]
    fn sse_lines_and_events_are_capped() {
        // Within the cap: data lines joined, CRLF endings, a final unterminated event, stopping when asked.
        assert_eq!(events("data: a\r\ndata:b\r\n\r\n: comment\n\ndata: 0123456789\n", 16), (vec!["a\nb".into(), "0123456789".into()], Ok(())));
        assert_eq!(events("data: stop\n\ndata: never\n\n", 16).0, ["stop"]);
        // A line exactly at the cap is fine; one byte more ends the stream with an error, before any later event.
        assert_eq!(events("data:0123456789\n\n", 15), (vec!["0123456789".into()], Ok(())));
        let (seen, r) = events("data: ok\n\ndata:01234567890\n\ndata: later\n\n", 15);
        assert_eq!((seen, r), (vec!["ok".to_string()], Err("the server sent an SSE line larger than 15 bytes".to_string())));
        // A line that never ends is cut off at the cap, not read to the end.
        let (seen, r) = events(&format!("data:{}", "x".repeat(1000)), 64);
        assert_eq!((seen.len(), r), (0, Err("the server sent an SSE line larger than 64 bytes".to_string())));
        // Many short lines that add up to too much in one event.
        let (seen, r) = events(&"data: 0123456789\n".repeat(10), 64);
        assert_eq!((seen.len(), r), (0, Err("the server sent an SSE event larger than 64 bytes".to_string())));
        assert_eq!(size(MAX_MESSAGE), "64 MiB");
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
