# 15. Idle CPU: where it went and what was changed

Reported: "moco seems to be using a lot of CPU on this machine." This records the investigation, the two fixes, the
measurements before and after, and what was not done. All numbers are from one macOS 10.7.5 machine (`hw.ncpu` reported 2),
built with the MacPorts toolchain as in `14-macos-10.7.md`. Compare ratios, not absolutes.

## Finding

moco was not busy when looked at. An idle TUI used under 1% of a core. It wakes on three timers, and none of them waits for
an event:

| Source | Where | Wakeups/s | What each wakeup did |
|---|---|---|---|
| UI redraw | `tui/mod.rs`, `recv_timeout(150 ms)` | ~7 | A timeout became `Tick`, then a full `terminal.draw` (layout, buffer diff, a `/dev/tty` open for the size), writing a cursor update to the terminal each time |
| Input thread | `tui/mod.rs`, `event::poll(100 ms)` plus `tty_gone` | ~10 | `kevent` and `select` |
| Foreground shell wait | `tools/shell.rs`, `wait_capped`, 15 ms sleep | ~66 | `waitid`, `stat` of the output file, `stop()` |

While a model ran commands (moco's own shell tool), the shell wait dominated.

## Fix 1: wait for a command without polling fast

`wait_capped` (`tools/shell.rs`, used by foreground shells and background jobs) registers a kqueue `EVFILT_PROC`/`NOTE_EXIT`
watch (`ExitWatch`) and sleeps on it for up to `CHECK_INTERVAL` (100 ms). It wakes at once when the command exits. Otherwise it
wakes every 100 ms to check `stop()` and the output-size cap. The watch is only a hint: `try_reap` (waitid with `WNOWAIT`,
then kill the group, then reap) still decides whether the child has exited, so the process-group safety logic is unchanged.
The sleep is cut to the time left before the timeout, so timeouts fire on time. If the watch cannot be created (or off macOS)
it polls every 15 ms as before.

Measured with a temporary test over the real `wait` (debug build, 3 runs each, removed afterwards):

| | Before | After |
|---|---|---|
| CPU while waiting on `sleep 10` | 0.57-0.59% of wall time | 0.11-0.14% |
| Wakeups while waiting | ~65/s | ~10/s |
| A short command (`true`, mean of 200) | 18.0-18.4 ms | 3.8-3.9 ms |
| Time from the stop flag to the kill | at most about 15-20 ms | at most about 100 ms (mean about 50 ms) |

The last row is the cost: Esc stops a running command up to 100 ms later. Lower `CHECK_INTERVAL` to 50 ms to halve that at
the price of about 10 more wakeups a second. A first, single-point measurement of the stop latency showed 7-15 ms. That was
misleading, and only a sweep across the 100 ms cycle shows the real worst case.

New tests: `a_timeout_fires_on_time_between_checks`, `a_command_that_exits_ends_the_wait_promptly`.

## Fix 2: do not redraw an idle screen

`Tick` only increments `App::tick`, which drives the status-line spinner, the retry countdown and the blinking glyph of a
running tool row. With nothing running, a tick redrew an identical screen. The loop now keeps a `dirty` flag and draws only
when the screen can have changed:

- any event other than `Tick` (`App::wants_redraw`);
- a `Tick` while `App::animating()`: a running turn, background jobs, a pending retry, or a tool row still marked running;
- a `sync_queue` that changed the queue, job count, job list, MCP status or row count. The store and the MCP manager change
  without sending an event, so `sync_queue` now reports whether it changed anything.

If `animating()` misses a case, the result is an extra redraw, not a frozen screen. A new test renders an idle app across ticks
and requires identical frames, and requires a running app's frames to differ on every tick. A tick-driven element added
without updating `animating()` fails it.

Measured on release builds, in a pty, after 15 s of settling, over a 20 s window, 3 runs per width, same harness and
settings before and after:

| Idle TUI | Before | After |
|---|---|---|
| CPU, 100 columns | 0.71-0.76% of a core | 0.05-0.09% |
| CPU, 130 columns (sidebar on) | 0.90-0.95% | 0.14-0.19% |
| Context switches | ~25/s | ~17/s |
| Bytes written to the terminal in 20 s | 3,753 (about 190 per second) | 0 |

`top` reports CPU time to 0.01 s, so the absolute CPU figures are coarse. The difference between before and after is far
outside the run-to-run spread. The remaining ~17 switches a second are the input thread (10) and the 150 ms receive timeout (7).
They do no work.

New tests: three unit tests in `tui/app.rs` and a pty scenario, `an_idle_screen_is_not_redrawn` (`tests/tui.rs`, scenario
`idle` in `tests/tui_pty.py`), which requires silence on the terminal at an idle prompt and after a finished turn, and checks
that typing still draws.

## Test results

On macOS 10.7.5, debug build, `cargo test -p moco`: 66 unit tests and the `logging`, `permissions_cli` and `protocols` tests pass.
`cargo test -p moco-core --lib -- tools::shell engine::jobs`: 40 passed. `tests/tui.rs`: 11 of 11 passed in two full runs
after the one failure described below.

## Known flake, not caused by this change

`a_hung_up_terminal_ends_moco_during_a_dialog` fails intermittently with `moco was still running 8 s after its terminal hung up
(cpu ~95%)`: the archive#87 symptom (crossterm spinning on a dead tty). It failed once in the first full `tui` run of this branch and
about 1 in 12 standalone runs, which raised the question of whether the redraw change caused it.

It did not. The original `HEAD` (68e9a52), built the same way and run under identical conditions (`a_hung_up`, which runs
this test and the idle-hangup test in parallel), failed 4 of 45 runs, with the same message. Run alone, 20 of 20 passed on
`HEAD`, so it needs the parallel load. This is a race in the input thread's hangup handling (`term::tty_gone` and crossterm's
reader), in code this change does not touch, and it should be filed and fixed separately.

## Not done, and not checked

- **Linux:** `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass. Clippy first flagged
  `drop(watch)` (`drop_non_drop`: the non-macOS `ExitWatch` stub, an uninhabited enum, has no `Drop`); it is allowed there.
- **Mojave:** not built or tested. Run `cargo test` there before merging. The release binary's `nm -u` and `otool -l` link checks
  from `14-macos-10.7.md` were not run either. `kqueue` and `kevent` exist on 10.7 and the Lion build links and runs.
- **Sidebar polling:** with the sidebar shown (automatic from 110 columns) or a turn or job active, `sync_queue` still runs every
  200 ms and queries the store (the inbox, and the job list). It no longer forces a redraw, but it still costs wakeups and
  SQLite calls. Waiting on a condition instead would remove it.
- **Input thread:** the 100 ms `event::poll` is unchanged. Raising it would add up to that much delay to noticing a dead
  terminal (archive#87) for about 10 fewer wakeups a second.
- **While a turn runs:** the 150 ms tick still redraws, because the spinner moves. Only the idle case was optimized.
- **Real use:** the numbers come from a scripted fake model and an empty session, not from a long transcript. Redraw cost grows
  with the amount of text on screen, so a full screen should save more per skipped redraw.
