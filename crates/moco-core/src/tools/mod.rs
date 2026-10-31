//! Built-in tools (docs/04-tools.md). Synchronous; cancellation via an `AtomicBool`.
pub mod edit;
pub mod glob;
pub mod grep;
pub mod html;
pub mod patch;
pub mod question;
pub mod read;
pub mod shell;
pub mod shellparse;
pub mod skill;
pub mod subagent;
pub mod todowrite;
pub mod webfetch;
pub mod write;

use serde::{Deserialize, Serialize};
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

/// A file a tool wants the model to see (an image or PDF); the engine stores it as a blob.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub media_type: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

impl ToolOutput {
    pub fn text(t: impl Into<String>) -> Self {
        ToolOutput { text: t.into(), metadata: json!({}) }
    }

    /// Output text plus a file for the model to look at. The bytes travel in `metadata._media` (base64) until
    /// the engine takes them with `take_media`.
    pub fn with_media(text: impl Into<String>, media_type: &str, name: &str, bytes: &[u8]) -> Self {
        use base64::Engine as _;
        ToolOutput {
            text: text.into(),
            metadata: json!({"_media": [{"mediaType": media_type, "name": name, "base64": base64::engine::general_purpose::STANDARD.encode(bytes)}]}),
        }
    }

    /// Remove and return the attachments carried by `with_media`.
    pub fn take_media(&mut self) -> Vec<Attachment> {
        use base64::Engine as _;
        let Some(list) = self.metadata.as_object_mut().and_then(|m| m.remove("_media")) else { return vec![] };
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some(Attachment {
                    media_type: e["mediaType"].as_str()?.to_string(),
                    name: e["name"].as_str().unwrap_or("attachment").to_string(),
                    bytes: base64::engine::general_purpose::STANDARD.decode(e["base64"].as_str()?).ok()?,
                })
            })
            .collect()
    }
}

/// What the current model accepts as input besides text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaCaps {
    pub image: bool,
    pub pdf: bool,
}

impl Default for MediaCaps {
    fn default() -> Self {
        MediaCaps { image: true, pdf: true }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    /// `pending`, `in_progress`, `completed` or `cancelled`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubagentRequest {
    pub agent: String,
    pub description: String,
    pub prompt: String,
    pub model: Option<String>,
    /// Continue an earlier child session instead of starting a new one.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubagentResult {
    pub session_id: String,
    pub output: String,
}

/// Engine services that some tools need. Implemented by the engine; tests use a stub.
pub trait Host: Sync {
    fn set_todos(&self, todos: Vec<Todo>) -> Result<(), ToolError>;
    /// One answer list (chosen labels or free text) per question; `Declined` if the user dismissed the form.
    fn ask(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, ToolError>;
    fn subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError>;
}

pub struct ToolCtx<'a> {
    pub media: MediaCaps,
    pub session_id: &'a str,
    pub host: &'a dyn Host,
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
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Every built-in tool. `subagents` lists `(id, description)` of the agents the subagent tool may start.
pub fn all(subagents: Vec<(String, String)>, skills: Vec<crate::skills::Skill>) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(read::Read),
        Box::new(write::Write),
        Box::new(edit::Edit),
        Box::new(patch::Patch),
        Box::new(glob::Glob),
        Box::new(grep::Grep),
        Box::new(shell::Shell),
        Box::new(webfetch::WebFetch),
        Box::new(todowrite::TodoWrite),
        Box::new(question::AskQuestion),
        Box::new(subagent::Subagent { agents: subagents }),
    ];
    // Only offered when there is something to load: an empty skill tool is noise in every request.
    if !skills.is_empty() {
        tools.push(Box::new(skill::SkillTool { skills }));
    }
    tools
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

    #[derive(Default)]
    pub struct TestHost {
        pub todos: Mutex<Vec<Todo>>,
        pub asked: Mutex<Vec<Vec<Question>>>,
        pub answers: Mutex<Option<Vec<Vec<String>>>>,
        pub subagents: Mutex<Vec<SubagentRequest>>,
    }
    impl Host for TestHost {
        fn set_todos(&self, todos: Vec<Todo>) -> Result<(), ToolError> {
            *self.todos.lock().unwrap() = todos;
            Ok(())
        }
        fn ask(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, ToolError> {
            self.asked.lock().unwrap().push(questions.to_vec());
            self.answers.lock().unwrap().clone().ok_or(ToolError::Declined)
        }
        fn subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> {
            self.subagents.lock().unwrap().push(req.clone());
            Ok(SubagentResult { session_id: "ses_child".into(), output: format!("done: {}", req.prompt) })
        }
    }

    pub struct Env {
        pub dir: PathBuf,
        pub perm: TestPermitter,
        pub cancel: AtomicBool,
        pub host: TestHost,
    }
    impl Env {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(crate::id::new_id("moco-tool"));
            std::fs::create_dir_all(dir.join("work")).unwrap();
            Env { dir, perm: TestPermitter::allow(), cancel: AtomicBool::new(false), host: TestHost::default() }
        }
        pub fn work(&self) -> PathBuf {
            self.dir.join("work")
        }
        pub fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx { media: MediaCaps::default(), session_id: "ses_test", host: &self.host, cwd: self.work(), project_root: self.work(), spill_dir: self.dir.join("spill"), permitter: &self.perm, cancel: &self.cancel }
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
