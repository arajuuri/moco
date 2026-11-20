//! Terminal setup and restore: raw mode, alternate screen, bracketed paste.
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::os::unix::io::{AsRawFd, RawFd};
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
    let mut out = io::stdout();
    setup(&mut out)?;
    // Not `terminal.clear()`: it reads the cursor position from the terminal, which races the input thread for
    // stdin and fails (or stalls) on terminals that do not answer. Clear the screen ourselves and reset both
    // buffers so the next draw repaints everything.
    execute!(out, Clear(ClearType::All))?;
    terminal.swap_buffers();
    terminal.swap_buffers();
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

/// The terminal the input thread reads from, the way crossterm picks it: stdin when it is a tty, else `/dev/tty`.
/// The file (when one was opened) must outlive the fd's use.
pub fn input_tty() -> Option<(RawFd, Option<std::fs::File>)> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return Some((libc::STDIN_FILENO, None));
    }
    let f = std::fs::OpenOptions::new().read(true).open("/dev/tty").ok()?;
    Some((f.as_raw_fd(), Some(f)))
}

/// True when the terminal behind `fd` is gone (its other end closed: a dropped SSH connection, a killed `script`).
/// crossterm 0.27 must not be called then: its reader retries a read that returns 0 or an error forever, at full CPU
/// and without returning (issue archive#87).
///
/// Uses `select`, not `poll`: macOS `poll` does not work on ttys (it reports `POLLNVAL`). A tty that is readable but
/// has no bytes waiting is at end of file; one whose byte count cannot be read is hung up.
pub fn tty_gone(fd: RawFd) -> bool {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return false;
    }
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        let mut zero = libc::timeval { tv_sec: 0, tv_usec: 0 };
        let r = libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut zero);
        if r < 0 {
            // EBADF: the descriptor itself is gone. EINTR and the rest: try again next time.
            return io::Error::last_os_error().raw_os_error() == Some(libc::EBADF);
        }
        if r == 0 || !libc::FD_ISSET(fd, &set) {
            return false;
        }
        let mut waiting: libc::c_int = 0;
        libc::ioctl(fd, libc::FIONREAD, &mut waiting) != 0 || waiting == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pty pair from the calls every Unix has in libc (`openpty` needs libutil on some Linux systems).
    fn pty_pair() -> (RawFd, RawFd) {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt: {}", io::Error::last_os_error());
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
            let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_owned();
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "open {name:?}: {}", io::Error::last_os_error());
            (master, slave)
        }
    }

    #[test]
    fn a_tty_is_gone_only_once_its_other_end_has_closed() {
        let (master, slave) = pty_pair();
        assert!(!tty_gone(slave), "an open, idle terminal");
        unsafe {
            assert_eq!(libc::write(master, b"x\n".as_ptr().cast(), 2), 2);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!tty_gone(slave), "input is waiting");
        let mut buf = [0u8; 16];
        unsafe {
            libc::read(slave, buf.as_mut_ptr().cast(), buf.len());
            libc::close(master);
        }
        assert!(tty_gone(slave), "the other end closed (a dropped SSH connection)");
        unsafe {
            libc::close(slave);
        }
        assert!(!tty_gone(-1) && !tty_gone(libc::FD_SETSIZE as RawFd), "out of range is never reported as gone");
    }
}
