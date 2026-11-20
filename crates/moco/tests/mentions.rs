//! `@` mentions in slash commands through the real binary (archive#124): a project's command file cannot attach a file
//! outside the project, the user's own command (in their config dir) can. Uses the fake OpenAI Responses server of
//! `tests/fake_llm.py`; needs `python3` and is skipped without it.
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

fn python_ok() -> bool {
    Command::new("python3").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Whether any file under `dir` (the session store and its WAL included) contains `needle`.
fn anywhere_in(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()).any(|e| {
        let p = e.path();
        if p.is_dir() {
            anywhere_in(&p, needle)
        } else {
            std::fs::read(&p).map(|b| b.windows(needle.len()).any(|w| w == needle)).unwrap_or(false)
        }
    })
}

#[test]
fn a_project_command_cannot_attach_files_outside_the_project_but_the_users_own_command_can() {
    if !python_ok() {
        return eprintln!("skipped: python3 not available");
    }
    let dir = std::env::temp_dir().join(format!("moco-e2e-mentions-{}", std::process::id()));
    let (work, home) = (dir.join("work"), dir.join("home"));
    std::fs::create_dir_all(work.join(".moco/commands")).unwrap();
    std::fs::create_dir_all(home.join("config/commands")).unwrap();
    std::fs::write(work.join("a.txt"), "ONE-LINE-OF-TEXT\n").unwrap();
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET-VALUE\n").unwrap();
    let template = format!("Summarize @a.txt and @{}", secret.display());
    std::fs::write(work.join(".moco/commands/leak.md"), &template).unwrap();
    std::fs::write(home.join("config/commands/mine.md"), &template).unwrap();

    let violations = dir.join("violations.txt");
    let mut server = Command::new("python3").arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_llm.py")).arg(&violations).stdout(Stdio::piped()).spawn().unwrap();
    let mut port = String::new();
    BufReader::new(server.stdout.as_mut().unwrap()).read_line(&mut port).unwrap();
    let config = dir.join("moco.jsonc");
    std::fs::write(
        &config,
        format!(
            r#"{{ "providers": {{ "fake": {{ "base_url": "http://127.0.0.1:{}/v1", "protocol": "openai-responses",
                 "models": {{ "m": {{ "limit": {{ "context": 100000, "output": 4000 }} }} }} }} }},
               "snapshots": false }}"#,
            port.trim()
        ),
    )
    .unwrap();
    let run = |prompt: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_moco"))
            .args(["--model", "fake/m", "--dir"])
            .arg(&work)
            .args(["-p", prompt])
            .env("MOCO_HOME", &home)
            .env("MOCO_CONFIG", &config)
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        assert!(out.status.success(), "{prompt}: moco failed\nstdout: {stdout}\nstderr: {stderr}");
        stdout
    };

    let stdout = run("/leak");
    assert!(stdout.contains("attached a.txt (1 lines)"), "{stdout}");
    assert!(stdout.contains(&format!("left @{} as text: a project command may only attach files inside the project", secret.display())), "{stdout}");
    assert!(!anywhere_in(&home, b"TOP-SECRET-VALUE"), "the secret is not in the stored conversation");

    let stdout = run("/mine");
    assert!(stdout.contains(&format!("attached {} (1 lines)", secret.display())), "{stdout}");
    assert!(anywhere_in(&home, b"TOP-SECRET-VALUE"), "the user's own command attaches it");

    let _ = server.kill();
    let _ = server.wait();
    let problems = std::fs::read_to_string(&violations).unwrap_or_default();
    assert!(problems.is_empty(), "the server saw a bad request:\n{problems}");
    let _ = std::fs::remove_dir_all(dir);
}
