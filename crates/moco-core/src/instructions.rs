//! Project instructions (docs/08 "Instructions"): `AGENTS.md` files, with `CLAUDE.md` as a fallback
//! in directories that have no `AGENTS.md` (a moco extension, so existing Claude Code projects work).
use crate::tools::project_root_of;
use std::path::{Path, PathBuf};

const MAX_BYTES: usize = 64 * 1024;
const NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub path: PathBuf,
    pub text: String,
}

fn in_dir(dir: &Path) -> Option<PathBuf> {
    NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

fn expand(p: &str, cwd: &Path) -> PathBuf {
    let p = match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    };
    if p.is_absolute() { p } else { cwd.join(p) }
}

/// What `load` found: the instructions, and project files it refused because they resolve outside the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub items: Vec<Instruction>,
    /// Project instruction files (as found, before resolving) whose real path is outside the project root,
    /// e.g. a committed `AGENTS.md` symlink to `~/.aws/credentials`. They are not read.
    pub rejected: Vec<PathBuf>,
}

/// Instruction files found for a session, in precedence order (later wins when they conflict):
/// global, project (outermost directory first, ending at the working directory), configured extras,
/// then files discovered lazily by `read`.
///
/// Files that come from the project (discovered `AGENTS.md`/`CLAUDE.md`, lazily discovered ones and relative
/// `instructions` entries, the only kind a project config can name) must resolve, symlinks included, inside the
/// project root; the others are listed in `Loaded::rejected` instead of being sent to the model (archive#146). The global
/// file and absolute or `~/` entries can only come from the user's own config and are trusted wherever they point.
/// The file read is the resolved path that was checked.
pub fn load(app_dir: &Path, cwd: &Path, extra: &[String], lazy: &[PathBuf]) -> Loaded {
    // (path as found, trusted to resolve outside the project)
    let mut paths: Vec<(PathBuf, bool)> = vec![];
    let global = app_dir.join("config/AGENTS.md");
    if global.is_file() {
        paths.push((global, true));
    }
    let root = project_root_of(cwd);
    let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|d| d.starts_with(&root)).collect();
    dirs.reverse();
    paths.extend(dirs.into_iter().filter_map(in_dir).map(|p| (p, false)));
    for e in extra {
        let p = expand(e, cwd);
        if p.is_file() {
            paths.push((p, e.starts_with("~/") || Path::new(e).is_absolute()));
        }
    }
    paths.extend(lazy.iter().filter(|p| p.is_file()).map(|p| (p.clone(), false)));

    let real_root = root.canonicalize().unwrap_or(root);
    let mut out = Loaded::default();
    let mut seen = std::collections::HashSet::new();
    for (path, trusted) in paths {
        let Ok(real) = path.canonicalize() else { continue };
        if !trusted && !real.starts_with(&real_root) {
            if !out.rejected.contains(&path) {
                out.rejected.push(path);
            }
            continue;
        }
        if !seen.insert(real.clone()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&real) else { continue };
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.len() > MAX_BYTES {
            let mut cut = MAX_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n[truncated]");
        }
        if !text.trim().is_empty() {
            out.items.push(Instruction { path, text });
        }
    }
    out
}

/// Instruction files in directories between a file just read and the working directory
/// (exclusive of the working directory, whose files are already loaded), innermost last.
pub fn discover_between(file: &Path, cwd: &Path) -> Vec<PathBuf> {
    let Some(start) = file.parent().filter(|d| d.starts_with(cwd) && *d != cwd) else { return vec![] };
    let mut found: Vec<PathBuf> = start.ancestors().take_while(|d| *d != cwd).filter_map(in_dir).collect();
    found.reverse();
    found
}

pub fn render(items: &[Instruction]) -> String {
    items.iter().map(|i| format!("Instructions from: {}\n{}\n", i.path.display(), i.text.trim_end())).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(crate::id::new_id("moco-ins"));
        std::fs::create_dir_all(root.join("app/config")).unwrap();
        std::fs::create_dir_all(root.join("proj/.git")).unwrap();
        std::fs::create_dir_all(root.join("proj/pkg/deep")).unwrap();
        root
    }

    #[test]
    fn order_global_then_outer_to_inner_with_claude_fallback() {
        let r = tree();
        std::fs::write(r.join("app/config/AGENTS.md"), "global").unwrap();
        std::fs::write(r.join("proj/AGENTS.md"), "root rules").unwrap();
        std::fs::write(r.join("proj/CLAUDE.md"), "ignored: AGENTS.md wins in the same dir").unwrap();
        std::fs::write(r.join("proj/pkg/CLAUDE.md"), "pkg rules").unwrap();
        let got = load(&r.join("app"), &r.join("proj/pkg"), &[], &[]).items;
        let texts: Vec<_> = got.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["global", "root rules", "pkg rules"]);
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn does_not_leave_the_project_root() {
        let r = tree();
        std::fs::write(r.join("AGENTS.md"), "above the repo").unwrap();
        std::fs::write(r.join("proj/AGENTS.md"), "inside").unwrap();
        let got = load(&r.join("app"), &r.join("proj/pkg"), &[], &[]).items;
        assert_eq!(got.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["inside"]);
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn extras_lazy_dedup_and_truncation() {
        let r = tree();
        std::fs::write(r.join("proj/AGENTS.md"), "root").unwrap();
        std::fs::write(r.join("proj/extra.md"), "extra").unwrap();
        std::fs::write(r.join("proj/pkg/deep/AGENTS.md"), "x".repeat(70_000)).unwrap();
        let lazy = vec![r.join("proj/pkg/deep/AGENTS.md"), r.join("proj/AGENTS.md")];
        let got = load(&r.join("app"), &r.join("proj"), &["extra.md".into(), "missing.md".into()], &lazy).items;
        let names: Vec<_> = got.iter().map(|i| i.path.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, ["AGENTS.md", "extra.md", "AGENTS.md"], "root file not repeated");
        assert!(got[2].text.ends_with("[truncated]") && got[2].text.len() < 66_000);
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn lazy_discovery_between_file_and_cwd() {
        let r = tree();
        let cwd = r.join("proj");
        std::fs::write(cwd.join("AGENTS.md"), "root").unwrap();
        std::fs::write(cwd.join("pkg/AGENTS.md"), "pkg").unwrap();
        std::fs::write(cwd.join("pkg/deep/CLAUDE.md"), "deep").unwrap();
        let found = discover_between(&cwd.join("pkg/deep/f.rs"), &cwd);
        assert_eq!(found, [cwd.join("pkg/AGENTS.md"), cwd.join("pkg/deep/CLAUDE.md")]);
        assert!(discover_between(&cwd.join("f.rs"), &cwd).is_empty(), "files at the root add nothing");
        assert!(discover_between(&r.join("elsewhere/f.rs"), &cwd).is_empty());
        let _ = std::fs::remove_dir_all(r);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_inside_the_project_still_load() {
        use std::os::unix::fs::symlink;
        let r = tree();
        std::fs::create_dir_all(r.join("proj/docs")).unwrap();
        std::fs::write(r.join("proj/docs/rules.md"), "shared rules").unwrap();
        symlink(r.join("proj/docs/rules.md"), r.join("proj/AGENTS.md")).unwrap();
        // A symlinked parent that stays inside the project.
        std::fs::write(r.join("proj/docs/CLAUDE.md"), "docs rules").unwrap();
        symlink("docs", r.join("proj/alias")).unwrap();
        let got = load(&r.join("app"), &r.join("proj"), &["alias/rules.md".into()], &[r.join("proj/alias/CLAUDE.md")]);
        assert!(got.rejected.is_empty(), "{:?}", got.rejected);
        let texts: Vec<_> = got.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["shared rules", "docs rules"], "the extra is the same file as AGENTS.md");
        let _ = std::fs::remove_dir_all(r);
    }

    #[cfg(unix)]
    #[test]
    fn project_symlinks_out_of_the_project_are_not_read() {
        use std::os::unix::fs::symlink;
        let r = tree();
        std::fs::create_dir_all(r.join("outside/dir")).unwrap();
        std::fs::write(r.join("outside/secret"), "FAKE_SECRET").unwrap();
        std::fs::write(r.join("outside/dir/AGENTS.md"), "FAKE_SECRET_DIR").unwrap();
        let proj = r.join("proj");
        // Final component: the discovered AGENTS.md and a relative configured entry.
        symlink(r.join("outside/secret"), proj.join("AGENTS.md")).unwrap();
        symlink("../outside/secret", proj.join("local.txt")).unwrap();
        // Parent component: a directory symlink whose AGENTS.md is discovered lazily and found from a nested cwd.
        symlink(r.join("outside/dir"), proj.join("pkg/linked")).unwrap();
        let lazy = discover_between(&proj.join("pkg/linked/f.rs"), &proj);
        assert_eq!(lazy, [proj.join("pkg/linked/AGENTS.md")]);
        let got = load(&r.join("app"), &proj, &["local.txt".into()], &lazy);
        assert!(got.items.is_empty(), "{:?}", got.items);
        assert_eq!(got.rejected, [proj.join("AGENTS.md"), proj.join("local.txt"), proj.join("pkg/linked/AGENTS.md")]);
        let nested = load(&r.join("app"), &proj.join("pkg/linked"), &[], &[]);
        assert!(nested.items.is_empty() && nested.rejected.len() == 2, "{nested:?}");
        let _ = std::fs::remove_dir_all(r);
    }

    #[cfg(unix)]
    #[test]
    fn user_config_paths_outside_the_project_stay_trusted() {
        use std::os::unix::fs::symlink;
        let r = tree();
        std::fs::create_dir_all(r.join("outside")).unwrap();
        std::fs::write(r.join("outside/notes.md"), "my notes").unwrap();
        std::fs::write(r.join("outside/global.md"), "global").unwrap();
        symlink(r.join("outside/global.md"), r.join("app/config/AGENTS.md")).unwrap();
        let abs = r.join("outside/notes.md").to_string_lossy().into_owned();
        let got = load(&r.join("app"), &r.join("proj"), &[abs], &[]);
        assert!(got.rejected.is_empty(), "{:?}", got.rejected);
        assert_eq!(got.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["global", "my notes"]);
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn rendering() {
        let s = render(&[Instruction { path: "/a/AGENTS.md".into(), text: "be nice\n".into() }]);
        assert_eq!(s, "Instructions from: /a/AGENTS.md\nbe nice\n");
    }
}
