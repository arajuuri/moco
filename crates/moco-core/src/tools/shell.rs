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

/// Has `child` exited without having been reaped? `waitid(WNOWAIT)` leaves the zombie in place, so its process group id
/// cannot be reused yet and a signal to the group is safe. `None` when that cannot be told (already reaped, or an error).
pub(crate) fn exited_unreaped(child: &Child) -> Option<bool> {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT);
        if r != 0 {
            return None;
        }
        // Left untouched (still zero) when nothing has exited.
        Some(info.si_signo != 0)
    }
}

/// Reap `child` if it has exited, first killing the stragglers in its process group: the group id belongs to us only
/// until the leader is reaped. Once it is reaped, the group is never signalled again.
pub(crate) fn try_reap(child: &mut Child) -> std::io::Result<Option<std::process::ExitStatus>> {
    match exited_unreaped(child) {
        Some(true) => {
            kill_group(child.id());
            child.wait().map(Some)
        }
        Some(false) => Ok(None),
        None => child.try_wait(),
    }
}

/// A command whose output file grows past this is killed (1 GiB).
const MAX_OUTPUT_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// Output files up to this size are read whole; bigger ones only from the end.
const READ_WHOLE_BYTES: u64 = 4 * 1024 * 1024;
/// How much of the end of a big output file is read. Well over the 50 KiB that is kept.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// The output file's text, and its exact line count when only the end of it was read (so the caller reports the
/// real total). Memory use is bounded however much the command wrote.
fn read_output(path: &Path) -> (String, Option<usize>) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return (String::new(), None) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= READ_WHOLE_BYTES {
        let mut raw = Vec::new();
        let _ = f.read_to_end(&mut raw);
        return (String::from_utf8_lossy(&raw).into_owned(), None);
    }
    // Count lines in a streaming pass, as `split('\n')` would: one more than the newlines.
    let mut lines = 1usize;
    let mut chunk = vec![0u8; 64 * 1024];
    while let Ok(n) = f.read(&mut chunk) {
        if n == 0 {
            break;
        }
        lines += chunk[..n].iter().filter(|b| **b == b'\n').count();
    }
    let start = len - TAIL_WINDOW_BYTES;
    let mut raw = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_ok() {
        let _ = f.take(TAIL_WINDOW_BYTES).read_to_end(&mut raw);
    }
    // The window starts mid-line (and maybe mid-character): drop that partial first line when there is a whole one after it.
    if let Some(nl) = raw.iter().position(|b| *b == b'\n') {
        raw.drain(..=nl);
    }
    (String::from_utf8_lossy(&raw).into_owned(), Some(lines))
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
    wait_capped(child, out_path, timeout, stop, keep_file, MAX_OUTPUT_FILE_BYTES)
}

/// `wait`, killing the command as well when its output file grows past `output_cap` bytes (`yes`, a runaway log),
/// so it cannot fill the disk.
fn wait_capped(child: &mut Child, out_path: &Path, timeout: Option<Duration>, stop: &dyn Fn() -> bool, keep_file: bool, output_cap: u64) -> Finished {
    let pid = child.id();
    let start = Instant::now();
    let (mut timed_out, mut killed, mut over_cap) = (false, false, false);
    let status = loop {
        match try_reap(child) {
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
        if !killed && !timed_out && std::fs::metadata(out_path).is_ok_and(|m| m.len() > output_cap) {
            over_cap = true;
        }
        if killed || timed_out || over_cap {
            kill_group(pid);
            break child.wait().unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let (full, total_lines) = read_output(out_path);
    let (mut text, truncated) = match tail(&full) {
        Some((kept, n, total)) => (format!("[output truncated: showing last {n} of {} lines; full output saved to {}]\n{kept}", total_lines.unwrap_or(total), out_path.display()), true),
        // Only the end of a big file was read; even if that end is short it is not the whole output.
        None if total_lines.is_some() => (format!("[output truncated: showing last {} of {} lines; full output saved to {}]\n{full}", full.split('\n').count(), total_lines.unwrap_or(0), out_path.display()), true),
        None => (full, false),
    };
    if !truncated && !keep_file {
        let _ = std::fs::remove_file(out_path);
    }
    if over_cap {
        text.push_str(&format!("\n\n[output exceeded {} MiB and the command was killed]", output_cap / (1024 * 1024)));
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
Do not use interactive commands. Do not use this tool to create or change files (python or perl scripts, `sed -i`, `awk`, redirection such as `>` or `>>`, `tee`, heredocs) when the edit, write or patch tool can do it; if an edit fails, correct the edit call and retry it.\n\n{BACKGROUND_INSTRUCTION}"
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
        ctx.permitter.check(&PermReq { action: "shell".into(), resources: resources.clone(), save, force_ask: parsed.opaque, command: Some(command.to_string()), call_id: ctx.call_id.into() })?;

        // Leaving the sandbox is its own permission, so a blanket `shell` allow does not cover it.
        let unsandboxed = opt_bool(input, "unsandboxed").unwrap_or(false) && crate::sandbox::active();
        if unsandboxed {
            ctx.permitter.check(&PermReq { action: "shell.unsandboxed".into(), resources, save: unsandbox_save, force_ask: parsed.opaque, command: None, call_id: ctx.call_id.into() })?;
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
    #[test]
    fn the_description_sends_file_changes_to_the_edit_tools() {
        let d = super::Shell.description();
        assert!(d.contains("Do not use this tool to create or change files") && d.contains("`sed -i`") && d.contains("edit, write or patch"), "{d}");
        assert!(d.contains("correct the edit call and retry it"), "{d}");
    }

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
    fn stragglers_are_killed_after_the_shell_exits() {
        let env = Env::new();
        let pidfile = env.work().join("straggler.pid");
        let cmd = format!("sleep 30 >/dev/null 2>&1 & echo $! > {}", pidfile.display());
        run(&env, json!({"command": cmd})).unwrap();
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // The background sleep is gone (or at most a zombie awaiting init) shortly after the call returns.
        let t = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0 && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the straggler survived");
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
    fn huge_output_is_read_from_the_end_with_the_true_line_count() {
        let env = Env::new();
        // 1,200,000 lines of "line" = 6,000,000 bytes, over the read-whole threshold.
        let o = run(&env, json!({"command":"yes line | head -c 6000000"})).unwrap();
        assert!(o.text.starts_with("[output truncated: showing last 2000 of 1200001 lines;"), "{}", &o.text[..100]);
        assert!(o.text.ends_with("line\n"), "the end of the output is kept");
        let path = o.metadata["outputPath"].as_str().unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), 6_000_000, "the full output is still saved");
    }

    #[test]
    fn a_short_end_of_a_huge_output_is_still_marked_truncated() {
        let env = Env::new();
        // One 5 MB line, then a short one: the window ends up holding only "tail".
        let o = run(&env, json!({"command":"head -c 5000000 /dev/zero | tr '\\0' x; echo; echo tail"})).unwrap();
        assert!(o.text.starts_with("[output truncated:") && o.text.ends_with("tail\n"), "{}", &o.text[..80]);
        assert_eq!(o.metadata["truncated"], true);
    }

    #[test]
    fn output_past_the_cap_kills_the_command() {
        let env = Env::new();
        let out = env.work().join("capped.txt");
        let mut child = spawn("yes", &env.work(), &out, None).unwrap();
        let t = Instant::now();
        let f = wait_capped(&mut child, &out, None, &|| false, false, 1024 * 1024);
        assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
        assert!(f.text.contains("output exceeded 1 MiB and the command was killed"), "{}", &f.text[f.text.len().saturating_sub(200)..]);
        assert!(f.truncated && f.signal.is_some());
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
