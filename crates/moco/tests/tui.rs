//! End to end: the real `moco --tui` in a pseudo terminal against the scripted local model server
//! (`tests/fake_llm.py`, OpenAI Responses). Needs `python3`; skipped without it.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

fn python_ok() -> bool {
    Command::new("python3").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn scenario(name: &str) {
    if !python_ok() {
        return eprintln!("skipped: python3 not available");
    }
    let dir = std::env::temp_dir().join(format!("moco-tui-{name}-{}", std::process::id()));
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
               "permissions": [ {{ "action": "read", "resource": "*", "effect": "{}" }} ],
               "tui": {},
               "snapshots": false }}"#,
            port.trim(),
            if matches!(name, "steer" | "newsession") { "allow" } else { "ask" },
            if name == "config" { r#"{ "theme": "neon", "keys": { "palette": "f2" }, "away_after": 0, "sidebar": true }"# } else { "{}" }
        ),
    )
    .unwrap();
    // A theme file for the "config" scenario: green user rows, which a 256-color terminal gets as palette entry 46.
    std::fs::create_dir_all(dir.join("home/config/themes")).unwrap();
    std::fs::write(dir.join("home/config/themes/neon.json"), r##"{ "extends": "mono", "user": "#00ff00 bold" }"##).unwrap();
    let out = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tui_pty.py"))
        .arg(env!("CARGO_BIN_EXE_moco"))
        .arg(&config)
        .arg(dir.join("home"))
        .arg(&work)
        .arg(name)
        .output()
        .unwrap();
    let _ = server.kill();
    let _ = server.wait();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "tui {name} failed\n{stdout}\n{stderr}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn chat_with_permission_dialog_resize_and_clean_exit() {
    scenario("chat");
}

#[test]
fn sigterm_while_a_dialog_is_open_restores_the_terminal() {
    scenario("sigterm");
}

#[test]
fn a_hung_up_terminal_ends_moco_during_a_dialog() {
    scenario("hangup");
}

#[test]
fn a_hung_up_terminal_ends_moco_at_an_idle_prompt() {
    scenario("hangup-idle");
}

#[test]
fn prompts_typed_during_a_turn_are_steered_or_queued_and_delivered() {
    scenario("steer");
}

#[test]
fn configured_theme_keys_and_bell() {
    scenario("config");
}

#[test]
fn a_prompt_after_new_runs_in_the_new_session() {
    scenario("newsession");
}

#[test]
fn tab_switches_the_permission_mode() {
    scenario("mode");
}

#[test]
fn a_panic_on_the_engine_thread_ends_the_run_and_the_ui_carries_on() {
    if !cfg!(debug_assertions) {
        return eprintln!("skipped: the panic injection exists only in debug builds");
    }
    scenario("panic");
}
