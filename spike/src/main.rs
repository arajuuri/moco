//! Phase 0 platform spike (docs/10-roadmap.md). Each probe prints PASS/FAIL lines.
//! Usage: moco-spike [tls|sqlite|shell|secrets|all]
//! Optional env: ANTHROPIC_API_KEY enables a real SSE streaming request.
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn report(name: &str, ok: bool, detail: impl std::fmt::Display) {
    println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

fn tls() {
    let mode = if cfg!(feature = "native-certs") { "system-roots" } else { "bundled-roots" };
    println!("-- tls ({mode})");
    // Any HTTP status proves the handshake and certificate validation worked.
    for url in [
        "https://api.anthropic.com/",
        "https://api.openai.com/",
        "https://generativelanguage.googleapis.com/",
        "https://models.dev/api.json",
        "https://expired.badssl.com/", // must FAIL validation
    ] {
        let t = Instant::now();
        let r = agent().get(url).call();
        let status = match r {
            Ok(r) => Ok(r.status()),
            Err(ureq::Error::Status(c, _)) => Ok(c),
            Err(e) => Err(e.to_string()),
        };
        let expect_fail = url.contains("badssl");
        match status {
            Ok(c) => report(url, !expect_fail, format!("HTTP {c} in {:?}", t.elapsed())),
            Err(e) => report(url, expect_fail, format!("{e} (expected failure: {expect_fail})")),
        }
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        sse(&key);
    } else {
        println!("SKIP sse: set ANTHROPIC_API_KEY to test streaming");
    }
}

fn sse(key: &str) {
    let body = r#"{"model":"claude-haiku-5-5","max_tokens":64,"stream":true,
        "messages":[{"role":"user","content":"Count from 1 to 5."}]}"#;
    let t = Instant::now();
    let resp = agent()
        .post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_string(body);
    match resp {
        Ok(r) => {
            let (mut events, mut first) = (0, None);
            for line in BufReader::new(r.into_reader()).lines().map_while(Result::ok) {
                if line.starts_with("event:") {
                    events += 1;
                    first.get_or_insert(t.elapsed());
                }
            }
            report("sse anthropic", events > 2, format!("{events} events, first after {first:?}, total {:?}", t.elapsed()));
        }
        Err(e) => report("sse anthropic", false, e),
    }
}

fn sqlite() {
    println!("-- sqlite");
    let c = rusqlite::Connection::open_in_memory().unwrap();
    let v: String = c.query_row("select sqlite_version()", [], |r| r.get(0)).unwrap();
    report("version (bundled)", true, v);
    c.execute_batch("create table t(id integer primary key, v text)").unwrap();
    let ret: Result<i64, _> = c.query_row("insert into t(v) values ('x') returning id", [], |r| r.get(0));
    report("RETURNING", ret.is_ok(), format!("{ret:?}"));
    let js: Result<i64, _> = c.query_row("select json_extract('{\"a\":1}','$.a')", [], |r| r.get(0));
    report("JSON1", js.is_ok(), format!("{js:?}"));
    let path = std::env::temp_dir().join("moco-spike.db");
    let _ = std::fs::remove_file(&path);
    let f = rusqlite::Connection::open(&path).unwrap();
    let mode: String = f.query_row("pragma journal_mode=wal", [], |r| r.get(0)).unwrap();
    report("WAL on disk", mode == "wal", mode);
    let _ = std::fs::remove_file(&path);
}

fn shell() {
    println!("-- shell");
    let out = Command::new("/bin/zsh").args(["-c", "echo out; echo err >&2; exit 3"]).output().unwrap();
    report(
        "zsh spawn + capture",
        out.status.code() == Some(3) && out.stdout == b"out\n" && out.stderr == b"err\n",
        format!("{:?}", out.status),
    );
    // Timeout kill of a process that spawns a child.
    let mut ch = Command::new("/bin/zsh").args(["-c", "sleep 30 & wait"]).stdout(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let _ = ch.kill();
    let st = ch.wait().unwrap();
    let mut s = String::new();
    let _ = ch.stdout.take().unwrap().read_to_string(&mut s);
    report("kill running shell", !st.success(), format!("{st:?}"));
    for tool in ["git", "rg"] {
        let ok = Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
        println!("INFO {tool} on PATH: {ok}");
    }
}

fn secrets() {
    println!("-- secrets");
    const SVC: &str = "moco-spike-test";
    const ACCT: &str = "probe";
    let secret = b"sk-test-0123456789";

    // A. File with 0600 perms (what a file-based auth store would do).
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let path = std::env::temp_dir().join("moco-spike-auth.json");
        let _ = std::fs::remove_file(&path);
        let w = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, secret));
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
        let back = std::fs::read(&path).ok();
        report("file 0600 write/read", w.is_ok() && mode == 0o600 && back.as_deref() == Some(&secret[..]), format!("mode {mode:o}"));
        let _ = std::fs::remove_file(&path);
    }

    // B. Keychain via Security.framework (security-framework crate).
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords as pw;
        let t = Instant::now();
        let set = pw::set_generic_password(SVC, ACCT, secret);
        report("keychain set (framework)", set.is_ok(), format!("{set:?} in {:?}", t.elapsed()));
        if set.is_ok() {
            let got = pw::get_generic_password(SVC, ACCT);
            report("keychain get (framework)", got.as_deref().ok() == Some(&secret[..]), format!("{:?}", got.as_ref().map(|v| v.len())));
            let upd = pw::set_generic_password(SVC, ACCT, b"sk-updated");
            let got2 = pw::get_generic_password(SVC, ACCT).ok();
            report("keychain update", upd.is_ok() && got2.as_deref() == Some(&b"sk-updated"[..]), format!("{upd:?}"));
            let del = pw::delete_generic_password(SVC, ACCT);
            report("keychain delete", del.is_ok(), format!("{del:?}"));
        }
    }

    // C. Keychain via /usr/bin/security CLI (fallback; exposes secret in argv for `add`).
    let run = |args: &[&str]| Command::new("/usr/bin/security").args(args).output().map(|o| (o.status.success(), String::from_utf8_lossy(&o.stderr).trim().to_string())).unwrap_or((false, "spawn failed".into()));
    let (ok, err) = run(&["add-generic-password", "-s", SVC, "-a", ACCT, "-w", "cli-secret", "-U"]);
    report("keychain add (security CLI)", ok, err);
    if ok {
        let (ok, err) = run(&["find-generic-password", "-s", SVC, "-a", ACCT, "-w"]);
        report("keychain find (security CLI)", ok, err);
        let _ = run(&["delete-generic-password", "-s", SVC, "-a", ACCT]);
    }
    let (_, d) = run(&["default-keychain"]);
    println!("INFO default keychain: {d}");
    println!("INFO session: SSH_CONNECTION set = {}", std::env::var("SSH_CONNECTION").is_ok());
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    if matches!(which.as_str(), "tls" | "all") { tls(); }
    if matches!(which.as_str(), "sqlite" | "all") { sqlite(); }
    if matches!(which.as_str(), "shell" | "all") { shell(); }
    if matches!(which.as_str(), "secrets" | "all") { secrets(); }
}
