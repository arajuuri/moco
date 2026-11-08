//! `moco permissions`: listing and forgetting saved "always allow" rules without starting a session.
use moco_core::permission::{Effect, Rule};
use moco_core::store::Store;
use std::path::{Path, PathBuf};
use std::process::Command;

fn moco(home: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_moco")).args(args).current_dir(cwd).env("MOCO_HOME", home).env_remove("MOCO_LOG").env_remove("MOCO_CONFIG").output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("moco-permcli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("home/data")).unwrap();
    std::fs::create_dir_all(d.join("proj")).unwrap();
    std::fs::create_dir_all(d.join("elsewhere")).unwrap();
    d
}

#[test]
fn lists_removes_and_clears_saved_rules_of_a_project() {
    let d = scratch();
    let (home, proj, elsewhere) = (d.join("home"), d.join("proj"), d.join("elsewhere"));
    let db = home.join("data/moco.db");
    let project = {
        let s = Store::open(&db).unwrap();
        let p = s.project_for(&proj.to_string_lossy()).unwrap();
        for r in ["osascript *", "rm -rf *", "cargo test *"] {
            s.save_permission(&p, "shell", r).unwrap();
        }
        s.save_permission(&p, "edit", "*").unwrap();
        p
    };

    let (ok, out, err) = moco(&home, &proj, &["permissions"]);
    assert!(ok, "{err}");
    assert!(out.contains("rules for this project: 4") && out.contains(" 1. shell") && out.contains("osascript *"), "{out}");
    assert!(out.contains("moco permissions remove <number>"), "{out}");

    // By number, by action and pattern (dashes in the pattern are not options), and from another directory with --dir.
    let (ok, out, err) = moco(&home, &proj, &["permissions", "remove", "1"]);
    assert!(ok && out.contains("removed saved rule: shell osascript *"), "{out}{err}");
    let (ok, out, err) = moco(&home, &elsewhere, &["permissions", "--dir", &proj.to_string_lossy(), "remove", "shell", "rm -rf *"]);
    assert!(ok && out.contains("shell rm -rf *"), "{out}{err}");
    let rules = Store::open(&db).unwrap().saved_permissions(&project).unwrap();
    assert_eq!(rules, [Rule::new("shell", "cargo test *", Effect::Allow), Rule::new("edit", "*", Effect::Allow)]);

    // Mistakes fail and change nothing.
    let (ok, _, err) = moco(&home, &proj, &["permissions", "remove", "9"]);
    assert!(!ok && err.contains("no saved rule number 9"), "{err}");
    let (ok, _, err) = moco(&home, &proj, &["permissions", "--bogus"]);
    assert!(!ok && err.contains("unknown option --bogus"), "{err}");

    let (ok, out, _) = moco(&home, &proj, &["permissions", "clear"]);
    assert!(ok && out.contains("removed 2 saved"), "{out}");
    assert!(Store::open(&db).unwrap().saved_permissions(&project).unwrap().is_empty());

    // A directory moco has never seen is reported, not added as a project.
    let (ok, out, _) = moco(&home, &elsewhere, &["permissions"]);
    assert!(ok && out.contains("has not been used"), "{out}");
    assert_eq!(Store::open(&db).unwrap().find_project(&elsewhere.to_string_lossy()).unwrap(), None);
    let _ = std::fs::remove_dir_all(d);
}
