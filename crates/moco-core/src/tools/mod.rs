//! Built-in tools (docs/04-tools.md). Synchronous; cancellation via an `AtomicBool`.
pub mod edit;
pub mod fsio;
pub mod glob;
pub mod grep;
pub mod html;
pub mod jobs;
pub mod patch;
pub mod question;
#[cfg(test)]
mod race_tests;
pub mod read;
pub mod shell;
pub mod shellparse;
pub mod skill;
pub mod subagent;
pub mod todowrite;
pub mod webfetch;
pub mod websearch;
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

    /// Add another file for the model to see (see `with_media`).
    pub fn add_media(&mut self, media_type: &str, name: &str, bytes: &[u8]) {
        use base64::Engine as _;
        let entry = json!({"mediaType": media_type, "name": name, "base64": base64::engine::general_purpose::STANDARD.encode(bytes)});
        if let Some(list) = self.metadata.get_mut("_media").and_then(Value::as_array_mut) {
            list.push(entry);
        } else {
            self.metadata["_media"] = json!([entry]);
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
    /// For `shell`: the whole command line, which the command classifier judges (`classifier.rs`).
    pub command: Option<String>,
    /// The id of the tool call asking, so the front end can tie what happens to the request to that call's row.
    pub call_id: String,
}

/// Implemented by the agent loop: evaluates rules, prompts the user, blocks until answered.
pub trait Permitter: Sync {
    fn check(&self, req: &PermReq) -> Result<(), ToolError>;
    /// What `check` would decide from the rules (configured, agent and saved), without prompting anyone. For files a
    /// tool finds rather than is given (a directory grep), which cannot each be a prompt. A permitter that cannot tell
    /// says `Ask`, so such files are left out.
    fn evaluate(&self, _req: &PermReq) -> crate::permission::Effect {
        crate::permission::Effect::Ask
    }
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

/// One background job (a shell command or a subagent) as shown by the `jobs` tool and `/jobs`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobInfo {
    pub id: String,
    /// `shell` or `subagent`
    pub kind: String,
    /// The command line or the task description.
    pub title: String,
    /// `running`, `exited (code N)`, `stopped`, `finished`, `failed: ...`
    pub status: String,
    pub running: bool,
    pub seconds: u64,
    pub output_path: Option<PathBuf>,
}

/// Engine services that some tools need. Implemented by the engine; tests use a stub.
pub trait Host: Sync {
    fn set_todos(&self, todos: Vec<Todo>) -> Result<(), ToolError>;
    /// One answer list (chosen labels or free text) per question; `Declined` if the user dismissed the form.
    fn ask(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, ToolError>;
    fn subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError>;
    /// Provider credential variable names that shell children must not inherit.
    fn provider_key_env_vars(&self) -> Vec<String> {
        Vec::new()
    }
    /// Start a command that keeps running after the tool call returns; returns its job id and output file.
    /// `unsandboxed` skips the shell sandbox (already approved by the caller).
    fn start_background_shell(&self, command: &str, workdir: &Path, timeout: Option<std::time::Duration>, unsandboxed: bool) -> Result<(String, PathBuf), ToolError>;
    /// Start a subagent without waiting for it; returns its session id.
    fn start_background_subagent(&self, req: SubagentRequest) -> Result<String, ToolError>;
    fn jobs(&self) -> Vec<JobInfo>;
    /// Output of a job so far (its last `lines` lines) with its status.
    fn job_output(&self, id: &str, lines: usize) -> Result<String, ToolError>;
    fn kill_job(&self, id: &str) -> Result<String, ToolError>;
}

pub struct ToolCtx<'a> {
    pub formatters: &'a crate::format::Formatters,
    pub media: MediaCaps,
    pub session_id: &'a str,
    pub host: &'a dyn Host,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// Where truncated / long outputs are written.
    pub spill_dir: PathBuf,
    pub permitter: &'a dyn Permitter,
    pub cancel: &'a AtomicBool,
    /// The model's id for this tool call.
    pub call_id: &'a str,
}

impl ToolCtx<'_> {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        normalize(&if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) })
    }

    /// Inside the working directory or project, or in the folder where long tool output is kept (so the model can
    /// read what a truncated command printed without an `external_directory` prompt). Paths are compared with symlinks
    /// resolved: a project reached through a symlinked parent is internal under either name, and a symlink inside the
    /// project that leads out of it is not.
    pub fn is_internal(&self, p: &Path) -> bool {
        self.is_internal_real(&real_path(p))
    }

    /// `is_internal` for a path already resolved by `real_path`.
    fn is_internal_real(&self, real: &Path) -> bool {
        [&self.cwd, &self.project_root, &self.spill_dir].iter().any(|root| real.starts_with(real_path(root)))
    }

    /// Relative to the working directory when internal, absolute otherwise. The path is shown as it was given.
    pub fn display(&self, p: &Path) -> String {
        if self.is_internal(p) {
            let rel = |base: &Path| p.strip_prefix(base).ok().map(Path::to_path_buf);
            let real = real_path(p);
            let rel_real = |base: &Path| real.strip_prefix(real_path(base)).ok().map(Path::to_path_buf);
            rel(&self.cwd)
                .or_else(|| rel(&self.project_root))
                .or_else(|| rel_real(&self.cwd))
                .or_else(|| rel_real(&self.project_root))
                .unwrap_or_else(|| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        } else {
            p.to_string_lossy().into_owned()
        }
    }

    pub fn permit(&self, action: &str, resources: Vec<String>, save: Vec<String>) -> Result<(), ToolError> {
        self.permitter.check(&PermReq { action: action.into(), resources, save, force_ask: false, command: None, call_id: self.call_id.into() })
    }

    /// `permit` for a file or directory: the resources are the path as given and, when symlinks lead somewhere else
    /// (a linked final component or a linked parent), the place they lead to. A rule such as `read .env` therefore
    /// applies to `safe.txt -> .env`, and a deny on the target cannot be bypassed through a link.
    pub fn permit_path(&self, action: &str, path: &Path, save: Vec<String>) -> Result<(), ToolError> {
        self.permit_path_at(action, path, &real_path(path), save)
    }

    /// `permit_path` with the path's target already resolved (`real_path`). The file tools resolve once, check, and
    /// then use exactly `real` through `crate::nofollow`, which refuses any symlink in it, so a link swapped in after
    /// the check is not followed (archive#165).
    pub fn permit_path_at(&self, action: &str, path: &Path, real: &Path, save: Vec<String>) -> Result<(), ToolError> {
        self.permit(action, self.path_resources_at(path, real), save)
    }

    /// The rules' verdict on `action` for a path, named as `permit_path` names it, without prompting (`Permitter::evaluate`).
    pub fn path_effect(&self, action: &str, path: &Path) -> crate::permission::Effect {
        self.permitter.evaluate(&PermReq { action: action.into(), resources: self.path_resources(path), save: vec![], force_ask: false, command: None, call_id: self.call_id.into() })
    }

    /// The permission resources for a path: its name as given and, when links lead elsewhere, what they lead to.
    fn path_resources(&self, path: &Path) -> Vec<String> {
        self.path_resources_at(path, &real_path(path))
    }

    fn path_resources_at(&self, path: &Path, real: &Path) -> Vec<String> {
        // The name as given: a link that lives in the project keeps its relative name even when it points outside
        // (`display` would resolve it and show it absolute); the target is checked as its own resource below.
        let lexical = normalize(path);
        let name = [&self.cwd, &self.project_root]
            .iter()
            .find_map(|base| lexical.strip_prefix(normalize(base)).ok().filter(|r| !r.as_os_str().is_empty()))
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.display(path));
        let mut resources = vec![name];
        if real != normalize(path) {
            // `real` as it is, not resolved again: for a patch `Delete File` it is the symlink itself, and `display`
            // would follow it (then a project reached through a symlinked parent, such as `/tmp` -> `/private/tmp`,
            // named the link's target as well). For the other tools `real` is already symlink-free, so this is
            // what `display` gives.
            let target = [&self.cwd, &self.project_root]
                .iter()
                .find_map(|base| real.strip_prefix(real_path(base)).ok())
                .map_or_else(|| real.to_string_lossy().into_owned(), |r| r.to_string_lossy().into_owned());
            if !resources.contains(&target) {
                resources.push(target);
            }
        }
        resources
    }

    /// Anything outside the working directory and project root needs `external_directory`.
    /// `dir` is the directory being touched (the parent for a file).
    pub fn check_external(&self, dir: &Path) -> Result<(), ToolError> {
        // The symlink-free name, so what the user allows once or always holds for every spelling of the directory. The
        // rules see only this name, so the user's own temp directory, which the mode rules allow
        // (`permission::temp_dir_rules`), cannot be used to reach anything else through a link in it.
        self.check_external_at(&real_path(dir))
    }

    /// `check_external` for a directory already resolved by `real_path`: the one name both judged and then used.
    pub fn check_external_at(&self, dir: &Path) -> Result<(), ToolError> {
        if self.is_internal_real(dir) {
            return Ok(());
        }
        let root = always_root_of(dir);
        self.permit(
            "external_directory",
            vec![format!("{}/*", dir.display())],
            vec![format!("{}/*", root.display())],
        )
    }

    /// `check_external` for a file a tool reads or writes: its folder, and, when the file is a symlink, the folder of
    /// what it points to (a link inside the project can lead out of it).
    pub fn check_external_file(&self, path: &Path) -> Result<(), ToolError> {
        self.check_external_file_at(path, &real_path(path))
    }

    /// `check_external_file` with the file's target already resolved (`real_path`), as `permit_path_at` takes it.
    /// The folder of `real` is always the one judged, whatever the links in `path` lead to by now.
    pub fn check_external_file_at(&self, path: &Path, real: &Path) -> Result<(), ToolError> {
        let dir = real_path(path.parent().unwrap_or(path));
        self.check_external_at(&dir)?;
        match real.parent() {
            Some(t) if t != dir => self.check_external_at(t),
            _ => Ok(()),
        }
    }

    /// Run the configured formatter on a file a tool just wrote (`written` is what it wrote). Only files inside the
    /// project are touched. Returns text for the tool's output when the formatter changed the file or failed; when it
    /// changed the file the text lists the lines it changed, so the model need not read the file again.
    pub fn format_file(&self, path: &Path, written: &[u8]) -> Option<String> {
        if self.formatters.is_empty() || !self.is_internal(path) || path.starts_with(&self.spill_dir) {
            return None;
        }
        let outcome = self.formatters.run(path, &self.cwd);
        let changes = match outcome {
            crate::format::Outcome::Changed { .. } => std::fs::read(path).ok().and_then(|after| fsio::changes(written, &after)),
            _ => None,
        };
        crate::format::note_with_changes(&outcome, changes)
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Nearest ancestor containing a `.git` the user owns, else the directory itself. A `.git` at `/`, in the home
/// directory or above it (a dotfiles repo) does not count, nor does one owned by someone else (planted in `/tmp`):
/// everything under the root is internal, so either would turn off the external-directory checks there (archive#120).
pub fn project_root_of(dir: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    project_root_in(dir, home.as_deref(), &owned_by_user)
}

fn project_root_in(dir: &Path, home: Option<&Path>, owned: &dyn Fn(&Path) -> bool) -> PathBuf {
    let found = dir.ancestors().find(|a| {
        let git = a.join(".git");
        git.exists() && owned(&git)
    });
    let home = home.map(real_path);
    match found {
        Some(a) if a.parent().is_some() && !home.is_some_and(|h| h.starts_with(real_path(a))) => a.to_path_buf(),
        _ => dir.to_path_buf(),
    }
}

/// Whether the user running moco owns `p` itself (a symlink is judged by its own owner), as git's `safe.directory`
/// check does.
fn owned_by_user(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::geteuid() };
    std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == me)
}

/// What an "always" answer to `external_directory` covers: the project root of `dir`, or, when `dir` has none and lies
/// in a scratch folder of the user's in a shared temp directory (`/private/tmp/moco_head/src`), that whole scratch
/// folder (`/private/tmp/moco_head`), so the next subfolder does not ask again (archive#67). `dir` is symlink-free.
fn always_root_of(dir: &Path) -> PathBuf {
    let root = project_root_of(dir);
    if root != dir {
        return root;
    }
    scratch_folder_of(dir, unsafe { libc::geteuid() }).unwrap_or(root)
}

/// The ancestor of `dir` (or `dir` itself) that sits directly in a shared, sticky, world-writable directory such as
/// `/private/tmp`, `/var/tmp` or `/Users/Shared`, if `me` owns it and nobody else can write to it. A folder another
/// user made there (or one anyone can add to) is not widened: "always" then covers only `dir`.
fn scratch_folder_of(dir: &Path, me: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let shared = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_dir() && m.mode() & 0o1002 == 0o1002);
    let top = dir.ancestors().find(|a| a.parent().is_some_and(shared))?;
    let m = std::fs::symlink_metadata(top).ok()?;
    (m.is_dir() && m.uid() == me && m.mode() & 0o022 == 0).then(|| top.to_path_buf())
}

/// `external_directory` is allowed in the user's own temp directory (`user_temp_dir`, archive#67): a long session works in
/// scratch folders there and would otherwise ask again and again. These are rules, not an exemption in
/// `check_external`, and `config::resolve_agents_in` places them right after the mode's and the preset's rules, so
/// whatever comes later still wins: the explore agent's `external_directory * ask` (it reads the most untrusted text,
/// so it keeps asking there), and the user's own `permissions` (a `deny` or `ask` for the temp directory). moco's data
/// directory, when `MOCO_HOME` puts it in the temp directory, still asks, since it holds the provider keys. The shared
/// `/tmp` is not covered.
pub fn temp_dir_rules() -> Vec<crate::permission::Rule> {
    temp_dir_rules_for(user_temp_dir().as_deref(), &crate::auth::app_dir())
}

fn temp_dir_rules_for(tmp: Option<&Path>, app_dir: &Path) -> Vec<crate::permission::Rule> {
    use crate::permission::{Effect, Rule};
    let Some(tmp) = tmp else { return vec![] };
    let mut rules = vec![Rule::new("external_directory", &format!("{}/*", tmp.display()), Effect::Allow)];
    let app = real_path(app_dir);
    if app.starts_with(tmp) {
        rules.push(Rule::new("external_directory", &format!("{}/*", app.display()), Effect::Ask));
    }
    rules
}

/// The user's own temp directory, `$TMPDIR` with symlinks resolved (on macOS the per-user, mode 700
/// `/private/var/folders/xx/.../T`), if it is safe to let the file tools into it without an `external_directory`
/// prompt (archive#67). It must be a directory the user owns that no one else can write to, under parents that only root or
/// the user can change (or that are sticky, like `/tmp` for `/tmp/me`), and it must not be `/`, the home directory or
/// above it. The shared `/tmp` itself fails these checks (it is world-writable), and so does an unset `TMPDIR`.
pub fn user_temp_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(tmp) = TEST_TEMP_DIR.with(|t| t.borrow().clone()) {
        return private_temp_dir(&tmp, None, unsafe { libc::geteuid() });
    }
    let tmp = std::env::var_os("TMPDIR").filter(|t| !t.is_empty())?;
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    private_temp_dir(Path::new(&tmp), home.as_deref(), unsafe { libc::geteuid() })
}

// Tests set this instead of `TMPDIR`: the variable is process-wide (`std::env::temp_dir` reads it in every test), this
// is per thread.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_TEMP_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn private_temp_dir(dir: &Path, home: Option<&Path>, me: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    if !dir.is_absolute() {
        return None;
    }
    let real = std::fs::canonicalize(dir).ok()?;
    // It becomes a permission pattern: no wildcard characters (and no `\`, which patterns read as `/`).
    let plain = real.to_str().is_some_and(|s| !s.contains(|c| matches!(c, '*' | '?' | '\\')));
    if !plain || real.parent().is_none() || home.map(real_path).is_some_and(|h| h.starts_with(&real)) {
        return None;
    }
    let m = std::fs::metadata(&real).ok()?;
    if !m.is_dir() || m.uid() != me || m.mode() & 0o022 != 0 {
        return None;
    }
    let safe_parent = |p: &Path| std::fs::metadata(p).is_ok_and(|m| (m.uid() == 0 || m.uid() == me) && (m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0));
    real.ancestors().skip(1).all(safe_parent).then_some(real)
}

/// `p` with symlinks resolved. The part that does not exist yet (a file about to be created, in a folder that is
/// about to be created) is appended to its nearest existing ancestor. A dangling symlink is followed to where it
/// would create the file.
pub fn real_path(p: &Path) -> PathBuf {
    real_path_at(p, 0)
}

fn real_path_at(p: &Path, depth: u32) -> PathBuf {
    let p = normalize(p);
    let mut rest: Vec<&std::ffi::OsStr> = vec![];
    let mut cur: &Path = &p;
    loop {
        let base = match std::fs::canonicalize(cur) {
            Ok(real) => Some(real),
            Err(_) if depth < 40 => std::fs::read_link(cur).ok().map(|t| real_path_at(&cur.parent().unwrap_or(Path::new("/")).join(t), depth + 1)),
            Err(_) => None,
        };
        if let Some(real) = base {
            return rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                cur = parent;
            }
            _ => return p,
        }
    }
}

/// Lexical normalization (`.`/`..`), no symlink resolution. Use `real_path` to compare locations.
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
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Every built-in tool. `subagents` lists `(id, description)` of the agents the subagent tool may start.
pub fn all(subagents: Vec<(String, String)>, skills: Vec<crate::skills::Skill>, search: Option<websearch::Settings>) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(read::Read),
        Box::new(write::Write),
        Box::new(edit::Edit),
        Box::new(patch::Patch),
        Box::new(glob::Glob),
        Box::new(grep::Grep),
        Box::new(shell::Shell),
        Box::new(jobs::Jobs),
        Box::new(webfetch::WebFetch),
        Box::new(todowrite::TodoWrite),
        Box::new(question::AskQuestion),
        Box::new(subagent::Subagent { agents: subagents }),
    ];
    if let Some(settings) = search {
        tools.insert(9, Box::new(websearch::WebSearch { settings }));
    }
    // Only offered when there is something to load: an empty skill tool is noise in every request.
    if !skills.is_empty() {
        tools.push(Box::new(skill::SkillTool { skills }));
    }
    tools
}

// ---- input helpers -------------------------------------------------------------------------

/// Claude's own coding tools call these arguments `file_path`, `old_string`, `new_string` and `replace_all`, and models
/// trained on them sometimes use those names here. They are accepted as aliases of ours: each is renamed when our name
/// is not already present, wherever the call is first seen (the engine), so the UI, the stored history and the tool all
/// see one spelling. The schema we advertise does not change, so requests stay byte-stable for prompt caching.
pub fn normalize_input(tool: &str, mut input: Value) -> Value {
    fn rename(v: &mut Value, aliases: &[(&str, &str)]) {
        let Some(obj) = v.as_object_mut() else { return };
        for (alias, ours) in aliases {
            if !obj.contains_key(*ours) {
                if let Some(val) = obj.remove(*alias) {
                    obj.insert((*ours).to_string(), val);
                }
            }
        }
    }
    const REPLACEMENT: [(&str, &str); 3] = [("old_string", "oldString"), ("new_string", "newString"), ("replace_all", "replaceAll")];
    match tool {
        "read" | "write" => rename(&mut input, &[("file_path", "path")]),
        "edit" => {
            rename(&mut input, &[("file_path", "path"), REPLACEMENT[0], REPLACEMENT[1], REPLACEMENT[2]]);
            if let Some(list) = input.get_mut("edits").and_then(Value::as_array_mut) {
                for item in list {
                    rename(item, &REPLACEMENT);
                }
            }
        }
        _ => {}
    }
    input
}

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
mod alias_tests {
    use super::*;

    #[test]
    fn claude_code_argument_names_become_ours() {
        let got = normalize_input("edit", json!({"file_path":"a.rs","old_string":"x","new_string":"y","replace_all":true}));
        assert_eq!(got, json!({"path":"a.rs","oldString":"x","newString":"y","replaceAll":true}));
        assert_eq!(normalize_input("write", json!({"file_path":"a","content":"c"})), json!({"path":"a","content":"c"}));
        assert_eq!(normalize_input("read", json!({"file_path":"a","offset":3,"limit":5})), json!({"path":"a","offset":3,"limit":5}));
        // Inside `edits`, too.
        let got = normalize_input("edit", json!({"file_path":"a","edits":[{"old_string":"x","new_string":"y"},{"oldString":"p","new_string":"q","replace_all":false}]}));
        assert_eq!(got, json!({"path":"a","edits":[{"oldString":"x","newString":"y"},{"oldString":"p","newString":"q","replaceAll":false}]}));
    }

    #[test]
    fn our_names_win_and_other_tools_and_shapes_are_left_alone() {
        // Both spellings: ours is kept, the alias stays where it was.
        let got = normalize_input("edit", json!({"path":"ours","file_path":"theirs","oldString":"x","newString":"y"}));
        assert_eq!(got["path"], "ours");
        assert_eq!(got["file_path"], "theirs");
        // Tools with their own meaning for the names are not touched.
        let same = json!({"file_path":"a","old_string":"b"});
        assert_eq!(normalize_input("shell", same.clone()), same);
        assert_eq!(normalize_input("grep", json!({"pattern":"x","path":"."})), json!({"pattern":"x","path":"."}));
        // Not an object, or a bad `edits`: passed through for the tool to reject.
        assert_eq!(normalize_input("edit", json!("text")), json!("text"));
        assert_eq!(normalize_input("edit", json!({"edits":"x","file_path":"a"})), json!({"edits":"x","path":"a"}));
        assert_eq!(normalize_input("edit", json!({"edits":[1,"two"],"file_path":"a"})), json!({"edits":[1,"two"],"path":"a"}));
    }
}

#[cfg(test)]
mod project_root_tests {
    use super::*;

    fn tree() -> PathBuf {
        let r = std::env::temp_dir().join(crate::id::new_id("moco-root"));
        std::fs::create_dir_all(r.join("home/repo/src")).unwrap();
        std::fs::create_dir_all(r.join("home/scratch/deep")).unwrap();
        std::fs::create_dir_all(r.join("home/repo/.git")).unwrap();
        r
    }

    fn root(dir: &Path, home: &Path) -> PathBuf {
        project_root_in(dir, Some(home), &owned_by_user)
    }

    #[test]
    fn a_repo_below_home_is_the_root_of_everything_in_it() {
        let r = tree();
        let home = r.join("home");
        assert_eq!(root(&home.join("repo/src"), &home), home.join("repo"));
        assert_eq!(root(&home.join("repo"), &home), home.join("repo"));
        // A `.git` file (a worktree or submodule) counts as well.
        std::fs::create_dir_all(home.join("repo/wt/x")).unwrap();
        std::fs::write(home.join("repo/wt/.git"), "gitdir: ../.git/worktrees/wt").unwrap();
        assert_eq!(root(&home.join("repo/wt/x"), &home), home.join("repo/wt"));
        // Without a home directory the nearest `.git` still wins.
        assert_eq!(project_root_in(&home.join("repo/src"), None, &owned_by_user), home.join("repo"));
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn a_dotfiles_repo_in_home_or_above_does_not_widen_the_root() {
        let r = tree();
        let home = r.join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert_eq!(root(&home.join("scratch/deep"), &home), home.join("scratch/deep"));
        assert_eq!(root(&home, &home), home);
        // A repo inside home is still found first.
        assert_eq!(root(&home.join("repo/src"), &home), home.join("repo"));
        // A `.git` above home (`/Users/.git`) is no better.
        std::fs::remove_dir_all(home.join(".git")).unwrap();
        std::fs::create_dir_all(r.join(".git")).unwrap();
        assert_eq!(root(&home.join("scratch"), &home), home.join("scratch"));
        // The home is compared by its real location, whatever spelling `$HOME` uses.
        std::os::unix::fs::symlink(&home, r.join("homelink")).unwrap();
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert_eq!(root(&home.join("scratch"), &r.join("homelink")), home.join("scratch"));
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn a_git_outside_home_still_counts() {
        let r = tree();
        std::fs::create_dir_all(r.join("elsewhere/.git")).unwrap();
        std::fs::create_dir_all(r.join("elsewhere/a")).unwrap();
        assert_eq!(root(&r.join("elsewhere/a"), &r.join("home")), r.join("elsewhere"));
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn a_git_someone_else_owns_is_skipped() {
        let r = tree();
        let home = r.join("home");
        // As if another user had planted `<tmp>/.git`: the search goes on past it and falls back to the directory.
        std::fs::create_dir_all(r.join(".git")).unwrap();
        let planted = r.join(".git");
        let not_planted = |p: &Path| p != planted;
        assert_eq!(project_root_in(&r.join("home/scratch"), None, &not_planted), r.join("home/scratch"));
        assert_eq!(project_root_in(&home.join("repo/src"), None, &not_planted), home.join("repo"));
        // Our own files pass the real check.
        assert!(owned_by_user(&home.join("repo/.git")));
        assert!(!owned_by_user(&r.join("missing")));
        let _ = std::fs::remove_dir_all(r);
    }
}

#[cfg(test)]
mod symlink_tests {
    use super::testutil::*;
    use super::*;
    use std::os::unix::fs::symlink;

    /// `<env>/real/proj` reached as `<env>/link/proj` too; `<env>/outside` is elsewhere.
    fn setup(env: &Env) -> (PathBuf, PathBuf, PathBuf) {
        let real = env.dir.join("real/proj");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(env.dir.join("outside")).unwrap();
        symlink(env.dir.join("real"), env.dir.join("link")).unwrap();
        (real, env.dir.join("link/proj"), env.dir.join("outside"))
    }

    fn ctx_in<'a>(env: &'a Env, cwd: &Path) -> ToolCtx<'a> {
        ToolCtx { cwd: cwd.to_path_buf(), project_root: cwd.to_path_buf(), ..env.ctx() }
    }

    fn write(ctx: &ToolCtx, path: &Path) {
        write::Write.run(&json!({"path": path.to_string_lossy(), "content": "x"}), ctx).unwrap();
    }

    fn external(env: &Env) -> Vec<String> {
        env.perm.asked().into_iter().filter(|r| r.action == "external_directory").flat_map(|r| r.resources).collect()
    }

    #[test]
    fn both_spellings_of_the_project_are_internal() {
        let env = Env::new();
        let (real, link, _) = setup(&env);
        for cwd in [&real, &link] {
            let ctx = ctx_in(&env, cwd);
            for base in [&real, &link] {
                assert!(ctx.is_internal(&base.join("a.txt")));
                write(&ctx, &base.join("a.txt"));
                write(&ctx, &base.join("new/deeper/b.txt"));
            }
        }
        assert!(external(&env).is_empty(), "{:?}", external(&env));
        assert_eq!(std::fs::read_to_string(real.join("new/deeper/b.txt")).unwrap(), "x");
    }

    #[test]
    fn display_keeps_the_spelling_given() {
        let env = Env::new();
        let (real, link, _) = setup(&env);
        let ctx = ctx_in(&env, &link);
        assert_eq!(ctx.display(&real.join("src/a.rs")), "src/a.rs");
        assert_eq!(ctx.display(&link.join("src/a.rs")), "src/a.rs");
        assert_eq!(ctx.display(&env.dir.join("outside/o.txt")), env.dir.join("outside/o.txt").to_string_lossy());
    }

    #[test]
    fn a_link_inside_the_project_that_leads_out_is_external() {
        let env = Env::new();
        let (real, _, outside) = setup(&env);
        let ctx = ctx_in(&env, &real);
        symlink(&outside, real.join("dirlink")).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        symlink(outside.join("secret.txt"), real.join("filelink")).unwrap();
        symlink(outside.join("not-yet.txt"), real.join("dangling")).unwrap();

        assert!(!ctx.is_internal(&real.join("dirlink/x.txt")));
        for target in ["dirlink/x.txt", "dirlink/sub/x.txt", "filelink", "dangling"] {
            let before = external(&env).len();
            write(&ctx, &real.join(target));
            let asked = external(&env);
            assert_eq!(asked.len(), before + 1, "{target}");
            assert!(asked.last().unwrap().starts_with(&format!("{}/", std::fs::canonicalize(&outside).unwrap().display())), "{target}: {asked:?}");
        }
    }

    #[test]
    fn a_new_file_resolves_through_its_nearest_existing_ancestor() {
        let env = Env::new();
        let (real, link, _) = setup(&env);
        let want = std::fs::canonicalize(&real).unwrap().join("a/b/c.txt");
        assert_eq!(real_path(&link.join("a/b/c.txt")), want);
        assert_eq!(real_path(&link.join("a/./b/../b/c.txt")), want);
    }

    #[test]
    fn the_always_rule_is_the_same_for_both_spellings() {
        let env = Env::new();
        let (real, link, outside) = setup(&env);
        symlink(&outside, env.dir.join("outlink")).unwrap();
        let ctx = ctx_in(&env, &real);
        write(&ctx, &outside.join("a.txt"));
        write(&ctx, &env.dir.join("outlink/a.txt"));
        let asked: Vec<PermReq> = env.perm.asked().into_iter().filter(|r| r.action == "external_directory").collect();
        assert_eq!(asked.len(), 2);
        assert_eq!((&asked[0].resources, &asked[0].save), (&asked[1].resources, &asked[1].save));
        let _ = link;
    }

    fn read_resources(env: &Env) -> Vec<Vec<String>> {
        env.perm.asked().into_iter().filter(|r| r.action == "read").map(|r| r.resources).collect()
    }

    #[test]
    fn a_link_to_a_file_in_the_project_is_authorized_as_its_target() {
        let env = Env::new();
        env.file(".env", b"SECRET=1\n");
        symlink(env.work().join(".env"), env.work().join("safe.txt")).unwrap();
        let o = read::Read.run(&json!({"path":"safe.txt"}), &env.ctx()).unwrap();
        assert_eq!(o.text, "1: SECRET=1");
        assert_eq!(read_resources(&env), [["safe.txt", ".env"]], "both names are checked");
    }

    #[test]
    fn a_linked_parent_is_authorized_as_its_target() {
        let env = Env::new();
        env.file("secrets/.env", b"S=1\n");
        symlink(env.work().join("secrets"), env.work().join("docs")).unwrap();
        read::Read.run(&json!({"path":"docs/.env"}), &env.ctx()).unwrap();
        assert_eq!(read_resources(&env), [["docs/.env", "secrets/.env"]]);
    }

    #[test]
    fn a_link_out_of_the_project_asks_for_the_external_target_too() {
        let env = Env::new();
        let outside = env.dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("o.txt"), "o").unwrap();
        symlink(outside.join("o.txt"), env.work().join("l.txt")).unwrap();
        read::Read.run(&json!({"path":"l.txt"}), &env.ctx()).unwrap();
        let asked = env.perm.asked();
        assert_eq!(asked[0].action, "external_directory");
        assert_eq!(read_resources(&env)[0], ["l.txt".to_string(), real_path(&outside.join("o.txt")).display().to_string()]);
    }

    #[test]
    fn a_linked_parent_out_of_the_project_keeps_its_relative_name() {
        let env = Env::new();
        let outside = env.dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("o.txt"), "o").unwrap();
        symlink(&outside, env.work().join("ext")).unwrap();
        let target = real_path(&outside.join("o.txt")).display().to_string();
        read::Read.run(&json!({"path":"ext/o.txt"}), &env.ctx()).unwrap();
        // An absolute spelling of the same link is shown by the same relative name.
        read::Read.run(&json!({"path": env.work().join("ext/o.txt").display().to_string()}), &env.ctx()).unwrap();
        assert_eq!(read_resources(&env), [["ext/o.txt".to_string(), target.clone()], ["ext/o.txt".to_string(), target]]);
    }

    #[test]
    fn edit_write_and_patch_authorize_and_use_the_resolved_target() {
        let env = Env::new();
        env.file(".env", b"A=1\n");
        symlink(env.work().join(".env"), env.work().join("safe.txt")).unwrap();
        let edit_resources = |env: &Env| env.perm.asked().into_iter().filter(|r| r.action == "edit").map(|r| r.resources).collect::<Vec<_>>();
        edit::Edit.run(&json!({"path":"safe.txt","oldString":"A=1","newString":"A=2"}), &env.ctx()).unwrap();
        write::Write.run(&json!({"path":"safe.txt","content":"A=3\n"}), &env.ctx()).unwrap();
        patch::Patch.run(&json!({"patchText":"*** Begin Patch\n*** Update File: safe.txt\n@@\n-A=3\n+A=4\n*** End Patch"}), &env.ctx()).unwrap();
        assert_eq!(edit_resources(&env), [["safe.txt", ".env"]; 3]);
        assert!(std::fs::symlink_metadata(env.work().join("safe.txt")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(env.work().join(".env")).unwrap(), "A=4\n");
    }

    #[test]
    fn grep_and_glob_on_a_linked_root_authorize_its_target() {
        let env = Env::new();
        env.file(".env", b"needle\n");
        env.file("secrets/a.txt", b"needle\n");
        symlink(env.work().join(".env"), env.work().join("safe.txt")).unwrap();
        symlink(env.work().join("secrets"), env.work().join("docs")).unwrap();
        let t = grep::Grep.run(&json!({"pattern":"needle","path":"safe.txt"}), &env.ctx()).unwrap().text;
        assert!(t.contains("needle"), "{t}");
        assert_eq!(read_resources(&env), [["safe.txt", ".env"]]);
        let t = glob::Glob.run(&json!({"pattern":"*.txt","path":"docs"}), &env.ctx()).unwrap().text;
        assert_eq!(t, "a.txt");
        assert_eq!(read_resources(&env)[1], ["docs", "secrets"]);
    }

    #[test]
    fn a_plain_path_adds_no_extra_resource() {
        let env = Env::new();
        env.file("a.txt", b"x");
        read::Read.run(&json!({"path":"a.txt"}), &env.ctx()).unwrap();
        assert_eq!(read_resources(&env), [["a.txt"]]);
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct TestPermitter {
        pub asked: Mutex<Vec<PermReq>>,
        pub reply: Mutex<Result<(), ToolError>>,
    }

    pub struct ScopedEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        vars: Vec<(String, Option<std::ffi::OsString>)>,
    }
    impl ScopedEnv {
        pub fn set(vars: &[(&str, &str)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let vars = vars
                .iter()
                .map(|(name, value)| {
                    let previous = std::env::var_os(name);
                    std::env::set_var(name, value);
                    ((*name).to_string(), previous)
                })
                .collect();
            ScopedEnv { _lock: lock, vars }
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (name, value) in self.vars.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
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
        /// Allows without recording when every request is allowed, otherwise asks.
        fn evaluate(&self, _req: &PermReq) -> crate::permission::Effect {
            if self.reply.lock().unwrap().is_ok() { crate::permission::Effect::Allow } else { crate::permission::Effect::Ask }
        }
    }

    #[derive(Default)]
    pub struct TestHost {
        pub todos: Mutex<Vec<Todo>>,
        pub asked: Mutex<Vec<Vec<Question>>>,
        pub answers: Mutex<Option<Vec<Vec<String>>>>,
        pub subagents: Mutex<Vec<SubagentRequest>>,
        pub shells: Mutex<Vec<(String, PathBuf, Option<std::time::Duration>)>>,
        pub jobs: Mutex<Vec<JobInfo>>,
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
        fn provider_key_env_vars(&self) -> Vec<String> {
            crate::catalog::Catalog::embedded().key_env_vars()
        }
        fn start_background_shell(&self, command: &str, workdir: &Path, timeout: Option<std::time::Duration>, _unsandboxed: bool) -> Result<(String, PathBuf), ToolError> {
            self.shells.lock().unwrap().push((command.to_string(), workdir.to_path_buf(), timeout));
            Ok(("shl_test".into(), PathBuf::from("/tmp/shl_test.txt")))
        }
        fn start_background_subagent(&self, req: SubagentRequest) -> Result<String, ToolError> {
            self.subagents.lock().unwrap().push(req);
            Ok("ses_bg".into())
        }
        fn jobs(&self) -> Vec<JobInfo> {
            self.jobs.lock().unwrap().clone()
        }
        fn job_output(&self, id: &str, lines: usize) -> Result<String, ToolError> {
            Ok(format!("output of {id} ({lines} lines)"))
        }
        fn kill_job(&self, id: &str) -> Result<String, ToolError> {
            Ok(format!("stopped {id}"))
        }
    }

    pub struct Env {
        pub dir: PathBuf,
        pub perm: TestPermitter,
        pub cancel: AtomicBool,
        pub host: TestHost,
        pub formatters: crate::format::Formatters,
    }
    impl Env {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(crate::id::new_id("moco-tool"));
            std::fs::create_dir_all(dir.join("work")).unwrap();
            Env { dir, perm: TestPermitter::allow(), cancel: AtomicBool::new(false), host: TestHost::default(), formatters: Default::default() }
        }
        pub fn work(&self) -> PathBuf {
            self.dir.join("work")
        }
        pub fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx { formatters: &self.formatters, media: MediaCaps::default(), session_id: "ses_test", host: &self.host, cwd: self.work(), project_root: self.work(), spill_dir: self.dir.join("spill"), permitter: &self.perm, cancel: &self.cancel, call_id: "call_test" }
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

/// archive#67: the user's own temp directory needs no `external_directory` prompt; the shared `/tmp` still asks.
#[cfg(test)]
mod temp_dir_tests {
    use super::grep::tests::Rules;
    use super::testutil::*;
    use super::*;
    use crate::permission::PermissionMode;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn mkdir(p: &Path, mode: u32) -> PathBuf {
        std::fs::create_dir_all(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.to_path_buf()
    }

    /// An `Env` whose directory only the user can change (with umask 002 it would be group-writable, and so no
    /// place for a private temp dir).
    fn env() -> Env {
        let env = Env::new();
        mkdir(&env.dir, 0o755);
        env
    }

    /// A mode's rules (and the careful preset in auto mode), with the temp dir rules where `resolve_agents_in` puts them.
    fn rules(mode: PermissionMode) -> Rules {
        let mut extra = if mode == PermissionMode::Auto { crate::permission::preset_rules("careful").unwrap() } else { vec![] };
        extra.extend(temp_dir_rules());
        Rules::new(mode, extra)
    }

    /// Sets this thread's temp directory (instead of `TMPDIR`) until dropped.
    struct TempDir;
    impl TempDir {
        fn set(p: &Path) -> TempDir {
            TEST_TEMP_DIR.with(|t| *t.borrow_mut() = Some(p.to_path_buf()));
            TempDir
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            TEST_TEMP_DIR.with(|t| *t.borrow_mut() = None);
        }
    }

    fn read(ctx: &ToolCtx, p: &Path) -> Result<ToolOutput, ToolError> {
        read::Read.run(&json!({"path": p.to_string_lossy()}), ctx)
    }

    #[test]
    fn only_a_private_temp_dir_of_the_users_counts() {
        let env = env();
        let tmp = mkdir(&env.dir.join("T"), 0o700);
        let real = std::fs::canonicalize(&tmp).unwrap();
        assert_eq!(private_temp_dir(&tmp, None, me()), Some(real.clone()));
        symlink(&tmp, env.dir.join("T-link")).unwrap();
        assert_eq!(private_temp_dir(&env.dir.join("T-link"), None, me()), Some(real.clone()), "compared with links resolved");
        assert_eq!(private_temp_dir(&tmp, None, me() + 1), None, "someone else's");
        assert_eq!(private_temp_dir(&tmp, Some(&tmp), me()), None, "the home directory");
        assert_eq!(private_temp_dir(&env.dir, Some(&tmp), me()), None, "above the home directory");
        assert_eq!(private_temp_dir(Path::new("/"), None, me()), None);
        assert_eq!(private_temp_dir(Path::new("T"), None, me()), None, "relative");
        assert_eq!(private_temp_dir(&env.dir.join("missing"), None, me()), None);
        for mode in [0o777, 0o770, 0o1777, 0o720] {
            let d = mkdir(&env.dir.join(format!("m{mode:o}")), mode);
            assert_eq!(private_temp_dir(&d, None, me()), None, "{mode:o}: others can write to it");
        }
        let wild = mkdir(&env.dir.join("a*b"), 0o700);
        assert_eq!(private_temp_dir(&wild, None, me()), None, "a wildcard in the pattern");
        // A private folder below a parent that others can change is not private.
        let open = mkdir(&env.dir.join("open"), 0o777);
        let below = mkdir(&open.join("T"), 0o700);
        assert_eq!(private_temp_dir(&below, None, me()), None);
        // The shared /tmp (/private/tmp on macOS) is world-writable.
        assert_eq!(private_temp_dir(Path::new("/tmp"), None, me()), None);
    }

    #[test]
    fn the_users_temp_dir_needs_no_external_directory_prompt() {
        let env = env();
        let tmp = mkdir(&env.dir.join("T"), 0o700);
        std::fs::create_dir_all(tmp.join("scratch/sub")).unwrap();
        std::fs::write(tmp.join("scratch/sub/a.txt"), "a").unwrap();
        symlink(&tmp, env.dir.join("T-link")).unwrap();
        std::fs::create_dir_all(env.dir.join("outside")).unwrap();
        std::fs::write(env.dir.join("outside/o.txt"), "o").unwrap();
        let _t = TempDir::set(&tmp);
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            let perm = rules(mode);
            let ctx = ToolCtx { permitter: &perm, ..env.ctx() };
            assert_eq!(read(&ctx, &tmp.join("scratch/sub/a.txt")).unwrap().text, "1: a");
            assert_eq!(read(&ctx, &env.dir.join("T-link/scratch/sub/a.txt")).unwrap().text, "1: a");
            ctx.check_external(&tmp).unwrap();
            ctx.check_external(&tmp.join("new/dir")).unwrap();
            assert!(perm.prompts().is_empty(), "{mode:?}: {:?}", perm.prompts());
            assert_eq!(read(&ctx, &env.dir.join("outside/o.txt")), Err(ToolError::Declined));
            assert_eq!(perm.prompts(), [format!("external_directory {}/*", real_path(&env.dir.join("outside")).display())]);
        }
    }

    #[test]
    fn a_link_from_the_temp_dir_to_elsewhere_still_asks() {
        let env = env();
        let tmp = mkdir(&env.dir.join("T"), 0o700);
        let outside = env.dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        symlink(&outside, tmp.join("dirlink")).unwrap();
        symlink(outside.join("secret.txt"), tmp.join("filelink")).unwrap();
        let _t = TempDir::set(&tmp);
        let perm = rules(PermissionMode::Manual);
        let ctx = ToolCtx { permitter: &perm, ..env.ctx() };
        let want = format!("external_directory {}/*", real_path(&outside).display());
        for p in ["dirlink/secret.txt", "filelink"] {
            assert_eq!(read(&ctx, &tmp.join(p)), Err(ToolError::Declined), "{p}");
            assert_eq!(perm.prompts().last(), Some(&want), "{p}");
        }
        assert_eq!(glob::Glob.run(&json!({"pattern": "*", "path": tmp.join("dirlink").to_string_lossy()}), &ctx), Err(ToolError::Declined));
        assert_eq!(perm.prompts().last(), Some(&want));
        assert_eq!(perm.prompts().len(), 3);
    }

    #[test]
    fn a_shared_temp_dir_asks_and_always_covers_the_users_scratch_folder() {
        let env = env();
        let shared = mkdir(&env.dir.join("tmp"), 0o1777);
        let scratch = mkdir(&shared.join("moco_head"), 0o755);
        let sub = scratch.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.rs"), "a").unwrap();
        let ctx = env.ctx();
        let pattern = |p: &Path| format!("{}/*", real_path(p).display());
        read(&ctx, &sub.join("a.rs")).unwrap();
        let asked = env.perm.asked();
        assert_eq!(asked[0].action, "external_directory", "a shared temp dir is not exempt");
        assert_eq!(asked[0].resources, [pattern(&sub)]);
        assert_eq!(asked[0].save, [pattern(&scratch)], "\"always\" covers the whole scratch folder");
        assert!(crate::wildcard::matches(&pattern(&scratch.join("other")), &asked[0].save[0]), "a sibling folder is covered");
        assert!(!crate::wildcard::matches(&pattern(&shared.join("moco_other")), &asked[0].save[0]), "another scratch folder is not");

        let save = |dir: &Path| {
            ctx.check_external(dir).unwrap();
            env.perm.asked().last().unwrap().save.clone()
        };
        assert_eq!(save(&scratch), [pattern(&scratch)]);
        // The shared dir itself is not one of the user's scratch folders in it: it keeps its own pattern, unless the
        // test's own folder is one in the system's shared /tmp (Linux; on macOS it is under the private $TMPDIR).
        use std::os::unix::fs::MetadataExt;
        let in_shared_tmp = real_path(&env.dir).parent().and_then(|p| std::fs::metadata(p).ok()).is_some_and(|m| m.mode() & 0o1002 == 0o1002);
        assert_eq!(save(&shared), [pattern(if in_shared_tmp { &env.dir } else { &shared })]);
        // A folder others can write to is not widened, nor is one somebody else owns.
        let open = mkdir(&shared.join("open"), 0o777);
        std::fs::create_dir_all(open.join("x")).unwrap();
        assert_eq!(save(&open.join("x")), [pattern(&open.join("x"))]);
        assert_eq!(scratch_folder_of(&real_path(&sub), me() + 1), None);
        // A project in it keeps its project root.
        let repo = scratch.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(save(&repo.join("src")), [pattern(&repo)]);
    }

    #[test]
    fn temp_dir_rules_cover_the_temp_dir_but_not_moco_data_or_its_neighbours() {
        use crate::permission::{evaluate, Effect};
        let t = Path::new("/nonexistent-moco/folders/ab/cd/T");
        assert!(temp_dir_rules_for(None, t).is_empty(), "no private temp dir: no rule");
        let rules = temp_dir_rules_for(Some(t), &t.join("moco-home"));
        for r in ["/nonexistent-moco/folders/ab/cd/T/*", "/nonexistent-moco/folders/ab/cd/T/x/y/*"] {
            assert_eq!(evaluate("external_directory", r, &rules), Effect::Allow, "{r}");
        }
        for r in ["/nonexistent-moco/folders/ab/cd/T/moco-home/*", "/nonexistent-moco/folders/ab/cd/T/moco-home/sessions/*", "/nonexistent-moco/folders/ab/cd/T-x/*", "/nonexistent-moco/folders/ab/cd/C/*", "/private/tmp/x/*"] {
            assert_eq!(evaluate("external_directory", r, &rules), Effect::Ask, "{r}");
        }
        assert_eq!(evaluate("read", "/nonexistent-moco/folders/ab/cd/T/a", &rules), Effect::Ask, "only external_directory");
    }

    /// The temp dir rules come after the mode's and the preset's asks and before the explore agent's rules and the
    /// user's `permissions`, so build skips the prompt there, explore still asks, and the user can take it back.
    #[test]
    fn the_temp_dir_rule_sits_between_the_defaults_and_what_must_win() {
        use crate::permission::{evaluate, Effect, Rule};
        let env = env();
        let tmp = mkdir(&env.dir.join("T"), 0o700);
        let _t = TempDir::set(&tmp);
        let inside = format!("{}/scratch/*", real_path(&tmp).display());
        for preset in [None, Some("careful"), Some("trusted")] {
            for mode in [PermissionMode::Manual, PermissionMode::Auto] {
                for user in [vec![], vec![Rule::new("external_directory", &inside, Effect::Deny)]] {
                    let cfg: crate::config::Config = serde_json::from_value(json!({ "permission_preset": preset, "permissions": user })).unwrap();
                    let agents = crate::config::resolve_agents_in(&cfg, mode);
                    let perms = |id: &str| agents.iter().find(|a| a.id == id).unwrap().permissions.clone();
                    let at = format!("{mode:?} {preset:?} user rules {}", user.len());
                    let (want, explore) = if user.is_empty() { (Effect::Allow, Effect::Ask) } else { (Effect::Deny, Effect::Deny) };
                    for id in ["build", "plan", "general"] {
                        assert_eq!(evaluate("external_directory", &inside, &perms(id)), want, "{id} {at}");
                    }
                    assert_eq!(evaluate("external_directory", &inside, &perms("explore")), explore, "explore {at}");
                    if (mode, preset) != (PermissionMode::Auto, Some("trusted")) {
                        for shared in ["/private/tmp/x/*", "/tmp/x/*"] {
                            assert_eq!(evaluate("external_directory", shared, &perms("build")), Effect::Ask, "{shared} {at}");
                        }
                    }
                }
            }
        }
    }
}
