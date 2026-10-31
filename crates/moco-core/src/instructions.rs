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

/// Instruction files found for a session, in precedence order (later wins when they conflict):
/// global, project (outermost directory first, ending at the working directory), configured extras,
/// then files discovered lazily by `read`.
pub fn load(app_dir: &Path, cwd: &Path, extra: &[String], lazy: &[PathBuf]) -> Vec<Instruction> {
    let mut paths: Vec<PathBuf> = vec![];
    let global = app_dir.join("config/AGENTS.md");
    if global.is_file() {
        paths.push(global);
    }
    let root = project_root_of(cwd);
    let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|d| d.starts_with(&root)).collect();
    dirs.reverse();
    paths.extend(dirs.into_iter().filter_map(in_dir));
    paths.extend(extra.iter().map(|e| expand(e, cwd)).filter(|p| p.is_file()));
    paths.extend(lazy.iter().filter(|p| p.is_file()).cloned());

    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .filter_map(|path| {
            let mut text = String::from_utf8_lossy(&std::fs::read(&path).ok()?).into_owned();
            if text.len() > MAX_BYTES {
                let mut cut = MAX_BYTES;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n[truncated]");
            }
            (!text.trim().is_empty()).then_some(Instruction { path, text })
        })
        .collect()
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
        let got = load(&r.join("app"), &r.join("proj/pkg"), &[], &[]);
        let texts: Vec<_> = got.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["global", "root rules", "pkg rules"]);
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn does_not_leave_the_project_root() {
        let r = tree();
        std::fs::write(r.join("AGENTS.md"), "above the repo").unwrap();
        std::fs::write(r.join("proj/AGENTS.md"), "inside").unwrap();
        let got = load(&r.join("app"), &r.join("proj/pkg"), &[], &[]);
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
        let got = load(&r.join("app"), &r.join("proj"), &["extra.md".into(), "missing.md".into()], &lazy);
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

    #[test]
    fn rendering() {
        let s = render(&[Instruction { path: "/a/AGENTS.md".into(), text: "be nice\n".into() }]);
        assert_eq!(s, "Instructions from: /a/AGENTS.md\nbe nice\n");
    }
}
