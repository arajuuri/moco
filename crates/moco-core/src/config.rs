//! Config loading (docs/08-config-and-extensions.md): `moco.json[c]` (JSONC) files merged from
//! lowest to highest priority, `{env:NAME}` / `{file:path}` substitution on the raw text.
//!
//! Locations, lowest first: `<app dir>/config/moco.json[c]`; then for every ancestor of the working
//! directory, outermost first, `<dir>/moco.json[c]` and `<dir>/.moco/moco.json[c]`; then `$MOCO_CONFIG`.
//! Objects merge by key; `permissions` and `instructions` arrays concatenate (lowest priority first),
//! every other value is replaced.
use crate::agent::{self, Agent};
use crate::permission::Rule;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub model: Option<String>,
    pub system: Option<String>,
    pub description: Option<String>,
    pub steps: Option<u32>,
    /// `"primary"`, `"subagent"` or `"all"` (default for agents defined in config).
    pub mode: Option<String>,
    pub permissions: Vec<Rule>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub name: Option<String>,
    pub limit: Option<crate::catalog::Limit>,
    pub cost: Option<crate::catalog::Cost>,
}

/// `providers.<id>`: adds a provider (any OpenAI-compatible server, e.g. Ollama or LM Studio) or
/// overrides fields of a catalog provider.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub base_url: Option<String>,
    /// Environment variables to read the API key from.
    pub env: Vec<String>,
    /// `"openai-chat"` (default for new providers) or `"anthropic"`.
    pub protocol: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub models: BTreeMap<String, ModelConfig>,
    pub disabled: bool,
    /// Set `false` to stop sending `cache_control` markers (Anthropic protocol only; default on).
    pub prompt_cache: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    /// Free space to keep: a token count (`30000`) or a percentage of the context window (`"15%"`).
    pub buffer: Option<serde_json::Value>,
    /// Tokens of recent conversation kept verbatim.
    pub keep: Option<u64>,
}

/// `formatter.<name>`: a command run on a file after the model writes, edits or patches it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct FormatterConfig {
    /// Program and arguments; `$FILE` is replaced by the file's path (appended as the last argument when absent).
    pub command: Vec<String>,
    /// File extensions it handles, with or without the dot (`".rs"`, `"rs"`), case-insensitive.
    pub extensions: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub disabled: bool,
}

/// `formatter` is either `false` (no formatting at all) or a map of formatters by name.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FormatterSetting {
    Flag(bool),
    Formatters(BTreeMap<String, FormatterConfig>),
}

/// `mcp.servers.<name>.oauth` for a remote server: how to sign in. All fields are optional; a server that supports
/// dynamic client registration needs none of them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct OAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    /// Port of the local callback listener (default: any free port).
    pub callback_port: Option<u16>,
    /// Redirect URI to register and use instead of `http://127.0.0.1:<port>/callback`.
    pub redirect_uri: Option<String>,
    /// Where to fetch the authorization server metadata, when it cannot be discovered.
    pub auth_server_metadata_url: Option<String>,
}

/// `oauth` is `false` (never try OAuth for this server) or an object.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OAuthSetting {
    Flag(bool),
    Config(OAuthConfig),
}

impl OAuthSetting {
    /// `None` when OAuth is switched off.
    pub fn resolve(setting: &Option<OAuthSetting>) -> Option<OAuthConfig> {
        match setting {
            Some(OAuthSetting::Flag(false)) => None,
            Some(OAuthSetting::Config(c)) => Some(c.clone()),
            _ => Some(OAuthConfig::default()),
        }
    }
}

/// `mcp.servers.<name>`: a Model Context Protocol server. `local` servers are child processes speaking over stdio.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServerConfig {
    /// `"local"` (default when `command` is set) or `"remote"`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Program and arguments.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub disabled: bool,
    /// Per-call timeout for this server, in milliseconds.
    pub timeout: Option<u64>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub oauth: Option<OAuthSetting>,
}

impl McpServerConfig {
    pub fn is_remote(&self) -> bool {
        self.kind.as_deref() == Some("remote") || (self.kind.is_none() && self.command.is_empty() && self.url.is_some())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpTimeouts {
    /// Starting the server and the initialize handshake (ms, default 30 000).
    pub startup: Option<u64>,
    /// `tools/list`, `prompts/list`, `resources/list` (ms, default 30 000).
    pub catalog: Option<u64>,
    /// A tool or prompt call (ms, default 300 000).
    pub execution: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub timeout: McpTimeouts,
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// `commands.<name>`: a prompt template reachable as `/name`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CommandConfig {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Run in a child session (a subagent) instead of the current conversation.
    pub subagent: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub formatter: Option<FormatterSetting>,
    pub mcp: McpConfig,
    pub tui: TuiConfig,
    pub websearch: Option<WebSearchSetting>,
    /// Extra skill locations: a directory of skills, or a single skill directory.
    pub skills: Vec<String>,
    pub commands: BTreeMap<String, CommandConfig>,
    pub compaction: CompactionConfig,
    /// `false` turns off the per-step working-tree snapshots (and with them `/undo` and `/diff`). Default on.
    pub snapshots: Option<bool>,
    /// `provider/model` for small background jobs such as titles (default: the session's model).
    pub small_model: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
    /// `provider/model`
    pub model: Option<String>,
    pub default_agent: Option<String>,
    pub shell: Option<String>,
    pub permissions: Vec<Rule>,
    pub agents: BTreeMap<String, AgentConfig>,
    /// Extra instruction files (paths, `~/` allowed; relative paths resolve against the working directory).
    pub instructions: Vec<String>,
}

/// `websearch`: which service the `websearch` tool uses.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebSearchConfig {
    /// `duckduckgo` (default, no key), `brave`, `tavily`, `exa` or `searxng`.
    pub provider: Option<String>,
    /// Endpoint override; required for `searxng`.
    pub url: Option<String>,
    pub max_results: Option<u32>,
    /// Environment variables to read the API key from, besides `<PROVIDER>_API_KEY` and `auth.json`.
    pub key_env: Vec<String>,
}

/// `"websearch": false` switches the tool off; an object configures it.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WebSearchSetting {
    Flag(bool),
    Config(WebSearchConfig),
}

impl WebSearchSetting {
    /// `None` when switched off; the defaults when not mentioned at all.
    pub fn resolve(s: &Option<WebSearchSetting>) -> Option<WebSearchConfig> {
        match s {
            Some(WebSearchSetting::Flag(false)) => None,
            Some(WebSearchSetting::Config(c)) => Some(c.clone()),
            _ => Some(WebSearchConfig::default()),
        }
    }
}

/// `tui`: settings of the full-screen UI.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// `auto` (default), `dark`, `light`, `mono`, or the name of a theme file.
    pub theme: Option<String>,
    /// Rebind actions: `{"palette": "ctrl+k", "quit": ["ctrl+d", "ctrl+q"]}`. Values are one key or a list.
    pub keys: BTreeMap<String, Value>,
    /// What `leader` followed by a key does: `{"n": "/new", "b": "sidebar"}`.
    pub leader: BTreeMap<String, String>,
    /// Ring the terminal bell (and mark the window title) when a turn ends or input is needed while you are away. Default on.
    pub bell: Option<bool>,
    /// Seconds without a key press after which you count as away when the terminal reports no focus. Default 8.
    pub away_after: Option<u64>,
    /// `true`, `false` or `"auto"` (default: shown when the terminal is wide enough).
    pub sidebar: Option<Value>,
    /// Draw with plain ASCII instead of box-drawing and check-mark characters (also `MOCO_ASCII=1`).
    pub ascii: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub config: Config,
    pub sources: Vec<PathBuf>,
    pub warnings: Vec<String>,
    /// For each `mcp.servers` entry, the config file that defined it last (so project-supplied servers can be vetted).
    pub mcp_origins: BTreeMap<String, PathBuf>,
    /// The same for `formatter.<name>`: a project's formatter is a command to run, so it must be vetted like an MCP server.
    pub formatter_origins: BTreeMap<String, PathBuf>,
}

/// Remove `//` and `/* */` comments and trailing commas, leaving strings untouched.
pub fn strip_jsonc(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let (mut i, mut in_str) = (0, false);
    while i < cs.len() {
        let c = cs[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < cs.len() {
                out.push(cs[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == '/' && cs.get(i + 1) == Some(&'/') {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && cs.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < cs.len() && !(cs[i] == '*' && cs[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ',' {
            // Trailing comma: next significant char closes the container.
            let mut j = i + 1;
            loop {
                while j < cs.len() && cs[j].is_whitespace() {
                    j += 1;
                }
                if cs.get(j) == Some(&'/') && cs.get(j + 1) == Some(&'/') {
                    while j < cs.len() && cs[j] != '\n' {
                        j += 1;
                    }
                } else if cs.get(j) == Some(&'/') && cs.get(j + 1) == Some(&'*') {
                    j += 2;
                    while j + 1 < cs.len() && !(cs[j] == '*' && cs[j + 1] == '/') {
                        j += 1;
                    }
                    j += 2;
                } else {
                    break;
                }
            }
            if !matches!(cs.get(j), Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

fn json_escape(s: &str) -> String {
    let q = serde_json::to_string(s).unwrap_or_default();
    q[1..q.len().saturating_sub(1)].to_string()
}

/// Replace `{env:NAME}` (empty if unset) and `{file:path}` (contents, relative to `base`) in raw text.
/// Values are JSON-escaped so they are safe inside a string literal.
pub fn substitute(raw: &str, base: &Path) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find('}');
        match (end, tail.strip_prefix("{env:").or_else(|| tail.strip_prefix("{file:"))) {
            (Some(end), Some(_)) => {
                let token = &tail[1..end];
                let (kind, arg) = token.split_once(':').unwrap();
                let value = if kind == "env" {
                    std::env::var(arg.trim()).unwrap_or_default()
                } else {
                    let p = expand_home(arg.trim());
                    let p = if p.is_absolute() { p } else { base.join(p) };
                    std::fs::read_to_string(&p).map_err(|e| format!("{{file:{}}}: {e}", p.display()))?.trim_end_matches(['\n', '\r']).to_string()
                };
                out.push_str(&json_escape(&value));
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

const CONCAT_KEYS: &[&str] = &["permissions", "instructions"];

pub fn merge(base: Value, over: Value, key: &str) -> Value {
    match (base, over) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(existing) => merge(existing, v, &k),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        (Value::Array(mut b), Value::Array(o)) if CONCAT_KEYS.contains(&key) => {
            b.extend(o);
            Value::Array(b)
        }
        (_, o) => o,
    }
}

fn candidates(dir: &Path) -> Vec<PathBuf> {
    ["moco.json", "moco.jsonc"].iter().map(|n| dir.join(n)).collect()
}

/// Config files that exist, lowest priority first.
pub fn discover(app_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![app_dir.join("config")];
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for a in ancestors {
        dirs.push(a.to_path_buf());
        dirs.push(a.join(".moco"));
    }
    let mut files: Vec<PathBuf> = dirs.iter().flat_map(|d| candidates(d)).filter(|p| p.is_file()).collect();
    if let Ok(p) = std::env::var("MOCO_CONFIG") {
        files.push(PathBuf::from(p));
    }
    files
}

pub fn load(app_dir: &Path, cwd: &Path) -> Loaded {
    let mut merged = Value::Object(Default::default());
    let mut out = Loaded::default();
    for path in discover(app_dir, cwd) {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| substitute(&raw, path.parent().unwrap_or(Path::new("."))))
            .and_then(|raw| serde_json::from_str::<Value>(&strip_jsonc(&raw)).map_err(|e| e.to_string()));
        match parsed {
            Ok(v) if v.is_object() => {
                for name in v["mcp"]["servers"].as_object().into_iter().flat_map(|o| o.keys()) {
                    out.mcp_origins.insert(name.clone(), path.clone());
                }
                for name in v["formatter"].as_object().into_iter().flat_map(|o| o.keys()) {
                    out.formatter_origins.insert(name.clone(), path.clone());
                }
                if v["formatter"].is_boolean() {
                    out.formatter_origins.clear(); // `false`/`true` replaces everything defined before it
                }
                merged = merge(merged, v, "");
                out.sources.push(path);
            }
            Ok(_) => out.warnings.push(format!("{}: top level must be an object", path.display())),
            Err(e) => out.warnings.push(format!("{}: {e}", path.display())),
        }
    }
    match serde_json::from_value::<Config>(merged) {
        Ok(c) => out.config = c,
        Err(e) => out.warnings.push(format!("invalid config: {e}")),
    }
    out
}

/// Built-in agents with config overrides applied, plus agents defined only in config.
/// Rule order: agent defaults, then top-level `permissions`, then the agent's own `permissions`.
pub fn resolve_agents(cfg: &Config) -> Vec<Agent> {
    let mut agents: Vec<Agent> = agent::builtin();
    for (id, ac) in &cfg.agents {
        if let Some(a) = agents.iter_mut().find(|a| &a.id == id) {
            a.model = ac.model.clone().or(a.model.take());
            a.system = ac.system.clone().or(a.system.take());
            a.description = ac.description.clone().unwrap_or_else(|| a.description.clone());
            a.steps = ac.steps.or(a.steps);
            a.mode = ac.mode.as_deref().and_then(agent::Mode::parse).unwrap_or(a.mode);
        } else if !ac.disabled {
            agents.push(Agent {
                id: id.clone(),
                mode: ac.mode.as_deref().and_then(agent::Mode::parse).unwrap_or(agent::Mode::All),
                name: id.clone(),
                description: ac.description.clone().unwrap_or_default(),
                system: ac.system.clone(),
                steps: ac.steps,
                model: ac.model.clone(),
                permissions: crate::permission::default_rules(),
            });
        }
    }
    agents.retain(|a| !cfg.agents.get(&a.id).is_some_and(|c| c.disabled));
    for a in &mut agents {
        a.permissions.extend(cfg.permissions.iter().cloned());
        if let Some(ac) = cfg.agents.get(&a.id) {
            a.permissions.extend(ac.permissions.iter().cloned());
        }
    }
    agents
}

/// `provider/model` -> (`provider`, `model`); a bare id defaults to the anthropic provider.
pub fn split_model(s: &str) -> (&str, &str) {
    s.split_once('/').unwrap_or(("anthropic", s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::Effect;
    use serde_json::json;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id("moco-cfg"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let src = r#"{
            // line comment
            "a": "x // not a comment", /* block */
            "b": [1, 2, /* tail */ ],
            "c": {"d": "q\"uote",},
        }"#;
        let v: Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v, json!({"a":"x // not a comment","b":[1,2],"c":{"d":"q\"uote"}}));
    }

    #[test]
    fn variables_env_file_and_escaping() {
        let dir = tmp();
        std::fs::write(dir.join("prompt.txt"), "line1\n\"quoted\"\n").unwrap();
        std::env::set_var("MOCO_TEST_VAR", "va\"lue");
        let raw = r#"{"a":"{env:MOCO_TEST_VAR}","b":"{file:prompt.txt}","c":"{env:MOCO_UNSET_VAR_X}","d":"{not a var}"}"#;
        let v: Value = serde_json::from_str(&substitute(raw, &dir).unwrap()).unwrap();
        assert_eq!(v, json!({"a":"va\"lue","b":"line1\n\"quoted\"","c":"","d":"{not a var}"}));
        assert!(substitute(r#"{"a":"{file:missing.txt}"}"#, &dir).unwrap_err().contains("missing.txt"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merge_rules() {
        let base = json!({"model":"a","permissions":[1],"agents":{"build":{"steps":5,"permissions":[1]}},"list":[1]});
        let over = json!({"model":"b","permissions":[2],"agents":{"build":{"permissions":[2]}},"list":[2]});
        assert_eq!(
            merge(base, over, ""),
            json!({"model":"b","permissions":[1,2],"agents":{"build":{"steps":5,"permissions":[1,2]}},"list":[2]})
        );
    }

    #[test]
    fn layered_files_inner_directory_wins() {
        let root = tmp();
        let app = root.join("app");
        let proj = root.join("proj");
        let inner = proj.join("sub");
        std::fs::create_dir_all(app.join("config")).unwrap();
        std::fs::create_dir_all(inner.join(".moco")).unwrap();
        std::fs::write(app.join("config/moco.jsonc"), r#"{"model":"global/m","default_agent":"plan"}"#).unwrap();
        std::fs::write(proj.join("moco.json"), r#"{"model":"proj/m","permissions":[{"action":"shell","resource":"*","effect":"ask"}]}"#).unwrap();
        std::fs::write(inner.join(".moco/moco.jsonc"), r#"{"model":"inner/m", // c
            "permissions":[{"action":"shell","resource":"ls *","effect":"allow"},]}"#).unwrap();
        let l = load(&app, &inner);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.config.model.as_deref(), Some("inner/m"));
        assert_eq!(l.config.default_agent.as_deref(), Some("plan"));
        assert_eq!(l.config.permissions.len(), 2);
        assert_eq!(l.config.permissions[0].effect, Effect::Ask, "lower priority first");
        assert_eq!(l.sources.len(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bad_files_become_warnings_not_failures() {
        let root = tmp();
        std::fs::write(root.join("moco.json"), "{ not json").unwrap();
        std::fs::create_dir_all(root.join(".moco")).unwrap();
        std::fs::write(root.join(".moco/moco.json"), r#"{"model":"ok/m"}"#).unwrap();
        let l = load(&root.join("none"), &root);
        assert_eq!(l.warnings.len(), 1);
        assert!(l.warnings[0].contains("moco.json"));
        assert_eq!(l.config.model.as_deref(), Some("ok/m"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agents_resolve_with_config_rules_in_order() {
        let cfg: Config = serde_json::from_value(json!({
            "permissions": [{"action":"shell","resource":"*","effect":"ask"}],
            "default_agent": "docs",
            "agents": {
                "build": {"steps": 30, "permissions": [{"action":"shell","resource":"git *","effect":"allow"}]},
                "plan": {"disabled": true},
                "docs": {"description":"writes docs","system":"Be brief.","permissions":[{"action":"edit","resource":"*","effect":"ask"}]}
            }}))
        .unwrap();
        let agents = resolve_agents(&cfg);
        let ids: Vec<_> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["build", "general", "explore", "docs"]);
        let build = &agents[0];
        assert_eq!(build.steps, Some(30));
        use crate::permission::evaluate;
        assert_eq!(evaluate("shell", "ls", &build.permissions), Effect::Ask, "global rule beats the default allow");
        assert_eq!(evaluate("shell", "git status", &build.permissions), Effect::Allow, "agent rule beats the global one");
        assert_eq!(agents[3].system.as_deref(), Some("Be brief."));
        assert_eq!(evaluate("edit", "x", &agents[3].permissions), Effect::Ask);
    }

    #[test]
    fn formatter_setting_is_a_flag_or_a_map_and_later_files_win() {
        let cfg: Config = serde_json::from_value(json!({"formatter": {"rustfmt": {"command": ["rustfmt", "$FILE"], "extensions": [".rs"]}}})).unwrap();
        let Some(FormatterSetting::Formatters(m)) = cfg.formatter else { panic!() };
        assert_eq!((m["rustfmt"].command.clone(), m["rustfmt"].extensions.clone(), m["rustfmt"].disabled), (vec!["rustfmt".to_string(), "$FILE".to_string()], vec![".rs".to_string()], false));
        assert_eq!(serde_json::from_value::<Config>(json!({"formatter": false})).unwrap().formatter, Some(FormatterSetting::Flag(false)));
        assert_eq!(serde_json::from_value::<Config>(json!({})).unwrap().formatter, None);
        // Maps merge by name across files; a boolean in a later file replaces the lot.
        let merged = merge(json!({"formatter": {"a": {"command": ["a"]}, "b": {"command": ["b"]}}}), json!({"formatter": {"b": {"command": ["b2"]}}}), "");
        assert_eq!(merged["formatter"]["b"]["command"], json!(["b2"]));
        assert_eq!(merged["formatter"]["a"]["command"], json!(["a"]));
        assert_eq!(merge(json!({"formatter": {"a": {}}}), json!({"formatter": false}), "")["formatter"], json!(false));
    }

    #[test]
    fn mcp_remote_server_settings() {
        let cfg: Config = serde_json::from_value(json!({"mcp": {"servers": {
            "plain": {"url": "https://x/mcp"},
            "token": {"type": "remote", "url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}, "oauth": false},
            "oauth": {"url": "https://y/mcp", "oauth": {"client_id": "cid", "scope": "read", "callback_port": 8123}}}}})).unwrap();
        let s = &cfg.mcp.servers;
        assert!(s["plain"].is_remote() && s["token"].is_remote() && s["oauth"].is_remote());
        assert_eq!(OAuthSetting::resolve(&s["plain"].oauth), Some(OAuthConfig::default()), "OAuth is tried unless switched off");
        assert_eq!(OAuthSetting::resolve(&s["token"].oauth), None);
        let o = OAuthSetting::resolve(&s["oauth"].oauth).unwrap();
        assert_eq!((o.client_id.as_deref(), o.scope.as_deref(), o.callback_port), (Some("cid"), Some("read"), Some(8123)));
    }

    #[test]
    fn websearch_settings() {
        let cfg: Config = serde_json::from_value(json!({"websearch": {"provider": "Brave", "max_results": 5, "key_env": ["MY_KEY"]}})).unwrap();
        let c = WebSearchSetting::resolve(&cfg.websearch).unwrap();
        assert_eq!((c.provider.as_deref(), c.max_results, c.key_env), (Some("Brave"), Some(5), vec!["MY_KEY".to_string()]));
        let off: Config = serde_json::from_value(json!({"websearch": false})).unwrap();
        assert_eq!(WebSearchSetting::resolve(&off.websearch), None);
        let on: Config = serde_json::from_value(json!({"websearch": true})).unwrap();
        assert_eq!(WebSearchSetting::resolve(&on.websearch), Some(WebSearchConfig::default()));
        assert_eq!(WebSearchSetting::resolve(&Config::default().websearch), Some(WebSearchConfig::default()), "on by default");
    }

    #[test]
    fn tui_settings() {
        let cfg: Config = serde_json::from_value(json!({"tui": {"theme": "light", "keys": {"palette": "ctrl+k", "quit": ["ctrl+d", "ctrl+q"]},
            "leader": {"n": "/new"}, "bell": false, "away_after": 30, "sidebar": "auto"}})).unwrap();
        assert_eq!((cfg.tui.theme.as_deref(), cfg.tui.bell, cfg.tui.away_after), (Some("light"), Some(false), Some(30)));
        assert_eq!(cfg.tui.keys["quit"], json!(["ctrl+d", "ctrl+q"]));
        assert_eq!(cfg.tui.leader["n"], "/new");
        assert!(Config::default().tui.theme.is_none());
    }

    #[test]
    fn model_refs() {
        assert_eq!(split_model("anthropic/claude-x"), ("anthropic", "claude-x"));
        assert_eq!(split_model("claude-x"), ("anthropic", "claude-x"));
        assert_eq!(split_model("openrouter/anthropic/claude-x"), ("openrouter", "anthropic/claude-x"));
    }
}
