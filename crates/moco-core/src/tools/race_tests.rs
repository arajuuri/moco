//! A symlink swapped in between a file tool's permission check and its use of the file is not followed (archive#165). The
//! swap happens at `nofollow::between_check_and_use`, deterministically, and each case runs with `openat` and with the
//! full-path fallback of macOS 10.7 to 10.9, and with the project given by its own name and through a symlinked
//! parent (as `/tmp` -> `/private/tmp` on macOS).
use super::testutil::*;
use super::*;
use crate::nofollow::{both_modes, on_next_use};
use std::os::unix::fs::symlink;

/// A project with `a.txt` and `sub/b.txt`, and `<env>/outside` holding `secret.txt` and `b.txt`.
fn setup() -> (Env, PathBuf) {
    let env = Env::new();
    env.file("a.txt", b"mine\n");
    env.file("sub/b.txt", b"mine\n");
    let outside = env.dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "SECRET\n").unwrap();
    std::fs::write(outside.join("b.txt"), "SECRET\n").unwrap();
    symlink(&env.dir, env.dir.join("via")).unwrap();
    (env, outside)
}

/// The tools' context with the project as `<env>/work`, or `linked` as `<env>/via/work` (`via` -> `<env>`).
fn ctx(env: &Env, linked: bool) -> ToolCtx<'_> {
    let work = if linked { env.dir.join("via/work") } else { env.work() };
    ToolCtx { cwd: work.clone(), project_root: work, ..env.ctx() }
}

/// After the check, `a.txt` becomes a link to the outside secret.
fn swap_file(env: &Env, outside: &Path) {
    let (link, target) = (env.work().join("a.txt"), outside.join("secret.txt"));
    on_next_use(move || {
        std::fs::remove_file(&link).unwrap();
        symlink(target, link).unwrap();
    });
}

/// After the check, the folder `rel` (moved aside, or an empty one removed) becomes a link to the outside folder.
fn swap_dir(env: &Env, rel: &str, outside: &Path) {
    let (dir, target) = (env.work().join(rel), outside.to_path_buf());
    std::fs::create_dir_all(&dir).unwrap();
    on_next_use(move || {
        let mut aside = dir.clone().into_os_string();
        aside.push(".moved");
        std::fs::rename(&dir, aside).unwrap();
        symlink(target, dir).unwrap();
    });
}

fn refused(r: Result<ToolOutput, ToolError>) -> String {
    let e = r.unwrap_err().message();
    assert!(e.contains("symlink") && !e.contains("SECRET"), "{e}");
    e
}

fn outside_untouched(outside: &Path) {
    assert_eq!(std::fs::read_to_string(outside.join("secret.txt")).unwrap(), "SECRET\n");
    assert_eq!(std::fs::read_to_string(outside.join("b.txt")).unwrap(), "SECRET\n");
    let mut names: Vec<_> = std::fs::read_dir(outside).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["b.txt", "secret.txt"], "nothing created outside");
}

fn no_external_prompt(env: &Env) {
    assert!(env.perm.asked().iter().all(|r| r.action != "external_directory"), "{:?}", env.perm.asked());
}

#[test]
fn read_does_not_follow_a_link_swapped_in_after_the_check() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        swap_file(&env, &outside);
        refused(read::Read.run(&json!({"path":"a.txt"}), &ctx(&env, linked)));
        swap_dir(&env, "sub", &outside);
        refused(read::Read.run(&json!({"path":"sub/b.txt"}), &ctx(&env, linked)));
        // A folder listing too.
        swap_dir(&env, "list", &outside);
        let e = read::Read.run(&json!({"path":"list"}), &ctx(&env, linked)).unwrap_err().message();
        assert!(e.contains("symlink") && !e.contains("secret.txt"), "{e}");
        // Only project paths were ever checked: the swaps never got an external_directory prompt past the user.
        no_external_prompt(&env);
    });
}

#[test]
fn write_does_not_follow_a_link_swapped_in_after_the_check() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        swap_file(&env, &outside);
        refused(write::Write.run(&json!({"path":"a.txt","content":"pwned"}), &ctx(&env, linked)));
        assert!(std::fs::symlink_metadata(env.work().join("a.txt")).unwrap().file_type().is_symlink(), "the link is not replaced either");
        swap_dir(&env, "sub", &outside);
        refused(write::Write.run(&json!({"path":"sub/b.txt","content":"pwned"}), &ctx(&env, linked)));
        // A new file in a new folder under a swapped one: no folder is created through the link.
        swap_dir(&env, "fresh", &outside);
        refused(write::Write.run(&json!({"path":"fresh/new/c.txt","content":"pwned"}), &ctx(&env, linked)));
        outside_untouched(&outside);
        no_external_prompt(&env);
    });
}

#[test]
fn edit_does_not_follow_a_link_swapped_in_after_the_check() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        swap_file(&env, &outside);
        refused(edit::Edit.run(&json!({"path":"a.txt","oldString":"SECRET","newString":"pwned"}), &ctx(&env, linked)));
        swap_dir(&env, "sub", &outside);
        refused(edit::Edit.run(&json!({"path":"sub/b.txt","oldString":"SECRET","newString":"pwned"}), &ctx(&env, linked)));
        outside_untouched(&outside);
        no_external_prompt(&env);
    });
}

#[test]
fn patch_does_not_follow_a_link_swapped_in_after_the_check() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        let run = |body: &str| patch::Patch.run(&json!({"patchText": format!("*** Begin Patch\n{body}\n*** End Patch")}), &ctx(&env, linked));
        swap_file(&env, &outside);
        refused(run("*** Update File: a.txt\n@@\n-SECRET\n+pwned"));
        swap_dir(&env, "sub", &outside);
        refused(run("*** Update File: sub/b.txt\n@@\n-SECRET\n+pwned"));
        swap_dir(&env, "fresh", &outside);
        refused(run("*** Add File: fresh/c.txt\n+pwned"));
        outside_untouched(&outside);
        no_external_prompt(&env);
    });
}

#[test]
fn patch_delete_removes_a_symlink_not_its_target() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        symlink(env.work().join("a.txt"), env.work().join("in.txt")).unwrap();
        symlink(outside.join("secret.txt"), env.work().join("out.txt")).unwrap();
        symlink(outside.join("gone.txt"), env.work().join("dangling.txt")).unwrap();
        let o = patch::Patch.run(&json!({"patchText":"*** Begin Patch\n*** Delete File: in.txt\n*** Delete File: out.txt\n*** Delete File: dangling.txt\n*** End Patch"}), &ctx(&env, linked)).unwrap();
        assert!(o.text.contains("D in.txt") && o.text.contains("D out.txt") && o.text.contains("D dangling.txt"), "{}", o.text);
        for link in ["in.txt", "out.txt", "dangling.txt"] {
            assert!(std::fs::symlink_metadata(env.work().join(link)).is_err(), "{link} is removed");
        }
        assert_eq!(std::fs::read_to_string(env.work().join("a.txt")).unwrap(), "mine\n", "the target in the project stays");
        outside_untouched(&outside);
        // Removing a link touches only the project: what it pointed to is not asked about.
        no_external_prompt(&env);
        let edits: Vec<_> = env.perm.asked().into_iter().filter(|r| r.action == "edit").flat_map(|r| r.resources).collect();
        assert_eq!(edits, ["in.txt", "out.txt", "dangling.txt"]);
        // A plain file is still deleted, and a link swapped in for it after the check is removed, not its target.
        patch::Patch.run(&json!({"patchText":"*** Begin Patch\n*** Delete File: sub/b.txt\n*** End Patch"}), &ctx(&env, linked)).unwrap();
        assert!(!env.work().join("sub/b.txt").exists());
        swap_file(&env, &outside);
        patch::Patch.run(&json!({"patchText":"*** Begin Patch\n*** Delete File: a.txt\n*** End Patch"}), &ctx(&env, linked)).unwrap();
        assert!(std::fs::symlink_metadata(env.work().join("a.txt")).is_err());
        outside_untouched(&outside);
    });
}

#[test]
fn permission_resources_do_not_depend_on_how_the_project_is_reached() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        symlink(env.work().join("a.txt"), env.work().join("alias.txt")).unwrap();
        symlink(outside.join("secret.txt"), env.work().join("out.txt")).unwrap();
        let c = ctx(&env, linked);
        read::Read.run(&json!({"path":"a.txt"}), &c).unwrap();
        read::Read.run(&json!({"path":"alias.txt"}), &c).unwrap();
        write::Write.run(&json!({"path":"sub/new.txt","content":"n"}), &c).unwrap();
        edit::Edit.run(&json!({"path":"alias.txt","oldString":"mine","newString":"x"}), &c).unwrap();
        patch::Patch.run(&json!({"patchText":"*** Begin Patch\n*** Update File: sub/b.txt\n@@\n-mine\n+y\n*** Delete File: out.txt\n*** Delete File: alias.txt\n*** End Patch"}), &c).unwrap();
        let asked: Vec<(String, Vec<String>)> = env.perm.asked().into_iter().map(|r| (r.action, r.resources)).collect();
        let want = |a: &str, r: &[&str]| (a.to_string(), r.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            asked,
            [
                want("read", &["a.txt"]),
                want("read", &["alias.txt", "a.txt"]),
                want("edit", &["sub/new.txt"]),
                want("edit", &["alias.txt", "a.txt"]),
                want("read", &["alias.txt", "a.txt"]),
                want("edit", &["sub/b.txt", "out.txt", "alias.txt"]),
                want("read", &["sub/b.txt"]),
            ],
            "linked: {linked}"
        );
        outside_untouched(&outside);
    });
}

#[test]
fn files_reached_through_links_made_before_the_check_still_work() {
    both_modes(|| for linked in [false, true] {
        let (env, outside) = setup();
        symlink(env.work().join("a.txt"), env.work().join("alias.txt")).unwrap();
        symlink(env.work().join("sub"), env.work().join("subalias")).unwrap();
        symlink(&outside, env.work().join("ext")).unwrap();
        assert_eq!(read::Read.run(&json!({"path":"alias.txt"}), &ctx(&env, linked)).unwrap().text, "1: mine");
        edit::Edit.run(&json!({"path":"subalias/b.txt","oldString":"mine","newString":"edited"}), &ctx(&env, linked)).unwrap();
        write::Write.run(&json!({"path":"subalias/deep/new.txt","content":"n"}), &ctx(&env, linked)).unwrap();
        assert_eq!(std::fs::read_to_string(env.work().join("sub/b.txt")).unwrap(), "edited\n");
        assert_eq!(std::fs::read_to_string(env.work().join("sub/deep/new.txt")).unwrap(), "n");
        no_external_prompt(&env);
        // Outside the project the target is asked about and then used.
        write::Write.run(&json!({"path":"ext/secret.txt","content":"allowed\n"}), &ctx(&env, linked)).unwrap();
        assert_eq!(std::fs::read_to_string(outside.join("secret.txt")).unwrap(), "allowed\n");
        assert!(env.perm.asked().iter().any(|r| r.action == "external_directory"));
    });
}
