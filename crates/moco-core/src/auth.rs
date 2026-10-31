//! Credential lookup (docs/02-domain-model.md): environment variable first, then
//! `<data dir>/auth.json` shaped `{"<provider-id>": {"key": "..."}}`.
use std::path::{Path, PathBuf};

/// `~/Library/Application Support/moco` on macOS, `$XDG_DATA_HOME/moco` (or `~/.local/share/moco`) elsewhere.
/// `MOCO_HOME` overrides both.
pub fn app_dir() -> PathBuf {
    if let Ok(p) = std::env::var("MOCO_HOME") {
        return PathBuf::from(p);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/moco")
    } else {
        std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/share")).join("moco")
    }
}

pub fn env_var_for(provider: &str) -> String {
    format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"))
}

pub fn load_key(provider: &str, app_dir: &Path) -> Option<String> {
    if let Ok(k) = std::env::var(env_var_for(provider)) {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let text = std::fs::read_to_string(app_dir.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get(provider)?.get("key")?.as_str().map(String::from)
}

/// Warn text if `auth.json` is readable by group/others.
pub fn permissions_warning(app_dir: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let p = app_dir.join("auth.json");
    let mode = std::fs::metadata(&p).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| format!("{} has mode {mode:o}; run: chmod 600 '{}'", p.display(), p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_and_file_lookup() {
        assert_eq!(env_var_for("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(env_var_for("open-router"), "OPEN_ROUTER_API_KEY");
        let dir = std::env::temp_dir().join(crate::id::new_id("auth"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), r#"{"zzprov": {"key": "sk-file"}}"#).unwrap();
        assert_eq!(load_key("zzprov", &dir).as_deref(), Some("sk-file"));
        assert_eq!(load_key("nonexistent-prov", &dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
