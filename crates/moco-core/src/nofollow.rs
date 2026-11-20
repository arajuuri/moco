//! Using a file by the path a permission check approved, without following a symlink swapped in after the check (archive#165).
//!
//! The file tools decide on a symlink-free path (`tools::real_path`, resolved once, before the check) and then use
//! that path. Between the two, another process (a background shell, say) could replace the file or one of its folders
//! with a symlink to somewhere the check never saw. Everything here refuses a symlink anywhere in the path instead of
//! following it, and checks with `fstat` that what it opened is the kind of file it expected.
//!
//! * Where the system has `openat` (Linux; macOS 10.10 and later, looked up at run time because 10.7 to 10.9 lack the
//!   `*at` calls), a folder is opened component by component from `/`, each step relative to the folder opened before
//!   and with `O_NOFOLLOW`. Files are then opened, created, renamed and removed relative to that open folder, so no
//!   name is looked up twice.
//! * Without `openat` (macOS 10.7 to 10.9, or a folder on the way that cannot be opened for reading), everything goes
//!   by full path: the final component is opened with `O_NOFOLLOW`, and after each operation the folder is checked
//!   again: no component is a symlink, the folder is the one first seen (same device and inode), and an opened file is
//!   the one now at its name. That leaves a window only for a folder swapped out and back between two system calls.
use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

type OpenAt = unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int, ...) -> libc::c_int;
type RenameAt = unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int, *const libc::c_char) -> libc::c_int;
type UnlinkAt = unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int) -> libc::c_int;
type MkdirAt = unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::mode_t) -> libc::c_int;

struct AtFns {
    open: OpenAt,
    rename: RenameAt,
    unlink: UnlinkAt,
    mkdir: MkdirAt,
}

/// The `*at` calls, if this system has them.
fn at_fns() -> Option<&'static AtFns> {
    #[cfg(test)]
    if FORCE_PATHS.with(|f| f.get()) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        static FNS: AtFns = AtFns { open: libc::openat, rename: libc::renameat, unlink: libc::unlinkat, mkdir: libc::mkdirat };
        Some(&FNS)
    }
    #[cfg(target_os = "macos")]
    {
        // Linking them directly would stop the binary from loading on 10.7 (they arrived in 10.10).
        static FNS: std::sync::OnceLock<Option<AtFns>> = std::sync::OnceLock::new();
        FNS.get_or_init(|| unsafe {
            let sym = |name: &std::ffi::CStr| libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            let (o, r, u, m) = (sym(c"openat"), sym(c"renameat"), sym(c"unlinkat"), sym(c"mkdirat"));
            if o.is_null() || r.is_null() || u.is_null() || m.is_null() {
                return None;
            }
            Some(AtFns { open: std::mem::transmute::<*mut libc::c_void, OpenAt>(o), rename: std::mem::transmute::<*mut libc::c_void, RenameAt>(r), unlink: std::mem::transmute::<*mut libc::c_void, UnlinkAt>(u), mkdir: std::mem::transmute::<*mut libc::c_void, MkdirAt>(m) })
        })
        .as_ref()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    None
}

// Tests run the full-path fallback (what 10.7 to 10.9 use) on any system.
#[cfg(test)]
thread_local! {
    pub(crate) static FORCE_PATHS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BETWEEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Tests: run `f` once, the next time a file tool (or snapshot capture) passes from its check to its use, on this
/// thread. This is where a racing process would swap in a symlink.
#[cfg(test)]
pub(crate) fn on_next_use(f: impl FnOnce() + 'static) {
    BETWEEN.with(|b| *b.borrow_mut() = Some(Box::new(f)));
}

/// Tests: run `f` with `openat` (where the system has it) and again with the full-path fallback 10.7 to 10.9 use.
#[cfg(test)]
pub(crate) fn both_modes(f: impl Fn()) {
    f();
    FORCE_PATHS.with(|p| p.set(true));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&f));
    FORCE_PATHS.with(|p| p.set(false));
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

/// The point between a permission check (or a `stat`) and the use of the file it judged. Does nothing outside tests.
pub(crate) fn between_check_and_use() {
    #[cfg(test)]
    if let Some(f) = BETWEEN.with(|b| b.borrow_mut().take()) {
        f();
    }
}

fn swapped(path: &Path) -> io::Error {
    io::Error::other(format!("{} is now reached through a symlink that was not there when access was checked; not following it", path.display()))
}

fn cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// One component name: no `/`, not `.` or `..`.
fn single(name: &OsStr) -> io::Result<&OsStr> {
    let mut c = Path::new(name).components();
    match (c.next(), c.next()) {
        (Some(Component::Normal(n)), None) if n == name => Ok(n),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a single file name", Path::new(name).display()))),
    }
}

fn id(m: &Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}

/// Folders on the way are opened just to look up the next name: on Linux without needing read permission.
#[cfg(target_os = "linux")]
const WALK_FLAGS: libc::c_int = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const WALK_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// A folder, opened without following symlinks.
pub struct Dir {
    path: PathBuf,
    /// Open handle the `*at` calls work relative to; `None` in the full-path fallback.
    fd: Option<File>,
    /// Device and inode of the folder when it was opened.
    id: (u64, u64),
}

impl Dir {
    /// Open the folder `path`, which must be absolute and symlink-free (as `tools::real_path` returns it). A symlink
    /// anywhere in it is refused.
    pub fn open(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, false)
    }

    /// `open`, creating missing folders (as `create_dir_all` does) without following a symlink on the way.
    pub fn create(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, true)
    }

    fn open_with(path: &Path, create: bool) -> io::Result<Dir> {
        if !path.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not an absolute path", path.display())));
        }
        if let Some(at) = at_fns() {
            match Self::walk(path, create, at) {
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {} // a folder we may search but not read: by path
                r => return r,
            }
        }
        let mut cur = PathBuf::from("/");
        for c in path.components() {
            match c {
                Component::RootDir => {}
                Component::Normal(name) => {
                    cur.push(name);
                    match std::fs::symlink_metadata(&cur) {
                        Ok(m) if m.file_type().is_symlink() => return Err(swapped(&cur)),
                        Ok(m) if !m.is_dir() => return Err(io::Error::other(format!("{}: not a directory", cur.display()))),
                        Ok(_) => {}
                        Err(e) if create && e.kind() == io::ErrorKind::NotFound => match std::fs::create_dir(&cur) {
                            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                            _ => continue_after_mkdir(&cur)?,
                        },
                        Err(e) => return Err(e),
                    }
                }
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a normalized path", path.display()))),
            }
        }
        let id = id(&std::fs::symlink_metadata(path)?);
        let dir = Dir { path: path.to_path_buf(), fd: None, id };
        dir.check()?;
        Ok(dir)
    }

    fn walk(path: &Path, create: bool, at: &AtFns) -> io::Result<Dir> {
        let mut cur = File::open("/")?;
        let mut shown = PathBuf::from("/");
        for c in path.components() {
            let name = match c {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a normalized path", path.display()))),
            };
            shown.push(name);
            let cname = cstr(name)?;
            let mut fd = unsafe { (at.open)(cur.as_raw_fd(), cname.as_ptr(), WALK_FLAGS) };
            if fd < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
                if unsafe { (at.mkdir)(cur.as_raw_fd(), cname.as_ptr(), 0o777) } < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        return Err(e);
                    }
                }
                fd = unsafe { (at.open)(cur.as_raw_fd(), cname.as_ptr(), WALK_FLAGS) };
            }
            if fd < 0 {
                let e = io::Error::last_os_error();
                return Err(if e.raw_os_error() == Some(libc::ELOOP) { swapped(&shown) } else { e });
            }
            let next = unsafe { File::from_raw_fd(fd) };
            let m = next.metadata()?;
            if m.file_type().is_symlink() {
                return Err(swapped(&shown)); // Linux: O_PATH | O_NOFOLLOW opens the link itself
            }
            if !m.is_dir() {
                return Err(io::Error::other(format!("{}: not a directory", shown.display())));
            }
            cur = next;
        }
        let id = id(&cur.metadata()?);
        Ok(Dir { path: path.to_path_buf(), fd: Some(cur), id })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Full-path fallback: the folder is still the one opened and no component of it has become a symlink.
    fn check(&self) -> io::Result<()> {
        if self.fd.is_some() {
            return Ok(());
        }
        let mut cur = PathBuf::from("/");
        for c in self.path.components() {
            if let Component::Normal(name) = c {
                cur.push(name);
                if std::fs::symlink_metadata(&cur)?.file_type().is_symlink() {
                    return Err(swapped(&cur));
                }
            }
        }
        if id(&std::fs::symlink_metadata(&self.path)?) != self.id {
            return Err(swapped(&self.path));
        }
        Ok(())
    }

    /// Whether the folder at `path` is still this one (a listing read by name can then be trusted).
    pub fn still_there(&self) -> io::Result<()> {
        let again = Dir::open(&self.path)?;
        if again.id != self.id {
            return Err(swapped(&self.path));
        }
        Ok(())
    }

    /// Open `name` in this folder with `flags` (`O_NOFOLLOW` and `O_CLOEXEC` are added). A symlink there is refused.
    #[allow(clippy::useless_conversion)] // `mode_t` is u32 on Linux, u16 on macOS
    pub fn open_file(&self, name: &OsStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
        let name = single(name)?;
        let full = self.path.join(name);
        let file = match (&self.fd, at_fns()) {
            (Some(dir), Some(at)) => {
                let cname = cstr(name)?;
                let fd = unsafe { (at.open)(dir.as_raw_fd(), cname.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, libc::c_uint::from(mode)) };
                if fd < 0 {
                    let e = io::Error::last_os_error();
                    return Err(if e.raw_os_error() == Some(libc::ELOOP) { swapped(&full) } else { e });
                }
                unsafe { File::from_raw_fd(fd) }
            }
            _ => {
                let mut opts = std::fs::OpenOptions::new();
                // std picks the access mode from read/write; the rest goes through custom_flags.
                let access = flags & libc::O_ACCMODE;
                opts.read(access == libc::O_RDONLY || access == libc::O_RDWR).write(access != libc::O_RDONLY);
                opts.custom_flags((flags & !libc::O_ACCMODE) | libc::O_NOFOLLOW).mode(u32::from(mode));
                let file = opts.open(&full).map_err(|e| if e.raw_os_error() == Some(libc::ELOOP) { swapped(&full) } else { e })?;
                // The folder may have been swapped for a link and back around the open: then the name no longer
                // leads to the file we opened.
                self.check()?;
                if id(&std::fs::symlink_metadata(&full)?) != id(&file.metadata()?) {
                    return Err(swapped(&full));
                }
                file
            }
        };
        Ok(file)
    }

    /// Open `name` for reading. It must be a regular file (a FIFO or device is refused without blocking on it).
    pub fn open_read(&self, name: &OsStr) -> io::Result<(File, Metadata)> {
        let file = self.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::other(format!("{} is not a regular file", self.path.join(name).display())));
        }
        Ok((file, meta))
    }

    /// Rename `from` to `to`, both in this folder. Neither is followed if it is a symlink.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let (from, to) = (single(from)?, single(to)?);
        match (&self.fd, at_fns()) {
            (Some(dir), Some(at)) => {
                let (f, t) = (cstr(from)?, cstr(to)?);
                if unsafe { (at.rename)(dir.as_raw_fd(), f.as_ptr(), dir.as_raw_fd(), t.as_ptr()) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            _ => {
                self.check()?;
                std::fs::rename(self.path.join(from), self.path.join(to))
            }
        }
    }

    /// Remove `name` from this folder. A symlink is removed itself; what it points to is left alone.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        let name = single(name)?;
        match (&self.fd, at_fns()) {
            (Some(dir), Some(at)) => {
                let n = cstr(name)?;
                if unsafe { (at.unlink)(dir.as_raw_fd(), n.as_ptr(), 0) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            _ => {
                self.check()?;
                std::fs::remove_file(self.path.join(name))
            }
        }
    }
}

/// `create_dir` in the fallback succeeded or raced with another creator; what is there now must be a real folder.
fn continue_after_mkdir(p: &Path) -> io::Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    if m.file_type().is_symlink() {
        return Err(swapped(p));
    }
    Ok(())
}

/// The folder and file name of a symlink-free file path.
fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => Ok((dir, name)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a file path", path.display()))),
    }
}

/// Open the regular file at the symlink-free `path` for reading, refusing a symlink anywhere in it.
pub fn open_read(path: &Path) -> io::Result<(File, Metadata)> {
    let (dir, name) = split(path)?;
    Dir::open(dir)?.open_read(name)
}

/// The content of the regular file at the symlink-free `path`, refusing a symlink anywhere in it.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let (mut file, meta) = open_read(path)?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Remove the file or symlink at `path` (its folder symlink-free), never what a symlink there points to.
pub fn remove_file(path: &Path) -> io::Result<()> {
    let (dir, name) = split(path)?;
    Dir::open(dir)?.remove_file(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id(tag));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    fn both(f: impl Fn()) {
        both_modes(f)
    }

    #[test]
    fn plain_files_read_create_rename_and_remove() {
        both(|| {
            let d = tmp("nofollow-plain");
            std::fs::write(d.join("a.txt"), "a").unwrap();
            assert_eq!(read(&d.join("a.txt")).unwrap(), b"a");
            let dir = Dir::create(&d.join("x/y")).unwrap();
            assert!(d.join("x/y").is_dir());
            drop(dir.open_file(OsStr::new("n"), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o644).unwrap());
            dir.rename(OsStr::new("n"), OsStr::new("m")).unwrap();
            assert!(d.join("x/y/m").is_file() && !d.join("x/y/n").exists());
            remove_file(&d.join("x/y/m")).unwrap();
            assert!(!d.join("x/y/m").exists());
            assert!(Dir::open(&d.join("x/y")).unwrap().open_file(OsStr::new("../a.txt"), libc::O_RDONLY, 0).is_err(), "one name only");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_symlink_anywhere_in_the_path_is_refused() {
        both(|| {
            let d = tmp("nofollow-link");
            std::fs::create_dir_all(d.join("out")).unwrap();
            std::fs::write(d.join("out/secret"), "s").unwrap();
            symlink(d.join("out/secret"), d.join("file")).unwrap();
            symlink(d.join("out"), d.join("dir")).unwrap();
            let e = read(&d.join("file")).unwrap_err().to_string();
            assert!(e.contains("symlink"), "{e}");
            let e = read(&d.join("dir/secret")).unwrap_err().to_string();
            assert!(e.contains("symlink"), "{e}");
            assert!(Dir::create(&d.join("dir/new")).is_err());
            assert!(!d.join("out/new").exists(), "no folder created through the link");
            // A FIFO is not opened for a blocking read.
            let fifo = cstr(d.join("fifo").as_os_str()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            assert!(read(&d.join("fifo")).unwrap_err().to_string().contains("not a regular file"));
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn removing_a_symlink_leaves_its_target() {
        both(|| {
            let d = tmp("nofollow-rm");
            std::fs::write(d.join("target"), "keep").unwrap();
            symlink(d.join("target"), d.join("link")).unwrap();
            remove_file(&d.join("link")).unwrap();
            assert!(std::fs::symlink_metadata(d.join("link")).is_err());
            assert_eq!(std::fs::read_to_string(d.join("target")).unwrap(), "keep");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_folder_swapped_for_a_link_after_it_was_opened_is_not_followed() {
        both(|| {
            let d = tmp("nofollow-swap");
            std::fs::create_dir_all(d.join("proj/sub")).unwrap();
            std::fs::create_dir_all(d.join("out")).unwrap();
            std::fs::write(d.join("out/f"), "secret").unwrap();
            std::fs::write(d.join("proj/sub/f"), "mine").unwrap();
            let dir = Dir::open(&d.join("proj/sub")).unwrap();
            std::fs::rename(d.join("proj/sub"), d.join("proj/moved")).unwrap();
            symlink(d.join("out"), d.join("proj/sub")).unwrap();
            match dir.open_read(OsStr::new("f")) {
                // With an open folder the read stays in the folder that was opened.
                Ok((mut f, _)) => {
                    let mut s = String::new();
                    f.read_to_string(&mut s).unwrap();
                    assert_eq!(s, "mine");
                }
                Err(e) => assert!(e.to_string().contains("symlink"), "{e}"),
            }
            // Removing stays in the opened folder too (or is refused): the file the link leads to is untouched.
            let _ = dir.remove_file(OsStr::new("f"));
            assert_eq!(std::fs::read_to_string(d.join("out/f")).unwrap(), "secret");
            assert!(dir.still_there().is_err());
            let _ = std::fs::remove_dir_all(d);
        });
    }
}
