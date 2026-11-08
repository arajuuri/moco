//! Back-ports of standard-library methods newer than Rust 1.73, the last toolchain that targets macOS 10.7 (Lion).
//! Call sites stay identical to `main`; bring them into scope with `use crate::compat::*;`
//! (`use moco_core::compat::*;` in the front end). On a newer compiler the inherent methods win and these are unused.
#![allow(dead_code, clippy::wrong_self_convention)]

/// `Option::is_none_or` (stable in 1.82).
pub trait OptionIsNoneOr<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool;
}

impl<T> OptionIsNoneOr<T> for Option<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool {
        match self {
            None => true,
            Some(x) => f(x),
        }
    }
}

/// `Option::inspect` (stable in 1.76).
pub trait OptionInspect<T> {
    fn inspect(self, f: impl FnOnce(&T)) -> Self;
}

impl<T> OptionInspect<T> for Option<T> {
    fn inspect(self, f: impl FnOnce(&T)) -> Self {
        if let Some(x) = &self {
            f(x);
        }
        self
    }
}

/// `Result::inspect` and `Result::inspect_err` (stable in 1.76).
pub trait ResultInspect<T, E> {
    fn inspect(self, f: impl FnOnce(&T)) -> Self;
    fn inspect_err(self, f: impl FnOnce(&E)) -> Self;
}

impl<T, E> ResultInspect<T, E> for Result<T, E> {
    fn inspect(self, f: impl FnOnce(&T)) -> Self {
        if let Ok(x) = &self {
            f(x);
        }
        self
    }
    fn inspect_err(self, f: impl FnOnce(&E)) -> Self {
        if let Err(e) = &self {
            f(e);
        }
        self
    }
}

/// Unsigned `is_multiple_of` (stable in 1.87). Like std, `x.is_multiple_of(0)` is true only for `x == 0`.
pub trait IsMultipleOf: Sized {
    fn is_multiple_of(self, rhs: Self) -> bool;
}

macro_rules! is_multiple_of {
    ($($t:ty),*) => {$(
        impl IsMultipleOf for $t {
            fn is_multiple_of(self, rhs: Self) -> bool {
                if rhs == 0 { self == 0 } else { self % rhs == 0 }
            }
        }
    )*};
}
is_multiple_of!(u8, u16, u32, u64, u128, usize);

/// `File::set_modified` (stable in 1.75). Uses `futimes` (available since 10.0; `futimens` needs 10.13) and keeps the
/// current access time.
pub trait FileSetModified {
    fn set_modified(&self, t: std::time::SystemTime) -> std::io::Result<()>;
}

impl FileSetModified for std::fs::File {
    fn set_modified(&self, t: std::time::SystemTime) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;
        fn timeval(t: std::time::SystemTime) -> libc::timeval {
            let (secs, usecs) = match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => (d.as_secs() as i64, d.subsec_micros() as i64),
                Err(e) => {
                    // Before the epoch: floor to whole seconds, then add the positive microsecond part.
                    let d = e.duration();
                    let (s, us) = (d.as_secs() as i64, d.subsec_micros() as i64);
                    if us == 0 { (-s, 0) } else { (-s - 1, 1_000_000 - us) }
                }
            };
            libc::timeval { tv_sec: secs as libc::time_t, tv_usec: usecs as libc::suseconds_t }
        }
        let accessed = self.metadata()?.accessed()?;
        let times = [timeval(accessed), timeval(t)];
        // SAFETY: the fd is open for the lifetime of `self` and `times` is a valid two-element array.
        if unsafe { libc::futimes(self.as_raw_fd(), times.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_and_result_helpers() {
        assert!(None::<u8>.is_none_or(|_| false));
        assert!(Some(2).is_none_or(|x| x == 2));
        assert!(!Some(3).is_none_or(|x| x == 2));
        let mut seen = 0;
        let r: Result<u8, u8> = Err(7);
        let _ = r.inspect_err(|e| seen = *e);
        assert_eq!(seen, 7);
        assert!(0usize.is_multiple_of(0) && !3usize.is_multiple_of(0) && 9usize.is_multiple_of(3) && !10u32.is_multiple_of(3));
    }

    #[test]
    fn set_modified_sets_mtime() {
        let dir = std::env::temp_dir().join(format!("moco-compat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        std::fs::write(&p, b"x").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 24 * 3600);
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        FileSetModified::set_modified(&f, past).unwrap();
        let got = std::fs::metadata(&p).unwrap().modified().unwrap();
        let diff = got.duration_since(past).unwrap_or_else(|e| e.duration());
        assert!(diff < std::time::Duration::from_secs(1), "{diff:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
