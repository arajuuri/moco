use super::*;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub struct Shell;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Added to the tool description: how background commands are meant to be used.
const BACKGROUND_INSTRUCTION: &str = "Set `background: true` for commands that keep running or take long and that you do not need to wait for \
(dev servers, watchers, long builds or test runs). The call returns at once with a shell ID and the file the output streams to. \
You are told automatically when the command exits, with its result: do not poll, sleep or re-run it to check on it. \
The `jobs` tool shows the output so far or stops it. A background command has no timeout unless you pass one.";

pub(crate) fn shell_program() -> String {
    std::env::var("MOCO_SHELL")
        .ok()
        .or_else(|| std::env::var("SHELL").ok().filter(|s| Path::new(s).exists()))
        .unwrap_or_else(|| "/bin/sh".into())
}

pub(crate) fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Keep the last 2000 lines / 50 KiB (the end of command output matters most).
pub(crate) fn tail(text: &str) -> Option<(String, usize, usize)> {
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

/// Start `command` in its own process group, stdout and stderr combined into `out_path` (one file with a shared
/// offset keeps them interleaved), no stdin.
/// `sandbox_root` is `Some(project root)` to run it inside the sandbox (when one is active; see `sandbox.rs`).
pub(crate) fn spawn(command: &str, workdir: &Path, out_path: &Path, sandbox_root: Option<&Path>) -> Result<Child, ToolError> {
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::fail(e.to_string()))?;
    }
    let file = std::fs::File::create(out_path).map_err(|e| ToolError::fail(e.to_string()))?;
    let err_file = file.try_clone().map_err(|e| ToolError::fail(e.to_string()))?;
    let shell = shell_program();
    let mut cmd = match sandbox_root.and_then(|root| crate::sandbox::wrap(&shell, command, root)) {
        Some((program, args)) => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        None => {
            let mut c = Command::new(&shell);
            c.arg("-c").arg(command);
            c
        }
    };
    cmd.current_dir(workdir)
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err_file))
        .process_group(0);
    cmd.spawn().map_err(|e| ToolError::fail(format!("failed to start shell: {e}")))
}

/// How a command ended.
pub(crate) struct Finished {
    /// The output (its tail when long) followed by notices for a timeout, a signal or a non-zero exit.
    pub text: String,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// `stop` fired and the process group was killed.
    pub killed: bool,
    pub truncated: bool,
    pub out_path: PathBuf,
}

/// Wait for `child` (polling), killing its process group when `timeout` passes or `stop()` turns true.
/// The output file is removed afterwards unless it was truncated or `keep_file` is set.
pub(crate) fn wait(child: &mut Child, out_path: &Path, timeout: Option<Duration>, stop: &dyn Fn() -> bool, keep_file: bool) -> Finished {
    let pid = child.id();
    let start = Instant::now();
    let (mut timed_out, mut killed) = (false, false);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(_) => {
                kill_group(pid);
                break child.wait().unwrap_or_default();
            }
        }
        if stop() {
            killed = true;
        } else if timeout.is_some_and(|t| start.elapsed() >= t) {
            timed_out = true;
        }
        if killed || timed_out {
            kill_group(pid);
            break child.wait().unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    // Reap stragglers left in the process group after the shell exits.
    kill_group(pid);

    let raw = std::fs::read(out_path).unwrap_or_default();
    let full = String::from_utf8_lossy(&raw).into_owned();
    let (mut text, truncated) = match tail(&full) {
        Some((kept, n, total)) => (format!("[output truncated: showing last {n} of {total} lines; full output saved to {}]\n{kept}", out_path.display()), true),
        None => (full, false),
    };
    if !truncated && !keep_file {
        let _ = std::fs::remove_file(out_path);
    }
    if timed_out {
        text.push_str(&format!("\n\n[command timed out after {} ms and was killed]", timeout.map_or(0, |t| t.as_millis())));
    } else if let Some(sig) = status.signal() {
        text.push_str(&format!("\n\n[terminated by signal {sig}]"));
    } else if let Some(code) = status.code().filter(|c| *c != 0) {
        text.push_str(&format!("\n\n[exit code {code}]"));
    }
    Finished { text, exit: status.code(), signal: status.signal(), timed_out, killed, truncated, out_path: out_path.to_path_buf() }
}

/// Run `command` to completion, no model involved. `timeout_ms` 0 means no timeout.
pub(crate) fn run_foreground(command: &str, workdir: &Path, spill_dir: &Path, timeout_ms: u64, cancel: &AtomicBool, sandbox_root: Option<&Path>) -> Result<Finished, ToolError> {
    let out_path = spill_dir.join(format!("{}.txt", crate::id::new_id("shell")));
    let mut child = spawn(command, workdir, &out_path, sandbox_root)?;
    let timeout = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms));
    let started = std::time::Instant::now();
    let mut f = wait(&mut child, &out_path, timeout, &|| cancel.load(std::sync::atomic::Ordering::Relaxed), false);
    let sandboxed = sandbox_root.is_some() && crate::sandbox::active();
    if sandboxed {
        crate::sandbox::annotate(&mut f.text, f.exit);
    }
    let what = format!("exit={:?} signal={:?} timed_out={} killed={} truncated={} sandboxed={sandboxed} ms={} command={}", f.exit, f.signal, f.timed_out, f.killed, f.truncated, started.elapsed().as_millis(), crate::classifier::sanitize(command, 200));
    if f.timed_out {
        crate::log_warn!("shell", "{what}");
    } else {
        crate::log_debug!("shell", "{what}");
    }
    Ok(f)
}

impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> String {
        format!(
            "Run a shell command in the working directory (or `workdir`). stdout and stderr are combined. \
Default timeout is 120000 ms (0 disables). Long output is truncated to its last 2000 lines / 50 KiB; the full output is saved to a file. \
Do not use interactive commands.\n\n{BACKGROUND_INSTRUCTION}"
        )
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "command":{"type":"string"},"workdir":{"type":"string","description":"Directory to run in"},
            "timeout":{"type":"integer","minimum":0,"description":"Timeout in milliseconds"},
            "background":{"type":"boolean","description":"Run in the background and return at once; you are told when it exits"},
            "unsandboxed":{"type":"boolean","description":"Only when the sandbox blocked a command that must run: run it outside the sandbox. The user is asked first."}},"required":["command"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = str_arg(input, "command")?;
        let background = opt_bool(input, "background").unwrap_or(false);
        let workdir = opt_str(input, "workdir").map(|p| ctx.resolve(p)).unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = opt_u64(input, "timeout");

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
        let unsandbox_save = save.clone();
        ctx.permitter.check(&PermReq { action: "shell".into(), resources: resources.clone(), save, force_ask: parsed.opaque, command: Some(command.to_string()) })?;

        // Leaving the sandbox is its own permission, so a blanket `shell` allow does not cover it.
        let unsandboxed = opt_bool(input, "unsandboxed").unwrap_or(false) && crate::sandbox::active();
        if unsandboxed {
            ctx.permitter.check(&PermReq { action: "shell.unsandboxed".into(), resources, save: unsandbox_save, force_ask: parsed.opaque, command: None })?;
        }

        if background {
            let (id, path) = ctx.host.start_background_shell(command, &workdir, timeout_ms.filter(|t| *t > 0).map(Duration::from_millis), unsandboxed)?;
            return Ok(ToolOutput {
                text: format!("Command moved to the background (shell ID: {id}).\nOutput is streaming to: {}", path.display()),
                metadata: json!({"shellID": id, "outputPath": path.to_string_lossy(), "status": "running"}),
            });
        }

        let root = (!unsandboxed).then_some(ctx.project_root.as_path());
        let f = run_foreground(command, &workdir, &ctx.spill_dir, timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS), ctx.cancel, root)?;
        if f.killed {
            return Err(ToolError::Interrupted);
        }
        Ok(ToolOutput {
            text: f.text,
            metadata: json!({"exit": f.exit, "signal": f.signal, "timeout": f.timed_out, "truncated": f.truncated,
                             "outputPath": f.truncated.then(|| f.out_path.to_string_lossy().into_owned())}),
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
    fn background_goes_through_the_host() {
        let env = Env::new();
        let o = run(&env, json!({"command":"sleep 5","background":true,"timeout":1500})).unwrap();
        assert_eq!(o.text, "Command moved to the background (shell ID: shl_test).\nOutput is streaming to: /tmp/shl_test.txt");
        assert_eq!(o.metadata["status"], "running");
        let started = env.host.shells.lock().unwrap().clone();
        assert_eq!((started[0].0.as_str(), started[0].2), ("sleep 5", Some(Duration::from_millis(1500))));
        // Permission is still checked before anything starts.
        assert_eq!(env.perm.asked()[0].action, "shell");
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(run(&env, json!({"command":"sleep 5","background":true})).unwrap_err(), ToolError::Declined);
        assert_eq!(env.host.shells.lock().unwrap().len(), 1, "a declined command is never started");
    }

    #[test]
    fn the_description_tells_the_model_not_to_poll() {
        let d = Shell.description();
        assert!(d.contains("background: true") && d.contains("do not poll") && d.contains("`jobs` tool"));
    }
}
