//! Terminal setup and restore: raw mode, alternate screen, bracketed paste.
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

/// Undo everything `enter` did. Safe to call twice.
pub fn restore() {
    // Pop the window title saved by `enter` (terminals that do not know the sequence ignore it).
    let _ = execute!(io::stdout(), DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen, Show);
    let _ = io::Write::write_all(&mut io::stdout(), b"\x1b[23;0t");
    let _ = io::Write::flush(&mut io::stdout());
    let _ = disable_raw_mode();
}

/// Restores the terminal when dropped, including on early returns.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Set by the panic hook when a thread other than the main one panicked (and the terminal was restored).
static PANICKED_OFF_MAIN: AtomicBool = AtomicBool::new(false);

fn setup(out: &mut Stdout) -> io::Result<()> {
    enable_raw_mode()?;
    io::Write::write_all(out, b"\x1b[22;0t")?; // push the current window title
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange, SetTitle("moco"))
}

/// After a worker-thread panic the hook has restored the terminal; switch the UI back on. Returns whether it did.
pub fn reenter_after_panic(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<bool> {
    if !PANICKED_OFF_MAIN.swap(false, Ordering::SeqCst) {
        return Ok(false);
    }
    setup(&mut io::stdout())?;
    terminal.clear()?;
    Ok(true)
}

pub fn enter() -> io::Result<(Terminal<CrosstermBackend<Stdout>>, Guard)> {
    let guard = Guard;
    let mut out = io::stdout();
    setup(&mut out)?;
    // A panic on any thread must leave the terminal usable before the message is printed. A panic off the main
    // thread (the engine worker) is caught and reported in the UI, so the main loop re-enters the screen afterwards.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("main") {
            PANICKED_OFF_MAIN.store(true, Ordering::SeqCst);
        }
        restore();
        previous(info);
    }));
    Ok((Terminal::new(CrosstermBackend::new(out))?, guard))
}
