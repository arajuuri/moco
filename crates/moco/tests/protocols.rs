//! End to end: the real `moco` binary talking to a local server that speaks the OpenAI Responses and Gemini protocols
//! (`tests/fake_llm.py`). Covers config -> provider registry -> adapter -> engine -> tool loop -> replay of the
//! provider's private data. Needs `python3`; skipped without it.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

fn python_ok() -> bool {
    Command::new("python3").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn run(protocol: &str, base_path: &str) {
    let dir = std::env::temp_dir().join(format!("moco-e2e-{protocol}-{}", std::process::id()));
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "ONE-LINE-OF-TEXT\n").unwrap();
    let violations = dir.join("violations.txt");

    let mut server = Command::new("python3").arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_llm.py")).arg(&violations).stdout(Stdio::piped()).spawn().unwrap();
    let mut port = String::new();
    BufReader::new(server.stdout.as_mut().unwrap()).read_line(&mut port).unwrap();
    let port = port.trim();

    let config = dir.join("moco.jsonc");
    std::fs::write(
        &config,
        format!(
            r#"{{ "providers": {{ "fake": {{ "base_url": "http://127.0.0.1:{port}{base_path}", "protocol": "{protocol}",
                 "models": {{ "m": {{ "limit": {{ "context": 100000, "output": 4000 }} }} }} }} }},
               "snapshots": false }}"#
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_moco"))
        .args(["--model", "fake/m", "--dir"])
        .arg(&work)
        .args(["-p", "Read a.txt and tell me what it says."])
        .env("MOCO_HOME", dir.join("home"))
        .env("MOCO_CONFIG", &config)
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    let _ = server.kill();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    assert!(out.status.success(), "{protocol}: moco failed\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("> read a.txt"), "{protocol}: the tool call is shown\n{stdout}");
    assert!(stdout.contains("The file says: ONE-LINE-OF-TEXT"), "{protocol}: the final answer quotes the file\n{stdout}");
    let problems = std::fs::read_to_string(&violations).unwrap_or_default();
    assert!(problems.is_empty(), "{protocol}: the server saw a bad request:\n{problems}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn openai_responses_round_trip_with_reasoning_replay() {
    if !python_ok() {
        return eprintln!("skipped: python3 not available");
    }
    run("openai-responses", "/v1");
}

#[test]
fn gemini_round_trip_with_thought_signature_replay() {
    if !python_ok() {
        return eprintln!("skipped: python3 not available");
    }
    run("gemini", "/v1beta");
}
