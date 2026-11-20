//! `moco --version` says which build this is (issue archive#94): the version, then the commit and what it was built for.
use std::process::Command;

#[test]
fn version_prints_the_build_and_its_commit() {
    let out = Command::new(env!("CARGO_BIN_EXE_moco")).arg("--version").env("MOCO_HOME", std::env::temp_dir().join("moco-version-test")).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], format!("moco {}", moco_core::version::VERSION), "the first line is what scripts/install.sh shows");
    assert!(moco_core::version::VERSION.starts_with(moco_core::version::RELEASE), "{}", moco_core::version::VERSION);
    assert_eq!(lines[1], format!("commit {}, uncommitted changes: {}", moco_core::version::COMMIT, moco_core::version::UNCOMMITTED));
    assert!(lines[2].starts_with("built for "), "{text}");
}
