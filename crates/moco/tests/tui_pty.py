#!/usr/bin/env python3
"""Drives `moco --tui` in a pseudo terminal: prompt, permission dialog, streamed answer, resize, quit.

Usage: tui_pty.py MOCO CONFIG HOME WORKDIR SCENARIO    (SCENARIO: chat | sigterm | hangup | hangup-idle | steer | config | mode | newsession | panic)
Exits 0 when everything held; otherwise prints what went wrong and the screen text so far."""
import fcntl, os, pty, re, select, signal, struct, sys, termios, time

moco, config, home, work, scenario = sys.argv[1:6]
env = dict(os.environ, MOCO_CONFIG=config, MOCO_HOME=home, TERM="xterm-256color")
if scenario == "panic":
    env["MOCO_TEST_PANIC_ON_TEXT"] = "Reading the file."  # the first thing the fake model says
env.pop("ANTHROPIC_API_KEY", None)
env.pop("COLORTERM", None)  # no truecolor: #rrggbb theme colors must fall back to the 256-color palette

pid, fd = pty.fork()
if pid == 0:
    os.chdir(work)
    os.execve(moco, [moco, "--model", "fake/m", "--dir", work], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 130 if scenario == "config" else 100, 0, 0))
raw = b""


def pump(t=0.1):
    global raw
    end = time.time() + t
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            raw += data
    return True


def text():
    s = raw.decode("utf-8", "replace")
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][A-Z0-9]|\x1b[=>]", "", s)


def squashed():
    # ratatui moves the cursor over unchanged cells instead of writing spaces, so compare without whitespace.
    return re.sub(r"\s+", "", text())


def fail(msg):
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    print("FAIL:", msg)
    print("--- screen text so far:\n" + text()[-3000:])
    sys.exit(1)


def wait_for(pattern, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        if re.search(pattern, squashed()):
            return
        pump(0.1)
    fail("timeout waiting for %r" % pattern)


def wait_for_response(user_text, timeout=15):
    import json, sqlite3
    db = os.path.join(home, "data", "moco.db")
    end = time.time() + timeout
    while time.time() < end:
        pump(0.1)
        try:
            rows = sqlite3.connect(db).execute("select type, data from message order by id").fetchall()
        except sqlite3.Error:
            continue
        for i, (kind, data) in enumerate(rows):
            if kind != "user" or json.loads(data).get("text") != user_text:
                continue
            for next_kind, next_data in rows[i + 1:]:
                if next_kind != "assistant":
                    continue
                assistant = json.loads(next_data)
                parts = assistant.get("parts", [])
                if "usage" in assistant and any(p.get("type") == "text" for p in parts) and not any(p.get("type") == "tool" for p in parts):
                    return
            break
    fail("timeout waiting for response to %r" % user_text)


def send(b):
    os.write(fd, b)


def wait_exit(timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.1)
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            pump(0.2)
            return status
    fail("moco did not exit")


def check_restored(status, code_ok=True):
    if code_ok and not (os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0):
        fail("bad exit status %r" % status)
    t = raw.decode("utf-8", "replace")
    if "\x1b[?1049h" not in t or "\x1b[?1049l" not in t:
        fail("the alternate screen was not entered and left")
    if t.rfind("\x1b[?1049l") < t.rfind("\x1b[?1049h"):
        fail("the alternate screen was not left last")
    if "\x1b[?2004h" in t and "\x1b[?2004l" not in t:
        fail("bracketed paste was left on")
    if t.rfind("\x1b[?25h") < t.rfind("\x1b[?1049l"):
        fail("the cursor was not shown again")


wait_for(r"moco0\.0\.0")
if scenario == "config":
    send(b"\x1bOQ")  # F2 is the configured palette key
    wait_for(r"commands·\d+")
    send(b"\x1b")
    pump(0.4)
    send(b"\x18")  # the leader key, then h: help, which names the configured key
    send(b"h")
    wait_for(r"F2opensthecommandpalette")
    send(b"Read a.txt and tell me what it says.\r")
    wait_for(r"permissionneeded:read")
    # tui.away_after is 0: the user counts as away, so the request rings the bell and marks the window title.
    if "\x1b]0;moco: read needs permission\x07" not in raw.decode("utf-8", "replace"):
        fail("no window title mark for the permission request")
    send(b"y")
    wait_for(r"Thefilesays:ONE-LINE-OF-TEXT")
    end = time.time() + 10
    while "\x1b]0;moco: done\x07" not in raw.decode("utf-8", "replace") and time.time() < end:
        pump(0.2)
    t = raw.decode("utf-8", "replace")
    if "\x1b]0;moco: done\x07" not in t or "\x07" not in t.replace("\x1b]0;moco: done\x07", "").replace("\x1b]0;moco: read needs permission\x07", "").replace("\x1b]0;moco\x07", ""):
        fail("no bell or no title mark when the turn finished")
    if not re.search(r"\x1b\[38;5;46[;m]", t):
        fail("the theme file's user color (#00ff00 -> palette 46) was not used; colors seen: %r" % sorted(set(re.findall(r"\x1b\[[0-9;]*m", t)))[:30])
    send(b"\x04")
    check_restored(wait_exit())
elif scenario == "steer":
    send(b"SLOW read a.txt\r")
    wait_for(r"running:")  # the turn is going (the model is slow on purpose)
    send(b"also steer this\r")
    wait_for(r"nextstepalsosteerthis")  # waiting in the strip above the composer
    send(b"and queue this\x11")  # Ctrl-Q
    pump(0.3)
    # Both are delivered by the engine: the steer at a step boundary, the queued one when the turn ends.
    import sqlite3, json
    db = os.path.join(home, "data", "moco.db")
    end = time.time() + 30
    texts = []
    while time.time() < end:
        pump(0.3)
        try:
            rows = sqlite3.connect(db).execute("select type, data from message order by id").fetchall()
        except sqlite3.Error:
            continue
        texts = [json.loads(d)["text"] for t, d in rows if t == "user"]
        if len(texts) == 3 and rows[-1][0] == "assistant":
            break
    if texts != ["SLOW read a.txt", "also steer this", "and queue this"]:
        fail("user messages in the store: %r" % texts)
    kinds = [t for t, _ in rows]
    # the steer went in after the first step (tool call + result), before the final answer; the queued one after it
    if kinds[:2] != ["user", "assistant"] or kinds.index("user", 1) > 3:
        fail("unexpected message order %r" % kinds)
    # Ctrl-D is ignored while a turn is still finishing (usage, title), so repeat it until moco leaves.
    end = time.time() + 15
    status = None
    while status is None and time.time() < end:
        send(b"\x04")
        pump(0.5)
        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            pump(0.2)
            status = st
    if status is None:
        fail("moco did not exit")
    check_restored(status)
elif scenario == "chat":
    send(b"Read a.txt and tell me what it says.\r")
    wait_for(r"permissionneeded:read")
    send(b"y")
    wait_for(r"Thefilesays:ONE-LINE-OF-TEXT")
    wait_for(r"in·\d+out")
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 20, 70, 0, 0))
    os.kill(pid, signal.SIGWINCH)
    pump(0.6)
    send(b"/nope\r")
    wait_for(r"unknowncommand/nope")
    send(b"!echo hi-from-shell\r")
    end = time.time() + 15
    while squashed().count("hi-from-shell") < 2:  # the command row and its output
        if time.time() > end:
            fail("the !command output never showed")
        pump(0.1)
    send(b"look at @a.t")
    wait_for(r"@a\.txt")  # the completion popup lists the file
    send(b"\t")  # Tab completes it
    send(b"\r")  # sends "look at @a.txt": the mention was completed, so the file gets attached
    wait_for(r"attached.*a\.txt")
    wait_for_response("look at @a.txt")
    send(b"/model\r")
    wait_for(r"model·1")  # the picker lists the one connected model
    wait_for(r"fake/m")
    send(b"\r")
    wait_for(r"model:fake/m")
    send(b"\x10")  # Ctrl-P: command palette
    wait_for(r"commands·\d+")
    send(b"und")  # filter
    wait_for(r"/undo")
    send(b"\x1b")
    pump(0.4)  # a lone Esc must not run into the next key (ESC + / would be Alt-/)
    send(b"/sessions\r")
    wait_for(r"sessions·1")
    send(b"\r")  # resume the one session listed
    wait_for(r"session\w{8}:Fakeprotocoltest")
    send(b"/new\r")
    wait_for(r"session\w{8}:\(untitled\)")
    send(b"/undo\r")
    wait_for(r"nothingtoundo")
    send(b"one\x1b\rtwo")  # Alt-Enter adds a line instead of sending
    pump(0.3)
    send(b"\x1b[A\x15\x0b")  # Up to the first line, Ctrl-U and Ctrl-K clear it
    send(b"\x1b[B\x05\x15")  # back down, End, clear the second
    pump(0.3)
    send(b"\x04")  # Ctrl-D on an empty composer
    check_restored(wait_exit())
elif scenario == "newsession":
    # A prompt typed after /new must run in the new session (it used to be queued in the old one, so nothing happened).
    send(b"/new\r")
    wait_for(r"session\w{8}:\(untitled\)")
    send(b"Read a.txt and tell me what it says.\r")
    wait_for_response("Read a.txt and tell me what it says.")
    send(b"\x04")
    check_restored(wait_exit())
elif scenario == "mode":
    # Tab switches the permission mode; the transcript confirms each switch.
    wait_for(r"fake/mbuildmanual")  # the status line starts in manual
    send(b"\t")
    wait_for(r"permissionmode:auto")
    send(b"\t")
    wait_for(r"permissionmode:manual")
    send(b"\x04")
    check_restored(wait_exit())
elif scenario == "panic":
    # A panic on the engine worker thread: the run ends with an error, the screen comes back, the session claim is
    # released (a second prompt runs, and crashes the same way), and moco still exits cleanly.
    send(b"Read a.txt and tell me what it says.\r")
    wait_for(r"theenginecrashed:injectedtestpanic")
    if "storeerror:theenginecrashed" in squashed():
        fail("the crash is labelled as a store error")
    pump(0.5)
    t = raw.decode("utf-8", "replace")
    if t.rfind("\x1b[?1049h") < t.rfind("\x1b[?1049l"):
        fail("the alternate screen was not entered again after the panic")
    send(b"Read a.txt and tell me what it says.\r")
    end = time.time() + 15
    while squashed().count("theenginecrashed") < 2:
        if time.time() > end:
            fail("the second prompt did not run (claim still held?)")
        pump(0.1)
    send(b"\x04")
    check_restored(wait_exit())
elif scenario == "sigterm":
    send(b"Read a.txt and tell me what it says.\r")
    wait_for(r"permissionneeded:read")  # the run is blocked on the dialog
    os.kill(pid, signal.SIGTERM)
    status = wait_exit()
    check_restored(status)
elif scenario in ("hangup", "hangup-idle"):
    # The terminal goes away under moco, as when an SSH connection drops: close the pty master. moco must exit
    # instead of spinning in crossterm's reader on the dead tty (issue #87). "hangup" drops it while a permission
    # dialog blocks a run, "hangup-idle" at an empty prompt.
    if scenario == "hangup":
        send(b"Read a.txt and tell me what it says.\r")
        wait_for(r"permissionneeded:read")
    else:
        wait_for(r"fake/mbuildmanual")
    os.close(fd)
    end = time.time() + 8
    while time.time() < end:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        time.sleep(0.1)
    else:
        cpu = os.popen("ps -o %%cpu= -p %d" % pid).read().strip()
        os.kill(pid, signal.SIGKILL)
        print("FAIL: moco was still running 8 s after its terminal hung up (cpu %s%%)" % cpu)
        sys.exit(1)
else:
    fail("unknown scenario")
print("ok")
