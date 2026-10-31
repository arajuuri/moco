//! JSON-RPC 2.0 over a child process's stdio, as used by MCP servers: one JSON message per line.
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
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

struct Inner {
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    next: AtomicU64,
    exited: AtomicBool,
    /// Set when the stderr reader hit end of file, so a dead server's last words are all in `stderr_tail`.
    stderr_done: AtomicBool,
    stderr_tail: Mutex<VecDeque<String>>,
}

pub struct Client {
    inner: Arc<Inner>,
    child: Mutex<Child>,
    pid: u32,
    /// From the `initialize` answer.
    pub info: Mutex<Value>,
}

fn kill_group(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

impl Inner {
    fn write(&self, msg: &Value) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(stdin) = guard.as_mut() else { return Err(RpcError::Closed(self.tail())) };
        let mut line = msg.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).map_err(|_| RpcError::Closed(self.tail()))
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
        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
            exited: AtomicBool::new(false),
            stderr_done: AtomicBool::new(false),
            stderr_tail: Mutex::new(VecDeque::new()),
        });
        let pid = child.id();

        let err_inner = inner.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
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
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Servers sometimes log to stdout; anything that is not a JSON object is ignored.
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
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

        Ok(Arc::new(Client { inner, child: Mutex::new(child), pid, info: Mutex::new(Value::Null) }))
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
        let _ = self.inner.write(&json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    /// The handshake: `initialize`, check the protocol revision, `notifications/initialized`.
    pub fn initialize(&self, timeout: Duration) -> Result<Value, String> {
        let res = self
            .request(
                "initialize",
                json!({"protocolVersion": OFFERED, "capabilities": {}, "clientInfo": {"name": "moco", "version": env!("CARGO_PKG_VERSION")}}),
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

    /// Stop the server: close its stdin (a well-behaved server exits), then terminate its process group.
    pub fn shutdown(&self) {
        self.inner.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..10 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                kill_group(self.pid, libc::SIGKILL); // stragglers in the group
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
