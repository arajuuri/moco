//! Built-in tools (docs/04-tools.md). Synchronous; cancellation via an `AtomicBool`.
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod shell;
pub mod shellparse;
pub mod write;

use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// Model-visible failure; the model can fix its call.
    Failure(String),
    /// User declined without feedback: aborts the step's tool batch.
    Declined,
    /// User declined with feedback: shown to the model as the tool error.
    Corrected(String),
    /// Blocked by a deny rule.
    Blocked(String),
    Interrupted,
}

impl ToolError {
    pub fn fail(msg: impl Into<String>) -> Self {
        ToolError::Failure(msg.into())
    }
    /// Text the model sees for this error.
    pub fn message(&self) -> String {
        match self {
            ToolError::Failure(m) | ToolError::Blocked(m) => m.clone(),
            ToolError::Corrected(f) => format!("The user rejected this tool call with feedback: {f}"),
            ToolError::Declined => "The user rejected this tool call.".into(),
            ToolError::Interrupted => "Tool execution interrupted".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub metadata: Value,
}
impl ToolOutput {
    pub fn text(t: impl Into<String>) -> Self {
        ToolOutput { text: t.into(), metadata: json!({}) }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PermReq {
    pub action: String,
    pub resources: Vec<String>,
    /// Patterns persisted by an "always" reply.
    pub save: Vec<String>,
    /// Ask even if the rules allow (configured denies still win). Used for unparsed shell commands.
    pub force_ask: bool,
}

/// Implemented by the agent loop: evaluates rules, prompts the user, blocks until answered.
pub trait Permitter: Sync {
    fn check(&self, req: &PermReq) -> Result<(), ToolError>;
}

pub struct ToolCtx<'a> {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// Where truncated / long outputs are written.
    pub spill_dir: PathBuf,
    pub permitter: &'a dyn Permitter,
    pub cancel: &'a AtomicBool,
}

impl ToolCtx<'_> {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        normalize(&if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) })
    }

    pub fn is_internal(&self, p: &Path) -> bool {
        p.starts_with(&self.cwd) || p.starts_with(&self.project_root)
    }

    /// Relative to the working directory when internal, absolute otherwise.
    pub fn display(&self, p: &Path) -> String {
        if self.is_internal(p) {
            p.strip_prefix(&self.cwd).or_else(|_| p.strip_prefix(&self.project_root)).unwrap_or(p).to_string_lossy().into_owned()
        } else {
            p.to_string_lossy().into_owned()
        }
    }

    pub fn permit(&self, action: &str, resources: Vec<String>, save: Vec<String>) -> Result<(), ToolError> {
        self.permitter.check(&PermReq { action: action.into(), resources, save, force_ask: false })
    }

    /// Anything outside the working directory and project root needs `external_directory`.
    /// `dir` is the directory being touched (the parent for a file).
    pub fn check_external(&self, dir: &Path) -> Result<(), ToolError> {
        if self.is_internal(dir) {
            return Ok(());
        }
        let root = project_root_of(dir);
        self.permit(
            "external_directory",
            vec![format!("{}/*", dir.display())],
            vec![format!("{}/*", root.display())],
        )
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Nearest ancestor containing `.git`, else the directory itself.
pub fn project_root_of(dir: &Path) -> PathBuf {
    dir.ancestors().find(|a| a.join(".git").exists()).unwrap_or(dir).to_path_buf()
}

/// Lexical normalization (`.`/`..`), no symlink resolution.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(read::Read),
        Box::new(write::Write),
        Box::new(edit::Edit),
        Box::new(glob::Glob),
        Box::new(grep::Grep),
        Box::new(shell::Shell),
    ]
}

// ---- input helpers -------------------------------------------------------------------------

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::fail(format!("`{key}` is required and must be a string")))
}
pub(crate) fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    // Models emit the literal strings "undefined"/"null" for unset paths.
    input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "undefined" && *s != "null")
}
pub(crate) fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}
pub(crate) fn opt_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::sync::Mutex;

    pub struct TestPermitter {
        pub asked: Mutex<Vec<PermReq>>,
        pub reply: Mutex<Result<(), ToolError>>,
    }
    impl TestPermitter {
        pub fn allow() -> Self {
            TestPermitter { asked: Mutex::new(vec![]), reply: Mutex::new(Ok(())) }
        }
        pub fn asked(&self) -> Vec<PermReq> {
            self.asked.lock().unwrap().clone()
        }
    }
    impl Permitter for TestPermitter {
        fn check(&self, req: &PermReq) -> Result<(), ToolError> {
            self.asked.lock().unwrap().push(req.clone());
            self.reply.lock().unwrap().clone()
        }
    }

    pub struct Env {
        pub dir: PathBuf,
        pub perm: TestPermitter,
        pub cancel: AtomicBool,
    }
    impl Env {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(crate::id::new_id("moco-tool"));
            std::fs::create_dir_all(dir.join("work")).unwrap();
            Env { dir, perm: TestPermitter::allow(), cancel: AtomicBool::new(false) }
        }
        pub fn work(&self) -> PathBuf {
            self.dir.join("work")
        }
        pub fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx { cwd: self.work(), project_root: self.work(), spill_dir: self.dir.join("spill"), permitter: &self.perm, cancel: &self.cancel }
        }
        pub fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
            let p = self.work().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
