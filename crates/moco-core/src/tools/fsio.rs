//! File writes shared by `edit`, `write` and `patch`.
//!
//! * **One writer per file at a time.** The tools read a file, change it in memory and write it back; models often send
//!   several `edit` calls for the same file in one step and the engine runs them on separate threads, so without a lock
//!   the second write silently drops the first (issue archive#6). `lock_files` serializes everything between the read and the
//!   end of formatting.
//! * **Replace by rename.** `write_atomic` writes a temporary file next to the target and renames it over the target, so
//!   a crash or a kill in the middle never leaves a truncated user file (`std::fs::write` truncates in place).
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// `true` while some call holds the file.
type Cell = (Mutex<bool>, Condvar);

static LOCKS: Mutex<BTreeMap<PathBuf, Arc<Cell>>> = Mutex::new(BTreeMap::new());

/// The locks of one tool call; released when dropped.
pub struct FileLocks(Vec<Arc<Cell>>);

impl Drop for FileLocks {
    fn drop(&mut self) {
        for cell in &self.0 {
            *cell.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
            cell.1.notify_one();
        }
    }
}

/// The name two spellings of one file share: symlinks resolved as far as the path exists.
fn key(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent().and_then(|p| std::fs::canonicalize(p).ok()), path.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

/// Wait until no other call is working on any of `paths`, then hold them all. Several files are taken in a fixed
/// order, so two calls that touch the same files cannot wait for each other. Take the locks after any permission
/// prompt (a prompt can take minutes) and hold them until the file is written and formatted.
pub fn lock_files<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> FileLocks {
    let mut keys: Vec<PathBuf> = paths.into_iter().map(|p| key(p.as_ref())).collect();
    keys.sort();
    keys.dedup();
    let mut held = Vec::with_capacity(keys.len());
    for k in keys {
        let cell = {
            let mut all = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
            all.retain(|_, c| Arc::strong_count(c) > 1); // forget files nobody holds or waits for
            all.entry(k).or_default().clone()
        };
        let mut busy = cell.0.lock().unwrap_or_else(|e| e.into_inner());
        while *busy {
            busy = cell.1.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy = true;
        drop(busy);
        held.push(cell);
    }
    FileLocks(held)
}

static TMP: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` by renaming a finished temporary file over it. The file keeps its permissions. `path` is the
/// symlink-free path the permission check approved (`real_path`, so a file reached through a symlink is written
/// through to its target): the temporary file and the rename go to that folder, opened without following symlinks,
/// and a symlink found in the path now (swapped in after the check) is refused rather than followed or replaced
/// (archive#165). When the directory does not allow new files the bytes are written in place, as before.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{}: not a file path", path.display())));
    };
    let dir = crate::nofollow::Dir::open(dir)?;
    // The file being replaced, opened without following a link, for its mode. Opening only for writing covers a
    // write-only file; one we can neither read nor write is replaced with the default mode.
    let existing = match dir.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => dir.open_file(name, libc::O_WRONLY | libc::O_NONBLOCK, 0),
        r => r,
    };
    let mode = match existing {
        Ok(f) => Some(f.metadata()?.permissions()),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => None,
        Err(e) => return Err(e),
    };
    let tmp = std::ffi::OsString::from(format!(".{}.moco-{}-{}.tmp", name.to_string_lossy(), std::process::id(), TMP.fetch_add(1, Ordering::Relaxed)));
    let mut file = match dir.open_file(&tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o666) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Truncated only once `open_file` has confirmed this is the file at the checked path.
            let mut f = dir.open_file(name, libc::O_WRONLY | libc::O_CREAT | libc::O_NONBLOCK, 0o666)?;
            f.set_len(0)?;
            return f.write_all(bytes);
        }
        Err(e) => return Err(e),
    };
    let result = (|| {
        file.write_all(bytes)?;
        if let Some(mode) = mode {
            file.set_permissions(mode)?;
        }
        file.sync_all()?;
        dir.rename(&tmp, name)
    })();
    drop(file);
    if result.is_err() {
        let _ = dir.remove_file(&tmp);
    }
    result
}

/// Read one line (up to and including `\n`) into `buf` (cleared first), keeping at most `cap` bytes of it. The rest of
/// a longer line is read and thrown away, so a single-line multi-hundred-MB file costs `cap` bytes of memory.
/// Returns the bytes consumed from the reader (0 at end of file) and whether the line was cut short.
pub fn read_line_capped<R: std::io::BufRead>(r: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<(usize, bool)> {
    buf.clear();
    let (mut consumed, mut cut) = (0usize, false);
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok((consumed, cut));
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let room = cap.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..take.min(room)]);
        cut |= take > room;
        r.consume(take);
        consumed += take;
        if done {
            return Ok((consumed, cut));
        }
    }
}

/// How many changed lines `changes` shows.
const MAX_SHOWN: usize = 30;

/// The lines a formatter added and removed, numbered as in the file after formatting, for the tool's output. `None`
/// when there is nothing to show (identical, or not text).
pub fn changes(before: &[u8], after: &[u8]) -> Option<String> {
    let (b, a) = (std::str::from_utf8(before).ok()?, std::str::from_utf8(after).ok()?);
    let diff = similar::TextDiff::from_lines(b, a);
    let mut shown = vec![];
    let mut more = 0usize;
    for c in diff.iter_all_changes() {
        let (sign, n) = match c.tag() {
            similar::ChangeTag::Equal => continue,
            similar::ChangeTag::Delete => ('-', c.old_index().map_or(0, |i| i + 1)),
            similar::ChangeTag::Insert => ('+', c.new_index().map_or(0, |i| i + 1)),
        };
        if shown.len() < MAX_SHOWN {
            let line: String = c.value().trim_end_matches(['\n', '\r']).chars().take(200).collect();
            shown.push(format!("{sign}{n}: {line}"));
        } else {
            more += 1;
        }
    }
    if shown.is_empty() {
        return None;
    }
    if more > 0 {
        shown.push(format!("... and {more} more changed lines"));
    }
    Some(shown.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id(tag));
        std::fs::create_dir_all(&d).unwrap();
        // Symlink-free, as the tools pass it (on macOS the temp dir is under the /var -> /private/var link).
        std::fs::canonicalize(d).unwrap()
    }

    #[test]
    fn an_atomic_write_replaces_the_content_keeps_the_mode_and_leaves_nothing_behind() {
        let dir = tmp("fsio-mode");
        let p = dir.join("run.sh");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&p, b"new content").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new content");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o750);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["run.sh"], "no temporary file is left");
        write_atomic(&dir.join("new.txt"), b"fresh").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "fresh");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_symlink_at_the_path_is_refused_not_followed_or_replaced() {
        // The tools pass the resolved path; a link there now was swapped in after their check (archive#165).
        let dir = tmp("fsio-link");
        std::fs::write(dir.join("real.txt"), "old").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        let e = write_atomic(&dir.join("link.txt"), b"new").unwrap_err().to_string();
        assert!(e.contains("symlink"), "{e}");
        assert!(std::fs::symlink_metadata(dir.join("link.txt")).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(std::fs::read_to_string(dir.join("real.txt")).unwrap(), "old");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left.len(), 2, "no temporary file is left: {left:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_swapped_for_a_link_is_refused() {
        let dir = tmp("fsio-dirlink");
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        std::os::unix::fs::symlink(dir.join("out"), dir.join("proj/sub")).unwrap();
        assert!(write_atomic(&dir.join("proj/sub/f.txt"), b"x").unwrap_err().to_string().contains("symlink"));
        assert_eq!(std::fs::read_dir(dir.join("out")).unwrap().count(), 0, "nothing written through the link");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_write_leaves_the_original_alone() {
        let dir = tmp("fsio-fail");
        // The target is a directory, so the final rename cannot succeed.
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("d/keep.txt"), "keep").unwrap();
        assert!(write_atomic(&dir.join("d"), b"x").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("d/keep.txt")).unwrap(), "keep");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["d"], "the temporary file is removed after a failure");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_writers_of_one_file_take_turns() {
        let dir = tmp("fsio-lock");
        let p = dir.join("count.txt");
        std::fs::write(&p, "0").unwrap();
        // Each thread does a read-modify-write, as the edit tool does. Without the lock updates are lost.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let _guard = lock_files([&p]);
                        let n: u32 = std::fs::read_to_string(&p).unwrap().parse().unwrap();
                        write_atomic(&p, (n + 1).to_string().as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "200", "no update was lost");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn locks_on_different_files_and_in_any_order_do_not_block_each_other() {
        let dir = tmp("fsio-order");
        let (a, b) = (dir.join("a"), dir.join("b"));
        let one = lock_files([&a]);
        let two = lock_files([&b]); // a different file: no waiting
        drop((one, two));
        // Two calls taking the same pair in opposite order must not deadlock.
        let (a2, b2) = (a.clone(), b.clone());
        let t = std::thread::spawn(move || {
            for _ in 0..200 {
                let _g = lock_files([&b2, &a2]);
            }
        });
        for _ in 0..200 {
            let _g = lock_files([&a, &b]);
        }
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_symlink_and_its_target_share_a_lock() {
        let dir = tmp("fsio-share");
        std::fs::write(dir.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        assert_eq!(key(&dir.join("link.txt")), key(&dir.join("real.txt")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn formatter_changes_are_numbered_and_capped() {
        let d = changes(b"a\nb\nc\n", b"a\nB\nc\n").unwrap();
        assert_eq!(d, "-2: b\n+2: B");
        assert_eq!(changes(b"same\n", b"same\n"), None);
        assert_eq!(changes(b"\xff", b"x"), None, "not text");
        let before: String = (0..100).map(|i| format!("l{i}\n")).collect();
        let after: String = (0..100).map(|i| format!("L{i}\n")).collect();
        let d = changes(before.as_bytes(), after.as_bytes()).unwrap();
        assert_eq!(d.lines().count(), MAX_SHOWN + 1);
        assert!(d.ends_with("... and 170 more changed lines"), "{d}");
    }
}
