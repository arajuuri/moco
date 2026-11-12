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

pub fn env_names_for(provider: &str, env_names: &[String]) -> Vec<String> {
    env_names.iter().cloned().chain(std::iter::once(env_var_for(provider))).collect()
}

/// Give a child process moco's environment without the provider key variables in `provider_keys` (moco's own
/// credentials, from `Catalog::key_env_vars`), then `configured` on top: what the child's own config sets on purpose,
/// so a key named there still reaches it. Shell commands, MCP servers, formatters and checks all start this way (#92, #122).
pub fn child_env(cmd: &mut std::process::Command, provider_keys: &[String], configured: &std::collections::BTreeMap<String, String>) {
    for name in provider_keys {
        cmd.env_remove(name);
    }
    cmd.envs(configured);
}

pub fn load_key(provider: &str, app_dir: &Path) -> Option<String> {
    load_key_with(provider, &[], app_dir)
}

/// Key lookup: the given environment variables (from the catalog), then `<PROVIDER>_API_KEY`, then `auth.json`.
pub fn load_key_with(provider: &str, env_names: &[String], app_dir: &Path) -> Option<String> {
    for name in env_names_for(provider, env_names) {
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
    let path = app_dir.join("auth.json");
    let mut root: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    root[provider] = serde_json::json!({ "key": key });
    write_private(&path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(path)
}

/// Create `dir` (and any missing parents) with mode 0700. An existing directory is left as it is; `secure_app_dir`
/// tightens the ones moco owns once at startup.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Replace `path` atomically with `bytes`, mode 0600 (a temp file in the same directory, then a rename). A missing
/// parent directory is created 0700. Used for what moco keeps that may hold secrets: `auth.json`, `mcp-auth.json`,
/// the trust list, media blobs and snapshot contents.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    // Unique per process and per call, so threads writing the same file never share a temp file.
    name.push(format!(".moco-tmp-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let tmp = path.with_file_name(name);
    let written = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| {
        std::io::Write::write_all(&mut f, bytes)?;
        // The mode given to open is masked by the umask; this makes it exactly 0600 whatever the umask is.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
    });
    written.and_then(|_| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Open `path` for writing, created with mode 0600 if it is new (an existing file keeps its mode, so a file the
/// user made readable on purpose stays that way). `append` appends; otherwise the file is truncated. For files
/// written in place: tool-output spill files, background job logs, the prompt history and session exports.
pub fn create_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).append(append).truncate(!append).mode(0o600).open(path)
}

/// moco's own subdirectories of the data dir: `data/` holds the database, media blobs, snapshots, spilled tool
/// output and the logs.
const PRIVATE_SUBDIRS: &[&str] = &["data", "data/blobs", "data/snapshots", "data/tool-output", "data/logs"];

/// Make the data dir private at startup (#164). A missing one is created 0700. moco's own subdirectories (`data/`
/// and those below it) are tightened to 0700 when they are looser: nothing else has a reason to read them. The
/// data dir itself is tightened too when it is the default location. A `MOCO_HOME` the user chose may be shared on
/// purpose, so for that one this returns a warning and leaves the mode alone; its secrets are covered anyway
/// (`data/` is 0700, `auth.json` and `mcp-auth.json` are 0600).
pub fn secure_app_dir(app_dir: &Path) -> Option<String> {
    secure_app_dir_with(app_dir, std::env::var_os("MOCO_HOME").is_none())
}

fn secure_app_dir_with(app_dir: &Path, default_location: bool) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let tighten = |dir: &Path| {
        if let Ok(m) = std::fs::metadata(dir) {
            let mode = m.permissions().mode();
            if m.is_dir() && mode & 0o077 != 0 {
                if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & 0o7700)) {
                    crate::log_warn!("auth", "could not make {} private: {e}", dir.display());
                }
            }
        }
    };
    let mut warning = None;
    match std::fs::metadata(app_dir) {
        Err(_) => {
            let _ = create_private_dir(app_dir);
        }
        Ok(m) if m.permissions().mode() & 0o077 != 0 => {
            if default_location {
                tighten(app_dir);
            } else {
                let mode = m.permissions().mode() & 0o777;
                warning = Some(format!("the data dir {} (MOCO_HOME) has mode {mode:o}; moco leaves it as it is; to keep others out of it: chmod 700 '{}'", app_dir.display(), app_dir.display()));
            }
        }
        Ok(_) => {}
    }
    let _ = create_private_dir(&app_dir.join("data"));
    for sub in PRIVATE_SUBDIRS {
        tighten(&app_dir.join(sub));
    }
    warning
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

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).permissions().mode() & 0o777
    }

    #[test]
    fn a_fresh_data_dir_is_0700_with_0600_files() {
        let root = std::env::temp_dir().join(crate::id::new_id("auth-modes"));
        let app = root.join("moco-home");
        assert!(secure_app_dir_with(&app, false).is_none(), "a new dir is created private, nothing to warn about");
        assert_eq!(mode(&app), 0o700);
        assert_eq!(mode(&app.join("data")), 0o700);

        // Everything moco writes there.
        store_key("a", "k", &app).unwrap();
        crate::mcp::trust::approve(&app, "fp").unwrap();
        let store = crate::store::Store::open(&app.join("data/moco.db")).unwrap();
        store.exec_for_test("create table t(x); insert into t values (1)");
        let hash = crate::blobs::put(&app, b"secret attachment").unwrap();
        let spilled = crate::truncate::truncate(&"line\n".repeat(3000), &app.join("data/tool-output")).unwrap().output_path.unwrap();
        create_private(&app.join("data/tui-history"), true).unwrap();
        for f in [app.join("auth.json"), app.join("data/mcp-trust.json"), app.join("data/moco.db"), app.join("data/moco.db-wal"), app.join("data/moco.db-shm"), app.join("data/blobs").join(&hash[..2]).join(&hash[2..]), spilled.clone(), app.join("data/tui-history")] {
            assert_eq!(mode(&f), 0o600, "{}", f.display());
        }
        for d in [app.join("data/blobs"), app.join("data/blobs").join(&hash[..2]), app.join("data/tool-output")] {
            assert_eq!(mode(&d), 0o700, "{}", d.display());
        }
        drop(store);
        // No temp files are left behind by the atomic writes.
        for d in [app.clone(), app.join("data")] {
            assert!(std::fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("moco-tmp")));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn existing_loose_dirs_and_database_are_tightened_but_moco_home_only_warned() {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        let root = std::env::temp_dir().join(crate::id::new_id("auth-tighten"));
        // As an older moco left it under a 022 umask.
        let app = root.join("moco");
        for d in ["data/logs", "data/blobs", "data/snapshots", "data/tool-output"] {
            std::fs::create_dir_all(app.join(d)).unwrap();
        }
        drop(crate::store::Store::open(&app.join("data/moco.db")).unwrap());
        for d in ["", "data", "data/logs", "data/blobs", "data/snapshots", "data/tool-output"] {
            set(&app.join(d), 0o755);
        }
        set(&app.join("data/moco.db"), 0o644);
        std::fs::write(app.join("data/moco.db-wal"), b"").unwrap();
        set(&app.join("data/moco.db-wal"), 0o644);

        // A MOCO_HOME the user chose: warned about, left alone; moco's own subdirectories are tightened.
        let w = secure_app_dir_with(&app, false).expect("a warning");
        assert!(w.contains("755") && w.contains("chmod 700") && w.contains(&app.display().to_string()), "{w}");
        assert_eq!(mode(&app), 0o755);
        for d in ["data", "data/logs", "data/blobs", "data/snapshots", "data/tool-output"] {
            assert_eq!(mode(&app.join(d)), 0o700, "{d}");
        }
        // The default location is moco's own: tightened silently.
        assert!(secure_app_dir_with(&app, true).is_none());
        assert_eq!(mode(&app), 0o700);
        // Opening the database tightens its files.
        let store = crate::store::Store::open(&app.join("data/moco.db")).unwrap();
        assert_eq!(mode(&app.join("data/moco.db")), 0o600);
        assert_eq!(mode(&app.join("data/moco.db-wal")), 0o600);
        drop(store);
        // A file the user created outside the data dir (an export target) keeps its mode.
        let export = root.join("export.md");
        std::fs::write(&export, "old").unwrap();
        set(&export, 0o644);
        std::io::Write::write_all(&mut create_private(&export, false).unwrap(), b"new").unwrap();
        assert_eq!((mode(&export), std::fs::read_to_string(&export).unwrap().as_str()), (0o644, "new"));
        let _ = std::fs::remove_dir_all(root);
    }
}
