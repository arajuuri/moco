//! Optional OS sandbox for the `shell` tool (macOS Seatbelt via `/usr/bin/sandbox-exec`).
//!
//! The profile is `(allow default)` with `file-write*` denied except for the project root, temp directories, common
//! caches and the paths in `sandbox.writable`. `sandbox.network: false` also denies outbound network except localhost.
//! `sandbox-exec` is a separate binary, so nothing new is linked into moco. Where it is missing (Linux), commands run
//! unsandboxed and `unavailable_notice` says so.
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `sandbox` in the config: `{"enabled": true, "writable": ["~/.cargo"], "network": true}`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// Extra writable paths (`~/` allowed).
    pub writable: Vec<String>,
    /// `false` denies outbound network except localhost. Default on.
    pub network: Option<bool>,
}

static ACTIVE: RwLock<Option<SandboxConfig>> = RwLock::new(None);

/// Make `cfg` the process-wide sandbox setting (called once at startup).
pub fn configure(cfg: &SandboxConfig) {
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = cfg.enabled.then(|| cfg.clone());
}

pub fn available() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists()
}

/// Sandbox is configured on and can be applied here.
pub fn active() -> bool {
    available() && ACTIVE.read().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// A warning for startup when the sandbox is enabled but this platform cannot provide it.
pub fn unavailable_notice(cfg: &SandboxConfig) -> Option<String> {
    (cfg.enabled && !available()).then(|| "sandbox is enabled but sandbox-exec is not available on this system; shell commands run WITHOUT a sandbox".to_string())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn expand(p: &str, home: &Path) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home.to_path_buf(),
        None => PathBuf::from(p),
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Seatbelt matches resolved paths, so allow both the path as written and its symlink-free form.
fn variants(p: &Path) -> Vec<PathBuf> {
    let mut v = vec![p.to_path_buf()];
    if let Ok(real) = p.canonicalize() {
        if real != p {
            v.push(real);
        }
    }
    v
}

impl SandboxConfig {
    /// The Seatbelt profile for a command whose project is `project_root`. Pure: all environment comes in as arguments.
    pub fn profile(&self, project_root: &Path, home: &Path, tmpdir: Option<&Path>) -> String {
        let mut paths: Vec<PathBuf> = vec![project_root.to_path_buf()];
        paths.extend(tmpdir.map(Path::to_path_buf));
        paths.extend(["/private/tmp", "/private/var/folders"].map(PathBuf::from));
        paths.extend([".cargo/registry", ".cargo/git", ".cargo/.package-cache", ".npm", "Library/Caches", ".cache"].map(|p| home.join(p)));
        paths.extend(self.writable.iter().map(|w| expand(w, home)));
        let mut allowed: Vec<PathBuf> = vec![];
        for p in paths.iter().flat_map(|p| variants(p)) {
            if !allowed.contains(&p) {
                allowed.push(p);
            }
        }
        let mut out = String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
        for p in &allowed {
            // `.cargo/.package-cache` is a file, the rest are directories; `subpath` covers both.
            out.push_str(&format!("  (subpath \"{}\")\n", escape(&p.to_string_lossy())));
        }
        out.push_str("  (literal \"/dev/null\")\n  (literal \"/dev/tty\")\n  (literal \"/dev/dtracehelper\"))\n");
        if self.network == Some(false) {
            out.push_str("(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n");
        }
        out
    }
}

/// The program and arguments that run `command` through `shell` inside the sandbox, or `None` when the sandbox is off
/// or unavailable.
pub fn wrap(shell: &str, command: &str, project_root: &Path) -> Option<(String, Vec<String>)> {
    if !available() {
        return None;
    }
    let cfg = ACTIVE.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    let tmp = std::env::var("TMPDIR").ok().map(PathBuf::from);
    let profile = cfg.profile(project_root, &home(), tmp.as_deref());
    Some((SANDBOX_EXEC.into(), vec!["-p".into(), profile, shell.into(), "-c".into(), command.into()]))
}

const HINT: &str = "[sandbox: this command may have been blocked by the sandbox, which only allows writes to the project, temp directories, \
caches and `sandbox.writable` paths. If it really needs access outside, retry with `unsandboxed: true`; the user is asked first.]";

pub fn looks_denied(output: &str) -> bool {
    output.contains("Operation not permitted") || output.contains("deny file-write") || output.contains("deny(1) file-write")
}

/// Append the denial hint to the output of a failed sandboxed command that looks blocked.
pub fn annotate(text: &mut String, exit: Option<i32>) {
    if exit != Some(0) && looks_denied(text) {
        text.push_str("\n\n");
        text.push_str(HINT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(writable: &[&str], network: Option<bool>) -> SandboxConfig {
        SandboxConfig { enabled: true, writable: writable.iter().map(|s| s.to_string()).collect(), network }
    }

    #[test]
    fn golden_profile() {
        let p = cfg(&["~/.cargo-extra", "/opt/out"], None).profile(Path::new("/nonexistent/proj"), Path::new("/nonexistent/home"), Some(Path::new("/nonexistent/tmp")));
        let want = "(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath \"/nonexistent/proj\")
  (subpath \"/nonexistent/tmp\")
  (subpath \"/private/tmp\")
  (subpath \"/private/var/folders\")
  (subpath \"/nonexistent/home/.cargo/registry\")
  (subpath \"/nonexistent/home/.cargo/git\")
  (subpath \"/nonexistent/home/.cargo/.package-cache\")
  (subpath \"/nonexistent/home/.npm\")
  (subpath \"/nonexistent/home/Library/Caches\")
  (subpath \"/nonexistent/home/.cache\")
  (subpath \"/nonexistent/home/.cargo-extra\")
  (subpath \"/opt/out\")
  (literal \"/dev/null\")
  (literal \"/dev/tty\")
  (literal \"/dev/dtracehelper\"))
";
        assert_eq!(p, want);
    }

    #[test]
    fn network_off_denies_outbound_except_localhost() {
        let p = cfg(&[], Some(false)).profile(Path::new("/nonexistent/p"), Path::new("/nonexistent/h"), None);
        assert!(p.ends_with("(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n"), "{p}");
        let on = cfg(&[], Some(true)).profile(Path::new("/nonexistent/p"), Path::new("/nonexistent/h"), None);
        assert!(!on.contains("network"));
    }

    #[test]
    fn paths_are_escaped_and_symlinks_resolved() {
        let p = cfg(&[], None).profile(Path::new("/nonexistent/a\"b\\c"), Path::new("/nonexistent/h"), None);
        assert!(p.contains(r#"(subpath "/nonexistent/a\"b\\c")"#), "{p}");
        let dir = std::env::temp_dir().join(format!("moco-sbx-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let p = cfg(&[], None).profile(&dir.join("link"), Path::new("/nonexistent/h"), None);
        let real = dir.join("real").canonicalize().unwrap();
        assert!(p.contains(&format!("(subpath \"{}\")", dir.join("link").display())) && p.contains(&format!("(subpath \"{}\")", real.display())), "{p}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn denial_hint_only_on_failed_denied_commands() {
        let mut t = "touch: /x: Operation not permitted".to_string();
        annotate(&mut t, Some(1));
        assert!(t.contains("unsandboxed: true"));
        let mut ok = "Operation not permitted (but exit 0)".to_string();
        annotate(&mut ok, Some(0));
        assert!(!ok.contains("unsandboxed"));
        let mut other = "no such file".to_string();
        annotate(&mut other, Some(1));
        assert!(!other.contains("unsandboxed"));
    }

    #[test]
    fn config_parses_and_expands_home() {
        let c: SandboxConfig = serde_json::from_value(serde_json::json!({"enabled": true, "writable": ["~/.cargo"], "network": false})).unwrap();
        assert!(c.enabled && c.network == Some(false));
        assert_eq!(expand("~/.cargo", Path::new("/h")), PathBuf::from("/h/.cargo"));
        assert_eq!(unavailable_notice(&SandboxConfig::default()), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_writes_outside_the_project() {
        let base = std::env::temp_dir().join(format!("moco-sbx-it-{}", std::process::id()));
        let project = base.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let outside = PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".moco-sandbox-test-{}", std::process::id()));
        let c = SandboxConfig { enabled: true, ..Default::default() };
        let profile = c.profile(&project, &home(), None);
        let run = |cmd: &str| std::process::Command::new(SANDBOX_EXEC).args(["-p", &profile, "/bin/sh", "-c", cmd]).current_dir(&project).output().unwrap();
        let bad = run(&format!("touch {}", outside.display()));
        assert!(!bad.status.success() && looks_denied(&String::from_utf8_lossy(&bad.stderr)), "{:?}", bad);
        assert!(!outside.exists());
        assert!(run("touch inside").status.success());
        assert!(project.join("inside").exists());
        std::fs::remove_dir_all(&base).ok();
    }
}
