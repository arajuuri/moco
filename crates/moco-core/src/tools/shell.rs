use super::*;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Shell;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

fn shell_program() -> String {
    std::env::var("MOCO_SHELL")
        .ok()
        .or_else(|| std::env::var("SHELL").ok().filter(|s| Path::new(s).exists()))
        .unwrap_or_else(|| "/bin/sh".into())
}

fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Keep the last 2000 lines / 50 KiB (the end of command output matters most).
fn tail(text: &str) -> Option<(String, usize, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= crate::truncate::MAX_LINES && text.len() <= crate::truncate::MAX_BYTES {
        return None;
    }
    let (mut bytes, mut n) = (0usize, 0usize);
    for l in lines.iter().rev() {
        if n >= crate::truncate::MAX_LINES || bytes + l.len() + 1 > crate::truncate::MAX_BYTES {
            break;
        }
        bytes += l.len() + 1;
        n += 1;
    }
    let n = n.max(1);
    let mut kept = lines[lines.len() - n..].join("\n");
    if kept.len() > crate::truncate::MAX_BYTES {
        let mut cut = kept.len() - crate::truncate::MAX_BYTES;
        while !kept.is_char_boundary(cut) {
            cut += 1;
        }
        kept = kept[cut..].to_string();
    }
    Some((kept, n, lines.len()))
}

impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn description(&self) -> &'static str {
        "Run a shell command in the working directory (or `workdir`). stdout and stderr are combined. \
Default timeout is 120000 ms (0 disables). Long output is truncated to its last 2000 lines / 50 KiB; the full output is saved to a file. \
Do not use interactive commands."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "command":{"type":"string"},"workdir":{"type":"string","description":"Directory to run in"},
            "timeout":{"type":"integer","minimum":0,"description":"Timeout in milliseconds"}},"required":["command"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = str_arg(input, "command")?;
        if opt_bool(input, "background").unwrap_or(false) {
            return Err(ToolError::fail("background shell commands are not supported yet"));
        }
        let workdir = opt_str(input, "workdir").map(|p| ctx.resolve(p)).unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = opt_u64(input, "timeout").unwrap_or(DEFAULT_TIMEOUT_MS);

        // Permission: external directories first (workdir and cd targets), then the commands.
        ctx.check_external(&workdir)?;
        let parsed = shellparse::parse(command);
        for dir in parsed.commands.iter().flat_map(|c| &c.dirs) {
            let dir = match dir.strip_prefix("~") {
                Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest.trim_start_matches('/')),
                None => PathBuf::from(dir),
            };
            ctx.check_external(&normalize(&if dir.is_absolute() { dir } else { workdir.join(dir) }))?;
        }
        let mut resources: Vec<String> = parsed.commands.iter().map(|c| c.text.clone()).collect();
        let mut save: Vec<String> = parsed.commands.iter().map(|c| shellparse::save_pattern(&c.words)).collect();
        if resources.is_empty() {
            resources.push(command.trim().to_string());
        }
        resources.dedup();
        save.dedup();
        ctx.permitter.check(&PermReq { action: "shell".into(), resources, save, force_ask: parsed.opaque })?;

        // Combined stdout+stderr go to one file (shared offset keeps them interleaved).
        std::fs::create_dir_all(&ctx.spill_dir).map_err(|e| ToolError::fail(e.to_string()))?;
        let out_path = ctx.spill_dir.join(format!("{}.txt", crate::id::new_id("shell")));
        let file = std::fs::File::create(&out_path).map_err(|e| ToolError::fail(e.to_string()))?;
        let err_file = file.try_clone().map_err(|e| ToolError::fail(e.to_string()))?;
        let mut child = Command::new(shell_program())
            .arg("-c")
            .arg(command)
            .current_dir(&workdir)
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err_file))
            .process_group(0)
            .spawn()
            .map_err(|e| ToolError::fail(format!("failed to start shell: {e}")))?;
        let pid = child.id();

        let start = Instant::now();
        let (mut timed_out, mut interrupted) = (false, false);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {}
                Err(e) => return Err(ToolError::fail(e.to_string())),
            }
            if ctx.cancelled() {
                interrupted = true;
            } else if timeout_ms > 0 && start.elapsed() >= Duration::from_millis(timeout_ms) {
                timed_out = true;
            }
            if interrupted || timed_out {
                kill_group(pid);
                break child.wait().map_err(|e| ToolError::fail(e.to_string()))?;
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        // Reap stragglers left in the process group after the shell exits.
        kill_group(pid);

        let raw = std::fs::read(&out_path).unwrap_or_default();
        let full = String::from_utf8_lossy(&raw).into_owned();
        let (mut text, truncated) = match tail(&full) {
            Some((kept, n, total)) => (
                format!("[output truncated: showing last {n} of {total} lines; full output saved to {}]\n{kept}", out_path.display()),
                true,
            ),
            None => (full, false),
        };
        if !truncated {
            let _ = std::fs::remove_file(&out_path);
        }
        if interrupted {
            return Err(ToolError::Interrupted);
        }
        if timed_out {
            text.push_str(&format!("\n\n[command timed out after {timeout_ms} ms and was killed]"));
        } else if let Some(sig) = status.signal() {
            text.push_str(&format!("\n\n[terminated by signal {sig}]"));
        } else if let Some(code) = status.code().filter(|c| *c != 0) {
            text.push_str(&format!("\n\n[exit code {code}]"));
        }
        Ok(ToolOutput {
            text,
            metadata: json!({"exit": status.code(), "signal": status.signal(), "timeout": timed_out, "truncated": truncated,
                             "outputPath": truncated.then(|| out_path.to_string_lossy().into_owned())}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;
    use std::sync::atomic::Ordering;

    fn run(env: &Env, v: Value) -> Result<ToolOutput, ToolError> {
        Shell.run(&v, &env.ctx())
    }

    #[test]
    fn output_exit_codes_and_combined_streams() {
        let env = Env::new();
        let o = run(&env, json!({"command":"echo out; echo err >&2; exit 3"})).unwrap();
        assert_eq!(o.text, "out\nerr\n\n\n[exit code 3]");
        assert_eq!(o.metadata["exit"], 3);
        assert_eq!(run(&env, json!({"command":"printf hi"})).unwrap().text, "hi");
    }

    #[test]
    fn runs_in_workdir() {
        let env = Env::new();
        env.file("sub/x", b"");
        assert_eq!(run(&env, json!({"command":"ls","workdir":"sub"})).unwrap().text, "x\n");
    }

    #[test]
    fn permission_resources_and_save_patterns() {
        let env = Env::new();
        run(&env, json!({"command":"git status && npm run dev -- --x"})).unwrap();
        let req = &env.perm.asked()[0];
        assert_eq!(req.action, "shell");
        assert_eq!(req.resources, ["git status", "npm run dev -- --x"]);
        assert_eq!(req.save, ["git status *", "npm run dev *"]);
        assert!(!req.force_ask);
        run(&env, json!({"command":"echo $(id)"})).ok();
        assert!(env.perm.asked().last().unwrap().force_ask);
    }

    #[test]
    fn external_cd_asks_before_running() {
        let env = Env::new();
        run(&env, json!({"command":"cd /usr && true"})).unwrap();
        let asked = env.perm.asked();
        assert_eq!(asked[0].action, "external_directory");
        assert_eq!(asked[0].resources, ["/usr/*"]);
        assert_eq!(asked[1].action, "shell");
    }

    #[test]
    fn declined_command_never_runs() {
        let env = Env::new();
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(run(&env, json!({"command":"touch ran"})).unwrap_err(), ToolError::Declined);
        assert!(!env.work().join("ran").exists());
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        let env = Env::new();
        let t = Instant::now();
        let o = run(&env, json!({"command":"sleep 30 & sleep 30; echo never","timeout":300})).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
        assert!(o.text.contains("timed out after 300 ms"), "{}", o.text);
        assert_eq!(o.metadata["timeout"], true);
    }

    #[test]
    fn interrupt_cancels_running_command() {
        let env = Env::new();
        let t = Instant::now();
        let res = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                env.cancel.store(true, Ordering::Relaxed);
            });
            run(&env, json!({"command":"sleep 30"}))
        });
        assert_eq!(res.unwrap_err(), ToolError::Interrupted);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn long_output_keeps_the_tail_and_saves_the_rest() {
        let env = Env::new();
        let o = run(&env, json!({"command":"i=1; while [ $i -le 3000 ]; do echo line$i; i=$((i+1)); done"})).unwrap();
        assert!(o.text.starts_with("[output truncated: showing last 2000 of 3001 lines;"), "{}", &o.text[..120]);
        assert!(o.text.contains("line3000") && !o.text.contains("\nline1\n"));
        let path = o.metadata["outputPath"].as_str().unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("line1\n"));
    }

    #[test]
    fn background_is_rejected_for_now() {
        let env = Env::new();
        assert!(run(&env, json!({"command":"true","background":true})).is_err());
    }
}
