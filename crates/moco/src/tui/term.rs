//! Terminal setup and restore: raw mode, alternate screen, bracketed paste.
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};

/// Undo everything `enter` did. Safe to call twice.
pub fn restore() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
}

/// Restores the terminal when dropped, including on early returns.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

pub fn enter() -> io::Result<(Terminal<CrosstermBackend<Stdout>>, Guard)> {
    enable_raw_mode()?;
    let guard = Guard;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    // A panic on the main thread must leave the terminal usable before the message is printed.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore();
        }
        previous(info);
    }));
    Ok((Terminal::new(CrosstermBackend::new(out))?, guard))
}
