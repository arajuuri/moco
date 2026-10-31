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
    load_key_with(provider, &[], app_dir)
}

/// Key lookup: the given environment variables (from the catalog), then `<PROVIDER>_API_KEY`, then `auth.json`.
pub fn load_key_with(provider: &str, env_names: &[String], app_dir: &Path) -> Option<String> {
    for name in env_names.iter().cloned().chain(std::iter::once(env_var_for(provider))) {
        if let Ok(k) = std::env::var(&name) {
            if !k.is_empty() {
                return Some(k);
            }
        }
    }
    let text = std::fs::read_to_string(app_dir.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get(provider)?.get("key")?.as_str().map(String::from)
}

/// Save a key into `auth.json` (created with mode 0600), keeping other providers' entries.
pub fn store_key(provider: &str, key: &str, app_dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(app_dir)?;
    let path = app_dir.join("auth.json");
    let mut root: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    root[provider] = serde_json::json!({ "key": key });
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut f, serde_json::to_string_pretty(&root)?.as_bytes())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
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

    #[test]
    fn store_key_keeps_other_entries_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(crate::id::new_id("auth-store"));
        let p = store_key("a", "k1", &dir).unwrap();
        store_key("b", "k2", &dir).unwrap();
        store_key("a", "k3", &dir).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"a":{"key":"k3"},"b":{"key":"k2"}}));
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(permissions_warning(&dir).is_none());
        assert_eq!(load_key_with("zz-none", &["MOCO_TEST_NO_SUCH_VAR".into()], &dir), None);
        assert_eq!(load_key("b", &dir).as_deref(), Some("k2"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
