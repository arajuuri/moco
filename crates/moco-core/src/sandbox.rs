//! Optional OS sandbox for the `shell` tool (macOS Seatbelt via `/usr/bin/sandbox-exec`).
//!
//! The profile is `(allow default)` with `file-write*` denied except for the project root, temp directories, common
//! caches and the paths in `sandbox.writable`. Within those, writes that would plant code for a later, unsandboxed
//! run stay denied (`PROJECT_PROTECTED`, `HOME_PROTECTED`, moco's data directory), and so does renaming or removing
//! any directory above them. Reading credentials (`READ_PROTECTED`, moco's `auth.json`) is denied too: a command's
//! output goes to the model, and with network anywhere. `sandbox.network: false` also denies outbound network except
//! localhost. Seatbelt applies the last matching rule, so the denials come after the allows; only the `literal` and
//! `subpath` filters are used, which Lion (10.7) has.
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

/// Seatbelt matches resolved paths, so a rule names both the path as written and its symlink-free form. That form
/// is needed for paths that do not exist yet too (a `.moco` the command might create under `/var/folders`, which is
/// `/private/var/folders`), so the existing part is resolved and the rest appended.
fn variants(p: &Path) -> Vec<PathBuf> {
    let mut v = vec![p.to_path_buf()];
    let real = crate::tools::real_path(p);
    if real != p {
        v.push(real);
    }
    v
}

fn all_variants<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    for p in paths.into_iter().flat_map(|p| variants(p)) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Project paths a sandboxed command may not create, change, rename or delete, because they make code run later
/// outside the sandbox. Git runs `.git/hooks` and the commands in `.git/config` (`core.fsmonitor`, `core.pager`,
/// `diff.external`, aliases, filters, `core.hooksPath`, `include.path`); `config.worktree` is read as config too, and
/// `commondir` points git at another directory's config. `.git/modules` and `.git/worktrees` hold whole git dirs
/// (with their own hooks and config) that a rename could swap in. The rest are the configs moco, Claude Code and
/// VS Code load from the project (`.claude/settings.json` hooks, `.vscode/tasks.json` "runOn": "folderOpen").
/// What a normal commit, fetch, merge or rebase writes (`objects`, `refs`, `logs`, `index`, `HEAD`, `ORIG_HEAD`,
/// `FETCH_HEAD`, `COMMIT_EDITMSG`, `packed-refs`, `info/exclude`) stays writable. `.git/info/attributes` is left
/// writable as well: it can only pick filter and diff drivers that a config defines, as an in-tree `.gitattributes`
/// can. `.git` itself and the project root get a `literal` deny from `profile`, so neither can be renamed away and
/// replaced.
const PROJECT_PROTECTED: &[&str] = &[".git/hooks", ".git/config", ".git/config.worktree", ".git/commondir", ".git/modules", ".git/worktrees", ".moco", ".claude", ".agents", ".vscode", "moco.json", "moco.jsonc"];

/// Paths under `$HOME` that shells, launchd, git, cargo, editors or Python run code from. None is writable by
/// default; these matter when `sandbox.writable` (or a project root at `~`) covers a parent such as `~`, `~/Library`
/// or `~/.cargo`. Listing one of them (or a path inside it) in `sandbox.writable` makes it writable again.
/// `.cargo/registry` and `.cargo/git` hold the sources other projects' `build.rs` and proc macros are compiled from.
const HOME_PROTECTED: &[&str] = &[
    ".profile", ".bashrc", ".bash_profile", ".bash_login", ".bash_logout", ".zshrc", ".zshenv", ".zprofile", ".zlogin", ".zlogout", ".cshrc", ".tcshrc", ".login", ".logout",
    ".config/fish", ".MacOSX", "Library/LaunchAgents", "Library/Preferences/com.apple.loginitems.plist", ".ssh", ".gitconfig", ".config/git",
    ".cargo/config", ".cargo/config.toml", ".cargo/bin", ".cargo/registry", ".cargo/git", ".rustup", ".local/bin", "bin", ".npmrc", ".yarnrc",
    "Library/Python", ".cache/pre-commit", ".vimrc", ".vim", ".config/nvim", ".emacs", ".emacs.d", ".tmux.conf",
];

/// Credential files and directories under `$HOME` that sandboxed commands may not read (`file-read-data`, so `stat`
/// still works). `~/.ssh/config` and `~/.ssh/known_hosts` are allowed again so that git over ssh works with keys in
/// the agent.
const READ_PROTECTED: &[&str] = &[".ssh", ".aws", ".azure", ".config/gcloud", ".config/gh", ".gnupg", ".netrc", ".git-credentials", ".config/git/credentials", ".cargo/credentials", ".cargo/credentials.toml", ".pypirc", "Library/Keychains"];
const READ_ALLOWED: &[&str] = &[".ssh/config", ".ssh/known_hosts"];

/// What the profile needs from the environment, so that `SandboxConfig::profile` stays pure.
#[derive(Debug, Clone)]
pub struct ProfileEnv {
    pub home: PathBuf,
    pub tmpdir: Option<PathBuf>,
    /// moco's data directory (`auth::app_dir`).
    pub app_dir: PathBuf,
    /// `$MOCO_CONFIG`, a user config file.
    pub moco_config: Option<PathBuf>,
}

impl ProfileEnv {
    pub fn current() -> ProfileEnv {
        ProfileEnv {
            home: home(),
            tmpdir: std::env::var_os("TMPDIR").map(PathBuf::from),
            app_dir: crate::auth::app_dir(),
            moco_config: std::env::var_os("MOCO_CONFIG").filter(|s| !s.is_empty()).map(PathBuf::from),
        }
    }
}

fn rules(out: &mut String, head: &str, items: &[(&str, &PathBuf)]) {
    if items.is_empty() {
        return;
    }
    out.push_str(head);
    for (kind, p) in items {
        out.push_str(&format!("\n  ({kind} \"{}\")", escape(&p.to_string_lossy())));
    }
    out.push_str(")\n");
}

impl SandboxConfig {
    /// The Seatbelt profile for a command whose project is `project_root`. Pure: all environment comes in `env`.
    pub fn profile(&self, project_root: &Path, env: &ProfileEnv) -> String {
        let home = env.home.as_path();
        let user: Vec<PathBuf> = self.writable.iter().map(|w| expand(w, home)).collect();
        let mut paths: Vec<PathBuf> = vec![project_root.to_path_buf()];
        paths.extend(env.tmpdir.clone());
        paths.extend(["/private/tmp", "/private/var/folders"].map(PathBuf::from));
        // Cargo's lock file and last-use database; the registry itself is read-only (`HOME_PROTECTED`).
        paths.extend([".cargo/.package-cache", ".cargo/.global-cache", ".cargo/.global-cache-journal", ".cargo/.global-cache-wal", ".cargo/.global-cache-shm"].map(|p| home.join(p)));
        paths.extend([".npm", "Library/Caches", ".cache"].map(|p| home.join(p)));
        paths.extend(user.iter().cloned());
        let allowed = all_variants(&paths);

        // Protected paths, minus any that contain the project (a `MOCO_HOME` above it) or that the user listed
        // (or listed a path inside of) in `sandbox.writable`.
        let root = variants(project_root);
        let user = all_variants(&user);
        let mut protected: Vec<PathBuf> = PROJECT_PROTECTED.iter().map(|p| project_root.join(p)).collect();
        protected.extend(HOME_PROTECTED.iter().map(|p| home.join(p)));
        protected.push(env.app_dir.clone());
        protected.extend(["config", "auth.json", "mcp-auth.json"].map(|p| env.app_dir.join(p)));
        protected.extend(env.moco_config.clone());
        protected.retain(|p| !variants(p).iter().any(|v| root.iter().chain(&user).any(|keep| keep.starts_with(v))));
        let protected = all_variants(&protected);
        // A protected path inside a writable one could still be swapped in by renaming a directory above it, so
        // those directories themselves (not their contents) are denied as well: the project root and `.git` among them.
        let mut above: Vec<PathBuf> = vec![];
        for p in &protected {
            for a in p.ancestors().skip(1) {
                let a = a.to_path_buf();
                if allowed.iter().any(|w| a.starts_with(w)) && !above.contains(&a) && !protected.contains(&a) {
                    above.push(a);
                }
            }
        }

        let mut read_denied: Vec<PathBuf> = READ_PROTECTED.iter().map(|p| home.join(p)).collect();
        read_denied.extend(["auth.json", "mcp-auth.json"].map(|p| env.app_dir.join(p)));
        read_denied.retain(|p| !variants(p).iter().any(|v| root.iter().any(|r| r.starts_with(v))));
        let read_denied = all_variants(&read_denied);
        let read_allowed = all_variants(&READ_ALLOWED.iter().map(|p| home.join(p)).collect::<Vec<_>>());

        let mut out = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        let mut allow: Vec<(&str, &PathBuf)> = allowed.iter().map(|p| ("subpath", p)).collect();
        let devices = ["/dev/null", "/dev/tty", "/dev/dtracehelper"].map(PathBuf::from);
        allow.extend(devices.iter().map(|p| ("literal", p)));
        rules(&mut out, "(allow file-write*", &allow);
        let deny: Vec<(&str, &PathBuf)> = protected.iter().map(|p| ("subpath", p)).chain(above.iter().map(|p| ("literal", p))).collect();
        rules(&mut out, "(deny file-write*", &deny);
        rules(&mut out, "(deny file-read-data", &read_denied.iter().map(|p| ("subpath", p)).collect::<Vec<_>>());
        rules(&mut out, "(allow file-read-data", &read_allowed.iter().map(|p| ("literal", p)).collect::<Vec<_>>());
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
    let profile = cfg.profile(project_root, &ProfileEnv::current());
    Some((SANDBOX_EXEC.into(), vec!["-p".into(), profile, shell.into(), "-c".into(), command.into()]))
}

const HINT: &str = "[sandbox: this command may have been blocked by the sandbox, which only allows writes to the project, temp directories, \
caches and `sandbox.writable` paths, keeps the project's .git/hooks, .git/config, .moco, .claude and .vscode and the cargo registry \
(~/.cargo/registry, ~/.cargo/git) read-only, and denies reading credentials such as ~/.ssh keys. If it really needs that access \
(e.g. `git config`, `git init`, or cargo downloading new dependencies), retry with `unsandboxed: true`; the user is asked first.]";

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

    fn env(home: &str) -> ProfileEnv {
        ProfileEnv { home: home.into(), tmpdir: None, app_dir: format!("{home}/Library/Application Support/moco").into(), moco_config: None }
    }

    /// How Seatbelt decides this profile's file rules: the last rule whose filter matches wins, and the default is
    /// allow. `subpath` matches the path and everything below it, `literal` only the path. `op` is `write` or `read`.
    fn permits(profile: &str, op: &str, path: &str) -> bool {
        let mut verdict = true;
        let mut block: Option<bool> = None;
        for line in profile.lines() {
            if line == "(deny file-write*)" {
                if op == "write" {
                    verdict = false;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix('(') {
                let on = |o: &str| op == o && rest.ends_with(if o == "write" { "file-write*" } else { "file-read-data" });
                block = (on("write") || on("read")).then(|| rest.starts_with("allow"));
                continue;
            }
            let Some(allow) = block else { continue };
            let item = line.trim().trim_end_matches(')');
            let (kind, quoted) = item.trim_start_matches('(').split_once(' ').unwrap();
            let p = quoted.trim_matches('"').replace("\\\"", "\"").replace("\\\\", "\\");
            if path == p || (kind == "subpath" && path.starts_with(&format!("{p}/"))) {
                verdict = allow;
            }
        }
        verdict
    }

    fn writable(profile: &str, path: &str) -> bool {
        permits(profile, "write", path)
    }

    #[test]
    fn golden_profile() {
        let e = ProfileEnv { tmpdir: Some("/nonexistent/tmp".into()), ..env("/nonexistent/home") };
        let p = cfg(&["~/.cargo-extra", "/opt/out"], None).profile(Path::new("/nonexistent/proj"), &e);
        assert_eq!(p, GOLDEN);
    }

    #[test]
    fn git_commit_paths_stay_writable_but_hooks_and_config_do_not() {
        let p = cfg(&[], None).profile(Path::new("/n/proj"), &env("/n/home"));
        for ok in [
            "/n/proj/src/main.rs", "/n/proj/.gitignore", "/n/proj/.git/objects/ab/cdef", "/n/proj/.git/objects/pack/tmp_pack_x", "/n/proj/.git/refs/heads/main",
            "/n/proj/.git/refs/heads/main.lock", "/n/proj/.git/logs/HEAD", "/n/proj/.git/index", "/n/proj/.git/index.lock", "/n/proj/.git/HEAD", "/n/proj/.git/ORIG_HEAD",
            "/n/proj/.git/FETCH_HEAD", "/n/proj/.git/COMMIT_EDITMSG", "/n/proj/.git/packed-refs", "/n/proj/.git/info/exclude", "/n/proj/.git/config.lock",
        ] {
            assert!(writable(&p, ok), "{ok} must stay writable\n{p}");
        }
        for bad in [
            "/n/proj/.git/hooks", "/n/proj/.git/hooks/pre-commit", "/n/proj/.git/config", "/n/proj/.git/config.worktree", "/n/proj/.git/commondir",
            "/n/proj/.git/modules/sub/hooks/post-checkout", "/n/proj/.git/modules/sub/config", "/n/proj/.git/worktrees/wt/commondir", "/n/proj/.git", "/n/proj",
        ] {
            assert!(!writable(&p, bad), "{bad} must be denied\n{p}");
        }
        assert!(p.contains("\n  (literal \"/n/proj/.git\")") && p.contains("\n  (literal \"/n/proj\")"), "only the directories themselves, not their contents:\n{p}");
        assert!(p.find("(deny file-write*\n").unwrap() > p.find("(allow file-write*\n").unwrap(), "denials must follow the allows (last match wins)");
    }

    #[test]
    fn tool_configs_in_the_project_are_read_only() {
        let p = cfg(&[], None).profile(Path::new("/n/proj"), &env("/n/home"));
        for bad in ["/n/proj/.moco/moco.json", "/n/proj/.moco", "/n/proj/moco.json", "/n/proj/moco.jsonc", "/n/proj/.claude/settings.json", "/n/proj/.agents/skills/x/SKILL.md", "/n/proj/.vscode/tasks.json"] {
            assert!(!writable(&p, bad), "{bad}\n{p}");
        }
        assert!(writable(&p, "/n/proj/AGENTS.md") && writable(&p, "/n/proj/docs/moco.json.md"));
    }

    #[test]
    fn cargo_registry_is_read_only_unless_listed() {
        let p = cfg(&[], None).profile(Path::new("/n/proj"), &env("/n/home"));
        assert!(!writable(&p, "/n/home/.cargo/registry/src/index/serde-1.0.0/build.rs") && !writable(&p, "/n/home/.cargo/git/checkouts/x/build.rs"), "{p}");
        assert!(writable(&p, "/n/home/.cargo/.package-cache") && writable(&p, "/n/home/.cargo/.global-cache-wal") && writable(&p, "/n/home/.npm/_cacache/x"));
        let p = cfg(&["~/.cargo"], None).profile(Path::new("/n/proj"), &env("/n/home"));
        assert!(writable(&p, "/n/home/.cargo/foo"), "{p}");
        for bad in ["/n/home/.cargo/registry/src/x/build.rs", "/n/home/.cargo/git/db/x", "/n/home/.cargo/bin/cargo", "/n/home/.cargo/config.toml", "/n/home/.cargo/config"] {
            assert!(!writable(&p, bad), "a writable ~/.cargo keeps {bad} protected\n{p}");
        }
        let p = cfg(&["~/.cargo", "~/.cargo/registry"], None).profile(Path::new("/n/proj"), &env("/n/home"));
        assert!(writable(&p, "/n/home/.cargo/registry/src/x/build.rs") && !writable(&p, "/n/home/.cargo/bin/cargo"), "listing it opts in\n{p}");
    }

    #[test]
    fn startup_files_stay_protected_under_a_writable_home() {
        let e = ProfileEnv { moco_config: Some("/n/home/dotfiles/moco.json".into()), ..env("/n/home") };
        let p = cfg(&["~"], None).profile(Path::new("/n/proj"), &e);
        assert!(writable(&p, "/n/home/notes.txt") && writable(&p, "/n/home/Library/Caches/x"), "{p}");
        for bad in [
            "/n/home/.zshrc", "/n/home/.bash_profile", "/n/home/.profile", "/n/home/Library/LaunchAgents/evil.plist", "/n/home/.gitconfig", "/n/home/.config/git/config",
            "/n/home/.ssh/authorized_keys", "/n/home/.ssh/config", "/n/home/.rustup/toolchains/x/bin/rustc", "/n/home/Library/Python/3.9/lib/python/site-packages/x.pth",
            "/n/home/Library/Application Support/moco/config/moco.json", "/n/home/Library/Application Support/moco/auth.json",
            "/n/home/Library/Application Support/moco/data/moco.db", "/n/home/dotfiles/moco.json",
        ] {
            assert!(!writable(&p, bad), "{bad}\n{p}");
        }
        // Renaming a directory above them would swap them all out; the directories' other contents stay writable.
        for bad in ["/n/home", "/n/home/Library", "/n/home/Library/Application Support", "/n/home/.config"] {
            assert!(!writable(&p, bad), "{bad}\n{p}");
        }
        assert!(writable(&p, "/n/home/Library/Mail/x") && writable(&p, "/n/home/.config/other/x"));
        let p = cfg(&[], None).profile(Path::new("/n/proj"), &env("/n/home"));
        assert!(!p.contains("(literal \"/n/home\")"), "directories outside the writable paths need no extra rule:\n{p}");
    }

    #[test]
    fn credentials_are_unreadable() {
        let p = cfg(&[], None).profile(Path::new("/n/proj"), &env("/n/home"));
        let readable = |path: &str| permits(&p, "read", path);
        for bad in [
            "/n/home/.ssh/id_rsa", "/n/home/.ssh/id_ed25519", "/n/home/.aws/credentials", "/n/home/.netrc", "/n/home/.git-credentials", "/n/home/.config/gh/hosts.yml",
            "/n/home/.gnupg/private-keys-v1.d/x.key", "/n/home/Library/Keychains/login.keychain", "/n/home/.cargo/credentials.toml",
            "/n/home/Library/Application Support/moco/auth.json", "/n/home/Library/Application Support/moco/mcp-auth.json",
        ] {
            assert!(!readable(bad), "{bad}\n{p}");
        }
        for ok in [
            "/n/home/.ssh/config", "/n/home/.ssh/known_hosts", "/n/home/.gitconfig", "/n/home/.cargo/registry/src/x/lib.rs",
            "/n/home/Library/Application Support/moco/data/tool-output/shl_1.txt", "/n/home/Library/Application Support/moco/config/skills/x/SKILL.md", "/n/proj/src/main.rs",
        ] {
            assert!(readable(ok), "{ok}\n{p}");
        }
    }

    #[test]
    fn a_data_dir_above_the_project_is_not_denied() {
        let e = ProfileEnv { app_dir: "/n/work".into(), ..env("/n/home") };
        let p = cfg(&[], None).profile(Path::new("/n/work/proj"), &e);
        assert!(writable(&p, "/n/work/proj/src/main.rs") && !writable(&p, "/n/work/proj/.git/hooks/pre-commit"), "{p}");
        assert!(!writable(&p, "/n/work/config/moco.json") && !permits(&p, "read", "/n/work/auth.json"), "{p}");
    }

    #[test]
    fn network_off_denies_outbound_except_localhost() {
        let p = cfg(&[], Some(false)).profile(Path::new("/nonexistent/p"), &env("/nonexistent/h"));
        assert!(p.ends_with("(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n"), "{p}");
        let on = cfg(&[], Some(true)).profile(Path::new("/nonexistent/p"), &env("/nonexistent/h"));
        assert!(!on.contains("network"));
    }

    #[test]
    fn paths_are_escaped_and_symlinks_resolved() {
        let p = cfg(&[], None).profile(Path::new("/nonexistent/a\"b\\c"), &env("/nonexistent/h"));
        assert!(p.contains(r#"(subpath "/nonexistent/a\"b\\c")"#) && p.contains(r#"(subpath "/nonexistent/a\"b\\c/.git/hooks")"#), "{p}");
        assert!(!writable(&p, "/nonexistent/a\"b\\c/.git/config") && writable(&p, "/nonexistent/a\"b\\c/x"));
        let dir = std::env::temp_dir().join(format!("moco-sbx-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real/.git/hooks")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let p = cfg(&[], None).profile(&dir.join("link"), &env("/nonexistent/h"));
        let real = dir.join("real").canonicalize().unwrap();
        assert!(p.contains(&format!("(subpath \"{}\")", dir.join("link").display())) && p.contains(&format!("(subpath \"{}\")", real.display())), "{p}");
        assert!(!writable(&p, &format!("{}/.git/hooks/pre-commit", real.display())) && !writable(&p, &real.display().to_string()), "the resolved form is protected too\n{p}");
        assert!(!writable(&p, &format!("{}/.moco/moco.json", real.display())), "also where nothing exists yet\n{p}");
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
        let profile = c.profile(&project, &ProfileEnv::current());
        let run = |cmd: &str| std::process::Command::new(SANDBOX_EXEC).args(["-p", &profile, "/bin/sh", "-c", cmd]).current_dir(&project).output().unwrap();
        let bad = run(&format!("touch {}", outside.display()));
        assert!(!bad.status.success() && looks_denied(&String::from_utf8_lossy(&bad.stderr)), "{:?}", bad);
        assert!(!outside.exists());
        assert!(run("touch inside").status.success());
        assert!(project.join("inside").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    /// The profile under the real `sandbox-exec`: git still commits, while hooks, git config, `.moco`, renaming the
    /// project away and reading credentials are refused. The git part is skipped where there is no git (Lion without
    /// the command line tools).
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_git_working_but_protects_hooks_config_and_credentials() {
        let base = std::env::temp_dir().join(format!("moco-sbx-git-{}", std::process::id()));
        let (project, home, app) = (base.join("proj"), base.join("home"), base.join("app"));
        for d in [&project, &home.join(".ssh"), &app] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".ssh/id_rsa"), "SECRET KEY").unwrap();
        std::fs::write(home.join(".ssh/config"), "Host x\n").unwrap();
        std::fs::write(app.join("auth.json"), "{\"anthropic\":{\"key\":\"SECRET\"}}").unwrap();
        let e = ProfileEnv { home: home.clone(), tmpdir: None, app_dir: app.clone(), moco_config: None };
        let profile = SandboxConfig { enabled: true, ..Default::default() }.profile(&project, &e);
        let run = |cmd: &str| std::process::Command::new(SANDBOX_EXEC).args(["-p", &profile, "/bin/sh", "-c", cmd]).current_dir(&project).output().unwrap();
        let ok = |cmd: &str| {
            let o = run(cmd);
            assert!(o.status.success(), "{cmd} should work: {o:?}\n{profile}");
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let denied = |cmd: &str| {
            let o = run(cmd);
            assert!(!o.status.success(), "{cmd} should be refused: {o:?}\n{profile}");
        };
        let has_git = std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success());
        if has_git {
            assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&project).status().unwrap().success());
            let git = "git -c user.name=t -c user.email=t@example.com";
            ok(&format!("echo one > a.txt && {git} add a.txt && {git} commit -q -m one && echo two >> a.txt && {git} commit -q -am two"));
            assert_eq!(ok("git log --oneline | wc -l").trim(), "2");
            ok("git status --short && git diff HEAD~1 --stat && git branch side && git checkout -q side && git checkout -q -");
            denied("echo 'exit 0' > .git/hooks/pre-commit");
            assert!(!project.join(".git/hooks/pre-commit").exists());
            denied("git config core.fsmonitor 'touch pwned'");
            assert!(!std::fs::read_to_string(project.join(".git/config")).unwrap().contains("fsmonitor"));
            denied("mv .git .git-old");
            denied("rm -rf .git/hooks");
        }
        denied("mkdir .moco");
        denied("echo '{}' > moco.json");
        denied(&format!("mv {} {}", project.display(), base.join("moved").display()));
        assert!(project.exists());
        ok("mkdir -p src && echo x > src/x.txt");
        denied(&format!("cat {}", home.join(".ssh/id_rsa").display()));
        denied(&format!("cat {}", app.join("auth.json").display()));
        assert_eq!(ok(&format!("cat {}", home.join(".ssh/config").display())), "Host x\n");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The whole profile, so that a change to it shows in review.
    const GOLDEN: &str = r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath "/nonexistent/proj")
  (subpath "/nonexistent/tmp")
  (subpath "/private/tmp")
  (subpath "/private/var/folders")
  (subpath "/nonexistent/home/.cargo/.package-cache")
  (subpath "/nonexistent/home/.cargo/.global-cache")
  (subpath "/nonexistent/home/.cargo/.global-cache-journal")
  (subpath "/nonexistent/home/.cargo/.global-cache-wal")
  (subpath "/nonexistent/home/.cargo/.global-cache-shm")
  (subpath "/nonexistent/home/.npm")
  (subpath "/nonexistent/home/Library/Caches")
  (subpath "/nonexistent/home/.cache")
  (subpath "/nonexistent/home/.cargo-extra")
  (subpath "/opt/out")
  (literal "/dev/null")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
(deny file-write*
  (subpath "/nonexistent/proj/.git/hooks")
  (subpath "/nonexistent/proj/.git/config")
  (subpath "/nonexistent/proj/.git/config.worktree")
  (subpath "/nonexistent/proj/.git/commondir")
  (subpath "/nonexistent/proj/.git/modules")
  (subpath "/nonexistent/proj/.git/worktrees")
  (subpath "/nonexistent/proj/.moco")
  (subpath "/nonexistent/proj/.claude")
  (subpath "/nonexistent/proj/.agents")
  (subpath "/nonexistent/proj/.vscode")
  (subpath "/nonexistent/proj/moco.json")
  (subpath "/nonexistent/proj/moco.jsonc")
  (subpath "/nonexistent/home/.profile")
  (subpath "/nonexistent/home/.bashrc")
  (subpath "/nonexistent/home/.bash_profile")
  (subpath "/nonexistent/home/.bash_login")
  (subpath "/nonexistent/home/.bash_logout")
  (subpath "/nonexistent/home/.zshrc")
  (subpath "/nonexistent/home/.zshenv")
  (subpath "/nonexistent/home/.zprofile")
  (subpath "/nonexistent/home/.zlogin")
  (subpath "/nonexistent/home/.zlogout")
  (subpath "/nonexistent/home/.cshrc")
  (subpath "/nonexistent/home/.tcshrc")
  (subpath "/nonexistent/home/.login")
  (subpath "/nonexistent/home/.logout")
  (subpath "/nonexistent/home/.config/fish")
  (subpath "/nonexistent/home/.MacOSX")
  (subpath "/nonexistent/home/Library/LaunchAgents")
  (subpath "/nonexistent/home/Library/Preferences/com.apple.loginitems.plist")
  (subpath "/nonexistent/home/.ssh")
  (subpath "/nonexistent/home/.gitconfig")
  (subpath "/nonexistent/home/.config/git")
  (subpath "/nonexistent/home/.cargo/config")
  (subpath "/nonexistent/home/.cargo/config.toml")
  (subpath "/nonexistent/home/.cargo/bin")
  (subpath "/nonexistent/home/.cargo/registry")
  (subpath "/nonexistent/home/.cargo/git")
  (subpath "/nonexistent/home/.rustup")
  (subpath "/nonexistent/home/.local/bin")
  (subpath "/nonexistent/home/bin")
  (subpath "/nonexistent/home/.npmrc")
  (subpath "/nonexistent/home/.yarnrc")
  (subpath "/nonexistent/home/Library/Python")
  (subpath "/nonexistent/home/.cache/pre-commit")
  (subpath "/nonexistent/home/.vimrc")
  (subpath "/nonexistent/home/.vim")
  (subpath "/nonexistent/home/.config/nvim")
  (subpath "/nonexistent/home/.emacs")
  (subpath "/nonexistent/home/.emacs.d")
  (subpath "/nonexistent/home/.tmux.conf")
  (subpath "/nonexistent/home/Library/Application Support/moco")
  (subpath "/nonexistent/home/Library/Application Support/moco/config")
  (subpath "/nonexistent/home/Library/Application Support/moco/auth.json")
  (subpath "/nonexistent/home/Library/Application Support/moco/mcp-auth.json")
  (literal "/nonexistent/proj/.git")
  (literal "/nonexistent/proj")
  (literal "/nonexistent/home/.cache"))
(deny file-read-data
  (subpath "/nonexistent/home/.ssh")
  (subpath "/nonexistent/home/.aws")
  (subpath "/nonexistent/home/.azure")
  (subpath "/nonexistent/home/.config/gcloud")
  (subpath "/nonexistent/home/.config/gh")
  (subpath "/nonexistent/home/.gnupg")
  (subpath "/nonexistent/home/.netrc")
  (subpath "/nonexistent/home/.git-credentials")
  (subpath "/nonexistent/home/.config/git/credentials")
  (subpath "/nonexistent/home/.cargo/credentials")
  (subpath "/nonexistent/home/.cargo/credentials.toml")
  (subpath "/nonexistent/home/.pypirc")
  (subpath "/nonexistent/home/Library/Keychains")
  (subpath "/nonexistent/home/Library/Application Support/moco/auth.json")
  (subpath "/nonexistent/home/Library/Application Support/moco/mcp-auth.json"))
(allow file-read-data
  (literal "/nonexistent/home/.ssh/config")
  (literal "/nonexistent/home/.ssh/known_hosts"))
"#;
}
