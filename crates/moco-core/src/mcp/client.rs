//! JSON-RPC 2.0 over a child process's stdio, as used by MCP servers: one JSON message per line.
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use crate::tools::shell::try_reap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Protocol revisions this client can speak. The server picks one in its `initialize` answer.
pub const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const OFFERED: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// The server wants credentials (HTTP 401); carries its `WWW-Authenticate` challenge.
    Unauthorized(String),
    Timeout,
    Cancelled,
    /// The server process ended or its pipe broke; carries the tail of its stderr for diagnosis.
    Closed(String),
    Server { code: i64, message: String },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Unauthorized(_) => write!(f, "authorization required"),
            RpcError::Timeout => write!(f, "timed out"),
            RpcError::Cancelled => write!(f, "cancelled"),
            RpcError::Closed(m) if m.is_empty() => write!(f, "the server exited"),
            RpcError::Closed(m) => write!(f, "the server exited: {m}"),
            RpcError::Server { message, .. } => write!(f, "{message}"),
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, RpcError>>>>>;
pub type NotifyFn = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Wraps a connection to log what crosses it: every request with its duration (a failure, with the server's stderr
/// tail, at `warn`), and notifications at `trace`. Tool arguments and results are never logged, only the tool's name.
pub struct LoggedConn {
    name: String,
    inner: Arc<dyn McpConn>,
}

impl LoggedConn {
    pub fn wrap(name: &str, inner: Arc<dyn McpConn>) -> Arc<dyn McpConn> {
        Arc::new(LoggedConn { name: name.to_string(), inner })
    }
}

impl McpConn for LoggedConn {
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, RpcError> {
        let what = if method == "tools/call" { format!("{method} {}", params["name"].as_str().unwrap_or("?")) } else { method.to_string() };
        let started = Instant::now();
        let r = self.inner.request(method, params, timeout, cancel);
        let ms = started.elapsed().as_millis();
        match &r {
            Ok(_) => crate::log_debug!("mcp", "server={} {what} ok ms={ms}", self.name),
            Err(RpcError::Cancelled) => crate::log_debug!("mcp", "server={} {what} cancelled ms={ms}", self.name),
            Err(e) => {
                let tail = crate::log::tail(&self.inner.stderr_tail(), 5);
                crate::log_warn!("mcp", "server={} {what} failed after {ms} ms (timeout {} ms): {e}{}", self.name, timeout.as_millis(), tail_suffix(&tail));
            }
        }
        r
    }
    fn notify(&self, method: &str, params: Value) {
        crate::log_trace!("mcp", "server={} notify {method}", self.name);
        self.inner.notify(method, params)
    }
    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }
    fn info(&self) -> Value {
        self.inner.info()
    }
    fn shutdown(&self) {
        crate::log_debug!("mcp", "server={} shutdown", self.name);
        self.inner.shutdown()
    }
}

/// A connection to one MCP server, whatever it runs over (a child process's stdio, or HTTP).
pub trait McpConn: Send + Sync {
    /// Send a request and wait for its answer, up to `timeout`; abandoned (and the server told) when `cancel` is set.
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, RpcError>;
    fn notify(&self, method: &str, params: Value);
    fn is_alive(&self) -> bool;
    /// The tail of the server's stderr (stdio only), for explaining failures.
    fn stderr_tail(&self) -> String;
    /// The server's `initialize` answer.
    fn info(&self) -> Value;
    fn shutdown(&self);
}

impl McpConn for Client {
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, RpcError> {
        Client::request(self, method, params, timeout, cancel)
    }
    fn notify(&self, method: &str, params: Value) {
        Client::notify(self, method, params)
    }
    fn is_alive(&self) -> bool {
        Client::is_alive(self)
    }
    fn stderr_tail(&self) -> String {
        Client::stderr_tail(self)
    }
    fn info(&self) -> Value {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn shutdown(&self) {
        Client::shutdown(self)
    }
}

/// Is `version` a protocol revision this client speaks?
pub fn supported(version: &str) -> bool {
    SUPPORTED.contains(&version)
}

/// The longest line read from a server's stdout; the rest of a longer line is dropped.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Stderr is only kept for diagnosis, so its lines are cut much shorter.
const MAX_STDERR_LINE_BYTES: usize = 64 * 1024;
/// Messages waiting for the writer thread; a server that stops reading stdin fills this and is treated as stuck.
const WRITE_QUEUE: usize = 256;

/// Read one line into `buf` (cleared first, without the newline), keeping at most `max` bytes: the rest of a longer
/// line is read and dropped, so memory stays bounded. `Ok(None)` at end of file, else whether the line was cut.
fn read_line_capped(r: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> std::io::Result<Option<bool>> {
    buf.clear();
    let (mut any, mut cut) = (false, false);
    loop {
        let chunk = match r.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(any.then_some(cut));
        }
        any = true;
        let (line, used, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (&chunk[..i], i + 1, true),
            None => (chunk, chunk.len(), false),
        };
        let room = max.saturating_sub(buf.len());
        buf.extend_from_slice(&line[..line.len().min(room)]);
        cut |= line.len() > room;
        r.consume(used);
        if done {
            return Ok(Some(cut));
        }
    }
}

struct Inner {
    /// Feeds the writer thread, which alone touches the child's stdin: a server that stops reading blocks only that
    /// thread, never a request's timeout or the reader thread. `None` once shut down.
    stdin: Mutex<Option<SyncSender<String>>>,
    pending: Pending,
    next: AtomicU64,
    exited: AtomicBool,
    /// Set when the stderr reader hit end of file, so a dead server's last words are all in `stderr_tail`.
    stderr_done: AtomicBool,
    stderr_tail: Mutex<VecDeque<String>>,
}

pub struct Client {
    inner: Arc<Inner>,
    /// Set by the first `shutdown`, so the later ones (`Drop` after an explicit call) do nothing.
    shut_down: AtomicBool,
    child: Mutex<Child>,
    pid: u32,
    /// From the `initialize` answer.
    pub info: Mutex<Value>,
}

/// Signal the process group `pid` leads. Only call this while the leader is still unreaped (see `try_reap`): after
/// that the id may belong to someone else.
fn kill_group(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

impl Inner {
    /// Queue `msg` for the writer thread. Never blocks.
    fn write(&self, msg: &Value) -> Result<(), RpcError> {
        let guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tx) = guard.as_ref() else { return Err(RpcError::Closed(self.tail())) };
        let mut line = msg.to_string();
        line.push('\n');
        match tx.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(RpcError::Closed(format!("the server is not reading its input{}", tail_suffix(&self.tail())))),
            Err(TrySendError::Disconnected(_)) => Err(RpcError::Closed(self.tail())),
        }
    }

    fn tail(&self) -> String {
        self.stderr_tail.lock().map(|t| t.iter().rev().take(5).rev().cloned().collect::<Vec<_>>().join(" | ")).unwrap_or_default()
    }

    fn fail_all(&self) {
        // stdout closed first; give the stderr thread a moment to store the last lines the server wrote.
        for _ in 0..40 {
            if self.stderr_done.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let tail = self.tail();
        for (_, tx) in self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            let _ = tx.send(Err(RpcError::Closed(tail.clone())));
        }
    }
}

impl Client {
    /// Start `command` (program and arguments) in `cwd` with `env` added to the inherited environment.
    /// `notify` is called, from the reader thread, for every notification the server sends.
    pub fn spawn(command: &[String], cwd: &Path, env: &BTreeMap<String, String>, notify: NotifyFn) -> Result<Arc<Client>, String> {
        let Some((program, args)) = command.split_first() else { return Err("no command configured".into()) };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot start '{program}': {e}"))?;
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (wtx, wrx) = sync_channel::<String>(WRITE_QUEUE);
        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin.is_some().then_some(wtx)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
            exited: AtomicBool::new(false),
            stderr_done: AtomicBool::new(false),
            stderr_tail: Mutex::new(VecDeque::new()),
        });
        let pid = child.id();

        if let Some(mut stdin) = stdin {
            let wr = inner.clone();
            std::thread::spawn(move || {
                while let Ok(line) = wrx.recv() {
                    if stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                        // Close the queue so later writes fail at once, and fail what is waiting.
                        wr.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
                        wr.fail_all();
                        return;
                    }
                }
            });
        }

        let err_inner = inner.clone();
        std::thread::spawn(move || {
            let (mut reader, mut buf) = (BufReader::new(stderr), Vec::new());
            while let Ok(Some(_)) = read_line_capped(&mut reader, &mut buf, MAX_STDERR_LINE_BYTES) {
                let line = String::from_utf8_lossy(&buf).into_owned();
                let mut t = err_inner.stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.len() >= 20 {
                    t.pop_front();
                }
                t.push_back(line.chars().take(300).collect());
            }
            err_inner.stderr_done.store(true, Ordering::Relaxed);
        });

        let rd = inner.clone();
        std::thread::spawn(move || {
            let (mut reader, mut buf) = (BufReader::new(stdout), Vec::new());
            while let Ok(Some(cut)) = read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
                if cut {
                    crate::log_warn!("mcp", "dropped a line over {MAX_LINE_BYTES} bytes from the server");
                    continue;
                }
                // Servers sometimes log to stdout; anything that is not a JSON object is ignored.
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
                if !msg.is_object() {
                    continue;
                }
                match (msg.get("id"), msg.get("method").and_then(Value::as_str)) {
                    (Some(id), None) => {
                        let Some(id) = id.as_u64() else { continue };
                        let Some(tx) = rd.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else { continue };
                        let _ = tx.send(match msg.get("error") {
                            Some(e) => Err(RpcError::Server { code: e["code"].as_i64().unwrap_or(0), message: e["message"].as_str().unwrap_or("server error").to_string() }),
                            None => Ok(msg["result"].clone()),
                        });
                    }
                    (Some(id), Some(method)) => {
                        // A request from the server. `ping` is answered; moco offers no client features, so the rest are refused.
                        let reply = if method == "ping" {
                            json!({"jsonrpc":"2.0","id":id,"result":{}})
                        } else {
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not supported by this client: {method}")}})
                        };
                        let _ = rd.write(&reply);
                    }
                    (None, Some(method)) => notify(method, &msg["params"]),
                    _ => {}
                }
            }
            rd.exited.store(true, Ordering::Relaxed);
            rd.fail_all();
        });

        Ok(Arc::new(Client { inner, shut_down: AtomicBool::new(false), child: Mutex::new(child), pid, info: Mutex::new(Value::Null) }))
    }

    pub fn is_alive(&self) -> bool {
        !self.inner.exited.load(Ordering::Relaxed)
    }

    pub fn stderr_tail(&self) -> String {
        self.inner.tail()
    }

    /// Send a request and wait for its answer, up to `timeout`. When `cancel` becomes true the request is abandoned and
    /// the server is told (`notifications/cancelled`).
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, RpcError> {
        if !self.is_alive() {
            return Err(RpcError::Closed(self.stderr_tail()));
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        if let Err(e) = self.inner.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})) {
            self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(r) => return r,
                Err(RecvTimeoutError::Disconnected) => return Err(RpcError::Closed(self.stderr_tail())),
                Err(RecvTimeoutError::Timeout) => {
                    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));
                    if cancelled || Instant::now() >= deadline {
                        self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        self.notify("notifications/cancelled", json!({"requestId": id, "reason": if cancelled { "cancelled by the user" } else { "timed out" }}));
                        return Err(if cancelled { RpcError::Cancelled } else { RpcError::Timeout });
                    }
                }
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        if let Err(e) = self.inner.write(&json!({"jsonrpc":"2.0","method":method,"params":params})) {
            crate::log_debug!("mcp", "could not send notification {method}: {e}");
        }
    }

    /// The handshake: `initialize`, check the protocol revision, `notifications/initialized`.
    pub fn initialize(&self, timeout: Duration) -> Result<Value, String> {
        let res = self
            .request(
                "initialize",
                json!({"protocolVersion": OFFERED, "capabilities": {}, "clientInfo": {"name": "moco", "version": crate::version::RELEASE}}),
                timeout,
                None,
            )
            .map_err(|e| match e {
                RpcError::Timeout => format!("no answer to initialize within {} s{}", timeout.as_secs(), tail_suffix(&self.stderr_tail())),
                other => format!("initialize failed: {other}"),
            })?;
        let version = res["protocolVersion"].as_str().unwrap_or("");
        if !supported(version) {
            return Err(format!("the server speaks protocol revision '{version}', which this client does not support (supported: {})", SUPPORTED.join(", ")));
        }
        self.notify("notifications/initialized", json!({}));
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = res.clone();
        Ok(res)
    }

    /// Stop the server: close its stdin (a well-behaved server exits), then terminate its process group. The group is
    /// only signalled while its leader is unreaped, and only by the first call.
    pub fn shutdown(&self) {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..10 {
            // Kills the stragglers in the group, then reaps the leader.
            if matches!(try_reap(&mut child), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        kill_group(self.pid, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(100));
        kill_group(self.pid, libc::SIGKILL);
        let _ = child.wait();
    }
}

fn tail_suffix(t: &str) -> String {
    if t.is_empty() { String::new() } else { format!(" (stderr: {t})") }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_cut_and_the_next_line_is_intact() {
        let data = format!("{}\nshort\nlast", "x".repeat(100));
        let mut r = std::io::Cursor::new(data.into_bytes());
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut r, &mut buf, 10).unwrap(), Some(true));
        assert_eq!(buf, b"xxxxxxxxxx");
        assert_eq!(read_line_capped(&mut r, &mut buf, 10).unwrap(), Some(false));
        assert_eq!(buf, b"short");
        assert_eq!(read_line_capped(&mut r, &mut buf, 10).unwrap(), Some(false));
        assert_eq!(buf, b"last");
        assert_eq!(read_line_capped(&mut r, &mut buf, 10).unwrap(), None);
    }

    fn spawn(script: &str) -> Arc<Client> {
        let cmd = ["sh".to_string(), "-c".to_string(), script.to_string()];
        Client::spawn(&cmd, Path::new("/"), &BTreeMap::new(), Arc::new(|_, _| {})).unwrap()
    }

    #[test]
    fn a_server_that_never_reads_cannot_stop_the_timeout() {
        // Never reads stdin; a request bigger than the pipe buffer used to block in write_all forever.
        let c = spawn("sleep 30");
        let big = "x".repeat(1024 * 1024);
        let t = Instant::now();
        let r = c.request("x", json!({"big": big}), Duration::from_millis(300), None);
        assert_eq!(r, Err(RpcError::Timeout));
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
        c.shutdown();
    }

    #[test]
    fn shutdown_is_idempotent() {
        let c = spawn("sleep 30 & exec sleep 30");
        let t = Instant::now();
        c.shutdown();
        c.shutdown();
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
