//! Stand-ins for the C library functions that Rust 1.78's std imports but macOS 10.7 (Lion) lacks: `getentropy` and
//! `clock_gettime`, both 10.12+ (issue archive#100). Defining them in the binary makes the linker bind std's calls here instead
//! of importing them from libSystem, so dyld on Lion finds nothing missing. System libraries are not affected: with
//! two-level namespaces their own calls stay bound to libSystem.
//!
//! On 10.12 and newer each function forwards to the system's own, looked up in libSystem on first use. On older systems
//! it falls back to `/dev/urandom` and `gettimeofday`/`mach_absolute_time`. The `nm -u` check in docs/14-macos-10.7.md
//! catches any further import a toolchain or dependency update brings.
use libc::{c_int, c_void, clockid_t, size_t, timespec};
use std::sync::atomic::{AtomicUsize, Ordering};

const UNRESOLVED: usize = 0;
const MISSING: usize = 1;

/// The address of libSystem's `name` (NUL-terminated), cached in `cache`; None before 10.12.
fn system_fn(cache: &AtomicUsize, name: &[u8]) -> Option<usize> {
    let mut f = cache.load(Ordering::Relaxed);
    if f == UNRESOLVED {
        // Look in libSystem and what it re-exports, not the global namespace: there the first match is our own definition.
        let lib = unsafe { libc::dlopen(b"/usr/lib/libSystem.B.dylib\0".as_ptr().cast(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
        let p = if lib.is_null() { std::ptr::null_mut() } else { unsafe { libc::dlsym(lib, name.as_ptr().cast()) } };
        f = if p.is_null() { MISSING } else { p as usize };
        cache.store(f, Ordering::Relaxed);
    }
    (f != MISSING).then_some(f)
}

fn set_errno(e: c_int) {
    unsafe { *libc::__error() = e };
}

/// # Safety
/// `buf` must be valid for writes of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn getentropy(buf: *mut c_void, len: size_t) -> c_int {
    static SYS: AtomicUsize = AtomicUsize::new(UNRESOLVED);
    match system_fn(&SYS, b"getentropy\0") {
        Some(f) => std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void, size_t) -> c_int>(f)(buf, len),
        None => fallback_getentropy(buf, len),
    }
}

/// `getentropy` from `/dev/urandom`, with the system call's 256-byte limit.
unsafe fn fallback_getentropy(buf: *mut c_void, len: size_t) -> c_int {
    if len > 256 {
        set_errno(libc::EIO);
        return -1;
    }
    let fd = libc::open(b"/dev/urandom\0".as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return -1;
    }
    let (mut p, mut left) = (buf.cast::<u8>(), len);
    while left > 0 {
        let n = libc::read(fd, p.cast(), left);
        if n < 0 && *libc::__error() == libc::EINTR {
            continue;
        }
        if n <= 0 {
            let e = if n == 0 { libc::EIO } else { *libc::__error() };
            libc::close(fd);
            set_errno(e);
            return -1;
        }
        p = p.add(n as usize);
        left -= n as usize;
    }
    libc::close(fd);
    0
}

/// # Safety
/// `ts` must be valid for a write of one `timespec`.
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clock: clockid_t, ts: *mut timespec) -> c_int {
    static SYS: AtomicUsize = AtomicUsize::new(UNRESOLVED);
    match system_fn(&SYS, b"clock_gettime\0") {
        Some(f) => std::mem::transmute::<usize, unsafe extern "C" fn(clockid_t, *mut timespec) -> c_int>(f)(clock, ts),
        None => fallback_clock_gettime(clock, ts),
    }
}

/// `clock_gettime` for the clocks std uses (`CLOCK_REALTIME` for `SystemTime`, `CLOCK_UPTIME_RAW` for `Instant`) and
/// their relatives. The monotonic clocks all read `mach_absolute_time`, which stops during sleep; on 10.12+
/// `CLOCK_MONOTONIC` counts sleep, but nothing in moco relies on that.
#[allow(deprecated)] // libc's mach_* items point to the mach2 crate; the functions themselves are fine
unsafe fn fallback_clock_gettime(clock: clockid_t, ts: *mut timespec) -> c_int {
    match clock {
        libc::CLOCK_REALTIME => {
            let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
            if libc::gettimeofday(&mut tv, std::ptr::null_mut()) != 0 {
                return -1;
            }
            *ts = timespec { tv_sec: tv.tv_sec, tv_nsec: tv.tv_usec as libc::c_long * 1000 };
            0
        }
        libc::CLOCK_MONOTONIC_RAW | libc::CLOCK_MONOTONIC_RAW_APPROX | libc::CLOCK_MONOTONIC | libc::CLOCK_UPTIME_RAW | libc::CLOCK_UPTIME_RAW_APPROX => {
            static TIMEBASE: AtomicUsize = AtomicUsize::new(0); // numer << 32 | denom
            let mut tb = TIMEBASE.load(Ordering::Relaxed);
            if tb == 0 {
                let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
                libc::mach_timebase_info(&mut info);
                tb = (info.numer as usize) << 32 | info.denom as usize;
                TIMEBASE.store(tb, Ordering::Relaxed);
            }
            let ns = libc::mach_absolute_time() as u128 * (tb >> 32) as u128 / (tb & 0xffff_ffff) as u128;
            *ts = timespec { tv_sec: (ns / 1_000_000_000) as libc::time_t, tv_nsec: (ns % 1_000_000_000) as libc::c_long };
            0
        }
        _ => {
            set_errno(libc::EINVAL);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(clock: clockid_t, f: unsafe fn(clockid_t, *mut timespec) -> c_int) -> i128 {
        let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
        assert_eq!(unsafe { f(clock, &mut ts) }, 0, "clock {clock}");
        assert!((0..1_000_000_000).contains(&ts.tv_nsec));
        ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
    }

    unsafe fn exported(clock: clockid_t, ts: *mut timespec) -> c_int {
        clock_gettime(clock, ts)
    }

    #[test]
    fn fallback_clocks_agree_with_the_exported_ones() {
        let ms = 1_000_000;
        let (sys, ours) = (now(libc::CLOCK_REALTIME, exported), now(libc::CLOCK_REALTIME, fallback_clock_gettime));
        assert!((ours - sys).abs() < 100 * ms, "realtime {sys} vs {ours}");
        // mach_absolute_time is what CLOCK_UPTIME_RAW reads, so the two agree closely.
        let (sys, ours) = (now(libc::CLOCK_UPTIME_RAW, exported), now(libc::CLOCK_UPTIME_RAW, fallback_clock_gettime));
        assert!((ours - sys).abs() < 100 * ms, "uptime {sys} vs {ours}");
        let a = now(libc::CLOCK_MONOTONIC_RAW, fallback_clock_gettime);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = now(libc::CLOCK_MONOTONIC_RAW, fallback_clock_gettime);
        assert!(b - a >= 20 * ms, "monotonic moves forward: {a} {b}");
        let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
        assert_eq!(unsafe { fallback_clock_gettime(12345, &mut ts) }, -1, "unknown clocks are refused");
    }

    #[test]
    fn fallback_entropy_fills_the_buffer_and_keeps_the_limit() {
        let mut a = [0u8; 256];
        let mut b = [0u8; 256];
        assert_eq!(unsafe { fallback_getentropy(a.as_mut_ptr().cast(), a.len()) }, 0);
        assert_eq!(unsafe { fallback_getentropy(b.as_mut_ptr().cast(), b.len()) }, 0);
        assert!(a != b && a != [0; 256]);
        let mut big = [0u8; 257];
        assert_eq!(unsafe { fallback_getentropy(big.as_mut_ptr().cast(), big.len()) }, -1);
        assert_eq!(unsafe { getentropy(a.as_mut_ptr().cast(), 16) }, 0, "the exported function works too");
    }
}
