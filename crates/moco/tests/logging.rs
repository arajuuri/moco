//! Logging at the edges: which commands may leave a log behind, and that nothing logs to the terminal.
use std::path::PathBuf;
use std::process::Command;

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("moco-logtest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn moco(home: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_moco")).args(args).env("MOCO_HOME", home).env_remove("MOCO_LOG").env_remove("MOCO_CONFIG").output().unwrap()
}

#[test]
fn read_only_commands_do_not_create_a_log_or_its_directory() {
    let h = home("readonly");
    for args in [&["--help"][..], &["--version"], &["logs"], &["logs", "--path"], &["sessions"], &["permissions"], &["models", "nosuchmodel"]] {
        let out = moco(&h, args);
        assert!(out.status.success(), "moco {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        assert!(!h.join("data/logs").exists(), "moco {args:?} created the log directory");
    }
    // A bad MOCO_LOG used to be reported by writing to the log, which created the file `moco logs` then showed.
    let out = Command::new(env!("CARGO_BIN_EXE_moco")).arg("logs").env("MOCO_HOME", &h).env("MOCO_LOG", "loud").output().unwrap();
    assert!(out.status.success());
    assert!(!h.join("data/logs").exists(), "a bad MOCO_LOG made `moco logs` create a log");
    assert!(String::from_utf8_lossy(&out.stdout).contains("no log yet"), "{}", String::from_utf8_lossy(&out.stdout));
    let _ = std::fs::remove_dir_all(h);
}

fn python_ok() -> bool {
    Command::new("python3").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// One `-p` run against the fake LLM server with `MOCO_LOG` set to `level` (or unset). Returns what the process printed
/// and the text of the day file it wrote, if any.
fn run_with_log(tag: &str, level: Option<&str>) -> Option<(String, String, String)> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    if !python_ok() {
        eprintln!("skipped: python3 not available");
        return None;
    }
    let dir = home(tag);
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "ONE-LINE-OF-TEXT\n").unwrap();
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moco"));
    cmd.args(["--model", "fake/m", "--dir"]).arg(&work).args(["-p", "Read a.txt and tell me what it says."]).env("MOCO_HOME", dir.join("home")).env("MOCO_CONFIG", &config).env_remove("ANTHROPIC_API_KEY").env_remove("MOCO_LOG");
    if let Some(l) = level {
        cmd.env("MOCO_LOG", l);
    }
    let out = cmd.output().unwrap();
    let _ = server.kill();
    let _ = server.wait();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    let log = std::fs::read_dir(dir.join("home/data/logs")).ok().and_then(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).next()).map(|f| std::fs::read_to_string(f).unwrap()).unwrap_or_default();
    let _ = std::fs::remove_dir_all(dir);
    Some((stdout, stderr, log))
}

/// A whole `-p` run at the most verbose level: everything goes to the file, nothing to the terminal.
#[test]
fn a_run_at_trace_writes_the_log_file_and_nothing_to_the_terminal() {
    let Some((stdout, stderr, text)) = run_with_log("trace-run", Some("trace")) else { return };
    for (name, out) in [("stdout", &stdout), ("stderr", &stderr)] {
        for level in ["TRACE", "DEBUG", "INFO ", "WARN "] {
            assert!(!out.contains(&format!(" {level}")) && !out.starts_with(level), "{name} has a log line ({level}):\n{out}");
        }
        assert!(!out.contains("http-request"), "{name} shows wire-level logging:\n{out}");
    }
    assert!(text.lines().next().unwrap_or("").contains(" INFO  log: moco "), "the header comes first:\n{text}");
    for part in ["engine: run start", "provider:", "http-request", "engine: run end"] {
        assert!(text.contains(part), "the log has `{part}`:\n{text}");
    }
}

/// Unset `MOCO_LOG`: a turn with a tool call leaves only a few lines, and nothing a user did not ask to record.
#[test]
fn the_default_level_keeps_the_volume_small() {
    let Some((stdout, stderr, text)) = run_with_log("default-run", None) else { return };
    assert!(!stdout.contains(" WARN ") && !stderr.contains(" WARN "), "nothing is printed:\n{stdout}\n{stderr}");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() <= 6, "a turn with a tool call logs a few lines at the default level, got {}:\n{text}", lines.len());
    assert!(!text.contains("http-request") && !text.contains("DEBUG") && !text.contains("TRACE"), "{text}");
}
