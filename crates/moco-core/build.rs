//! Records which build this is (issue #94): `git describe` turned into a version such as `0.1.0-12-g1a2b3c4d5e`,
//! plus the target and minimum macOS. No dependencies: crates such as `vergen` may not build on Rust 1.78.
//! The rules are in `src/describe.rs`; "Versions and releases" in CONTRIBUTING.md is the practice.
use std::path::{Path, PathBuf};
use std::process::Command;

include!("src/describe.rs");

const DESCRIBE: &[&str] = &["describe", "--tags", "--match", "v[0-9]*", "--long", "--dirty", "--abbrev=10", "--always"];

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// `git describe` for the workspace, if it is a git checkout of its own (not a copy inside some other repository).
fn from_git(root: &Path) -> Option<String> {
    let top = PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?);
    if top.canonicalize().ok()? != root.canonicalize().ok()? {
        return None;
    }
    // Run again when the commit, the tags or the index change. Sources are watched too, so the uncommitted-changes
    // mark follows edits; an edit outside `crates/` (docs) does not rebuild, which is fine: the binary is the same.
    let abs = |p: String| -> PathBuf {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { root.join(p) }
    };
    let git_dir = abs(git(root, &["rev-parse", "--git-dir"])?);
    let common = git(root, &["rev-parse", "--git-common-dir"]).map(abs).unwrap_or_else(|| git_dir.clone());
    for p in [git_dir.join("HEAD"), git_dir.join("index"), common.join("refs"), common.join("packed-refs"), root.join("crates")] {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    git(root, DESCRIBE)
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.parent().and_then(Path::parent).unwrap_or(&manifest).to_path_buf();
    let release = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/describe.rs");
    println!("cargo:rerun-if-env-changed=MOCO_GIT_DESCRIBE");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    // A copy without `.git` (synced to the Mac, a tarball) can be told the describe output of the real checkout.
    let given = std::env::var("MOCO_GIT_DESCRIBE").ok().filter(|s| !s.trim().is_empty());
    let described = describe_version(given.or_else(|| from_git(&root)).as_deref().unwrap_or(""), &release);
    if let Some(tag) = &described.exact_tag {
        if *tag != release {
            println!("cargo:warning=this commit is tagged v{tag}, but [workspace.package] version is {release}");
        }
    }
    let dirty = match described.dirty {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    let target = std::env::var("TARGET").unwrap_or_default();
    let min_macos = if target.contains("apple") { std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_default() } else { String::new() };
    println!("cargo:rustc-env=MOCO_VERSION={}", described.version);
    println!("cargo:rustc-env=MOCO_COMMIT={}", described.commit);
    println!("cargo:rustc-env=MOCO_UNCOMMITTED={dirty}");
    println!("cargo:rustc-env=MOCO_TARGET={target}");
    println!("cargo:rustc-env=MOCO_MIN_MACOS={min_macos}");
}
