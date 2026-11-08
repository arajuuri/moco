//! Working-tree snapshots for undo and diffs (docs/03-agent-loop.md "Revert").
//!
//! A snapshot is a content-addressed manifest: every non-ignored file of a directory with its SHA-256 and mode,
//! file contents stored once as blobs. Capturing walks the tree (honoring `.gitignore`, skipping `.git`) and
//! re-hashes only files whose size or mtime changed since the previous snapshot. Restoring writes back changed
//! files and deletes files that did not exist; ignored files are never touched. Self-contained: no `git` needed.
use crate::compat::*;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapError {
    /// More files than `max_files`: probably not a project directory (a home directory, a build tree).
    TooManyFiles(usize),
    Missing(String),
    Io(String),
}

impl std::fmt::Display for SnapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapError::TooManyFiles(n) => write!(f, "more than {n} files; snapshots are skipped for this directory"),
            SnapError::Missing(m) => write!(f, "snapshot data missing: {m}"),
            SnapError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl From<std::io::Error> for SnapError {
    fn from(e: std::io::Error) -> Self {
        SnapError::Io(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub hash: String,
    pub size: u64,
    pub mode: u32,
    /// Modification time hint (ns since epoch) so unchanged files are not re-read.
    #[serde(default)]
    pub mtime_ns: u64,
}

/// Why a file is not part of a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    TooLarge,
    Unreadable,
    NoMetadata,
    NotUtf8,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SkipReason::TooLarge => "too large to snapshot",
            SkipReason::Unreadable => "unreadable",
            SkipReason::NoMetadata => "metadata unavailable",
            SkipReason::NotUtf8 => "path is not valid UTF-8",
        })
    }
}

/// A file capture could not record. `size` and `mtime_ns` (0 if unknown) tell whether it changed between snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ns: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: BTreeMap<String, Entry>,
    /// Files that exist but are not in `files`, so restore can neither revert nor delete them.
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub status: Status,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    /// Unified diff text (empty for binary files).
    pub patch: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    /// Files that changed (or were created or deleted) but are not covered by the snapshot, so were not restored.
    pub skipped: Vec<Skipped>,
    /// Snapshot of the state before the restore, so it can be undone.
    pub before: String,
}

pub struct Snapshots {
    root: PathBuf,
    pub max_files: usize,
    pub max_file_bytes: u64,
    /// Prune never deletes blobs (or temp files) modified more recently than this, so it cannot remove the blobs
    /// of a capture in another process that has not written its manifest yet.
    pub blob_grace: Duration,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Temp name unique per process and per call, so threads (a session and a background subagent share one store)
/// never write the same temp file.
fn tmp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".moco-tmp-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    path.with_file_name(name)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Write a content-addressed file (blob or manifest). If the write fails but the content is already there
/// (another writer got there first), that is success.
fn write_addressed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match write_atomic(path, bytes) {
        Err(e) if !path.exists() => Err(e),
        _ => Ok(()),
    }
}

/// Set a file's mtime to now so prune treats it as recently used. False if the file is gone.
fn touch(path: &Path) -> bool {
    std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(SystemTime::now())).is_ok()
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8192)].contains(&0) || std::str::from_utf8(b).is_err()
}

/// Relative path from a manifest is only ever used if it stays inside the directory.
fn safe_rel(p: &str) -> bool {
    !p.is_empty() && Path::new(p).components().all(|c| matches!(c, Component::Normal(_)))
}

/// Refuse to touch `rel` under `dir` if a parent directory component is a symlink (it could lead outside `dir`).
/// Missing components are fine: they will be created as real directories.
fn check_parents(dir: &Path, rel: &str) -> Result<(), SnapError> {
    let mut cur = dir.to_path_buf();
    let comps: Vec<_> = Path::new(rel).components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(SnapError::Io(format!("refusing to restore {rel}: {} is now a symlink", cur.strip_prefix(dir).unwrap_or(&cur).display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

impl Snapshots {
    pub fn new(root: PathBuf) -> Self {
        Snapshots { root, max_files: 50_000, max_file_bytes: 2 * 1024 * 1024, blob_grace: Duration::from_secs(3600) }
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(&hash[..2]).join(&hash[2..])
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{id}.json"))
    }

    pub fn manifest(&self, id: &str) -> Result<Manifest, SnapError> {
        let text = std::fs::read_to_string(self.manifest_path(id)).map_err(|_| SnapError::Missing(format!("snapshot {id}")))?;
        serde_json::from_str(&text).map_err(|e| SnapError::Io(format!("snapshot {id}: {e}")))
    }

    /// Like `manifest`, and marks it as recently used.
    fn manifest_in_use(&self, id: &str) -> Result<Manifest, SnapError> {
        let m = self.manifest(id)?;
        touch(&self.manifest_path(id));
        Ok(m)
    }

    fn blob(&self, hash: &str) -> Result<Vec<u8>, SnapError> {
        std::fs::read(self.blob_path(hash)).map_err(|_| SnapError::Missing(format!("file content {}", &hash[..12])))
    }

    /// Snapshot `dir` as it is now. `prev` (an earlier snapshot of the same directory) lets unchanged files skip hashing.
    pub fn capture(&self, dir: &Path, prev: Option<&str>) -> Result<String, SnapError> {
        // Using a snapshot counts as use: a resumed old session keeps its manifests from being pruned by age.
        let prev = prev.and_then(|p| self.manifest_in_use(p).ok()).unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut skipped: Vec<Skipped> = vec![];
        let walker = WalkBuilder::new(dir)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(dir) else { continue };
            let Some(rel) = rel.to_str().map(String::from) else {
                skipped.push(Skipped { path: rel.to_string_lossy().into_owned(), reason: SkipReason::NotUtf8, size: 0, mtime_ns: 0 });
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                skipped.push(Skipped { path: rel, reason: SkipReason::NoMetadata, size: 0, mtime_ns: 0 });
                continue;
            };
            if meta.len() > self.max_file_bytes {
                let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
                skipped.push(Skipped { path: rel, reason: SkipReason::TooLarge, size: meta.len(), mtime_ns });
                continue;
            }
            if files.len() >= self.max_files {
                return Err(SnapError::TooManyFiles(self.max_files));
            }
            let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            let mode = meta.permissions().mode() & 0o777;
            let cached = prev.files.get(&rel).filter(|e| e.size == meta.len() && e.mtime_ns == mtime_ns && mtime_ns != 0);
            // A cached entry's blob is not stat'ed: `prev` was just marked in use, so prune keeps the blobs it refers to.
            let hash = match cached {
                Some(e) => e.hash.clone(),
                None => {
                    let Ok(bytes) = std::fs::read(entry.path()) else {
                        skipped.push(Skipped { path: rel, reason: SkipReason::Unreadable, size: meta.len(), mtime_ns });
                        continue;
                    };
                    let h = sha(&bytes);
                    let bp = self.blob_path(&h);
                    // An existing blob is refreshed so a concurrent prune cannot take it before our manifest lands.
                    if !touch(&bp) {
                        write_addressed(&bp, &bytes)?;
                    }
                    h
                }
            };
            files.insert(rel, Entry { hash, size: meta.len(), mode, mtime_ns });
        }
        let mut hasher = Sha256::new();
        let mut line = String::new();
        for (p, e) in &files {
            line.clear();
            let _ = writeln!(line, "{p}\0{}\0{:o}", e.hash, e.mode);
            hasher.update(line.as_bytes());
        }
        skipped.sort_by(|a, b| a.path.cmp(&b.path));
        for s in &skipped {
            line.clear();
            let _ = writeln!(line, "skipped\0{}\0{:?}\0{}\0{}", s.path, s.reason, s.size, s.mtime_ns);
            hasher.update(line.as_bytes());
        }
        let id = hex(&hasher.finalize());
        // The id is content-addressed: an existing manifest is the same file, so only mark it as recently used.
        let mp = self.manifest_path(&id);
        if !touch(&mp) {
            let manifest = Manifest { files, skipped };
            write_addressed(&mp, serde_json::to_string(&manifest).map_err(|e| SnapError::Io(e.to_string()))?.as_bytes())?;
        }
        Ok(id)
    }

    /// Files that differ between two snapshots, with unified diffs.
    pub fn diff(&self, from: &str, to: &str) -> Result<Vec<FileDiff>, SnapError> {
        let (a, b) = (self.manifest_in_use(from)?, self.manifest_in_use(to)?);
        let mut paths: Vec<&String> = a.files.keys().chain(b.files.keys()).collect();
        paths.sort();
        paths.dedup();
        let mut out = vec![];
        for p in paths {
            let (ea, eb) = (a.files.get(p), b.files.get(p));
            if let (Some(x), Some(y)) = (ea, eb) {
                if x.hash == y.hash {
                    continue; // a mode-only change is not shown as a content diff
                }
            }
            let old = ea.map(|e| self.blob(&e.hash)).transpose()?.unwrap_or_default();
            let new = eb.map(|e| self.blob(&e.hash)).transpose()?.unwrap_or_default();
            let status = match (ea, eb) {
                (None, _) => Status::Added,
                (_, None) => Status::Deleted,
                _ => Status::Modified,
            };
            if is_binary(&old) || is_binary(&new) {
                out.push(FileDiff { path: p.clone(), status, additions: 0, deletions: 0, binary: true, patch: String::new() });
                continue;
            }
            let (old, new) = (String::from_utf8_lossy(&old).into_owned(), String::from_utf8_lossy(&new).into_owned());
            let diff = similar::TextDiff::from_lines(&old, &new);
            let (mut additions, mut deletions) = (0, 0);
            for c in diff.iter_all_changes() {
                match c.tag() {
                    similar::ChangeTag::Insert => additions += 1,
                    similar::ChangeTag::Delete => deletions += 1,
                    similar::ChangeTag::Equal => {}
                }
            }
            let (la, lb) = (if ea.is_some() { format!("a/{p}") } else { "/dev/null".into() }, if eb.is_some() { format!("b/{p}") } else { "/dev/null".into() });
            let patch = diff.unified_diff().context_radius(3).header(&la, &lb).to_string();
            out.push(FileDiff { path: p.clone(), status, additions, deletions, binary: false, patch });
        }
        Ok(out)
    }

    /// Make `dir` match snapshot `id`. The current state is captured first (`before`) so the restore can be undone.
    /// `hint` is a recent snapshot of the directory, used to avoid re-hashing unchanged files.
    pub fn restore(&self, dir: &Path, id: &str, hint: Option<&str>) -> Result<RestoreReport, SnapError> {
        let target = self.manifest_in_use(id)?;
        if let Some(bad) = target.files.keys().find(|p| !safe_rel(p)) {
            return Err(SnapError::Io(format!("refusing to restore unsafe path {bad:?}")));
        }
        let before = self.capture(dir, hint)?;
        let current = self.manifest(&before)?;
        let mut report = RestoreReport { before, ..Default::default() };
        // Files the snapshot cannot speak for: present now but not in the target, or in the target but gone/changed now.
        let same = |a: &Skipped, b: &Skipped| a.path == b.path && a.size == b.size && a.mtime_ns == b.mtime_ns;
        report.skipped.extend(current.skipped.iter().filter(|c| !target.skipped.iter().any(|t| same(c, t))).cloned());
        report.skipped.extend(target.skipped.iter().filter(|t| !current.skipped.iter().any(|c| same(c, t))).cloned());
        report.skipped.sort_by(|a, b| a.path.cmp(&b.path));
        // Check everything before touching anything, so a refusal leaves the tree as it was.
        let to_write: Vec<(&String, &Entry)> = target.files.iter().filter(|(p, e)| !current.files.get(*p).is_some_and(|c| c.hash == e.hash && c.mode == e.mode)).collect();
        let to_delete: Vec<&String> = current.files.keys().filter(|p| !target.files.contains_key(*p) && safe_rel(p)).collect();
        for p in to_write.iter().map(|(p, _)| *p).chain(to_delete.iter().copied()) {
            check_parents(dir, p)?;
        }
        for (p, _) in &to_write {
            let dest = dir.join(p);
            if let Ok(m) = std::fs::symlink_metadata(&dest) {
                if m.is_dir() {
                    return Err(SnapError::Io(format!("{p} is a directory now; cannot restore the file")));
                }
                if m.file_type().is_symlink() && dest.is_dir() {
                    return Err(SnapError::Io(format!("{p} is now a symlink to a directory; cannot restore the file")));
                }
            }
        }
        for (p, e) in to_write {
            let bytes = self.blob(&e.hash)?;
            let dest = dir.join(p);
            write_atomic(&dest, &bytes)?;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(e.mode))?;
            report.restored.push(p.clone());
        }
        for p in to_delete {
            let path = dir.join(p);
            if std::fs::remove_file(&path).is_ok() {
                report.deleted.push(p.clone());
                // Drop directories this emptied, but never the root.
                let mut parent = path.parent();
                while let Some(d) = parent.filter(|d| *d != dir && d.starts_with(dir)) {
                    if std::fs::remove_dir(d).is_err() {
                        break;
                    }
                    parent = d.parent();
                }
            }
        }
        Ok(report)
    }

    /// Delete snapshots older than `max_age` that are not in `keep`, then blobs no remaining snapshot refers to.
    /// Blobs modified within `blob_grace` are kept even if unreferenced (they may belong to a capture in progress).
    /// Returns (manifests removed, blobs removed).
    pub fn prune(&self, keep: &HashSet<String>, max_age: Duration) -> (usize, usize) {
        let mut removed_manifests = 0;
        let mut live: HashSet<String> = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(self.root.join("manifests")) {
            for e in rd.filter_map(|e| e.ok()) {
                let id = e.file_name().to_string_lossy().trim_end_matches(".json").to_string();
                let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age > max_age);
                if old && !keep.contains(&id) && std::fs::remove_file(e.path()).is_ok() {
                    removed_manifests += 1;
                    continue;
                }
                if let Ok(m) = self.manifest(&id) {
                    live.extend(m.files.into_values().map(|f| f.hash));
                }
            }
        }
        let mut removed_blobs = 0;
        if let Ok(shards) = std::fs::read_dir(self.root.join("blobs")) {
            for shard in shards.filter_map(|e| e.ok()) {
                let prefix = shard.file_name().to_string_lossy().to_string();
                if let Ok(blobs) = std::fs::read_dir(shard.path()) {
                    for b in blobs.filter_map(|e| e.ok()) {
                        let hash = format!("{prefix}{}", b.file_name().to_string_lossy());
                        let fresh = b.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_none_or(|age| age < self.blob_grace);
                        if !fresh && !live.contains(&hash) && std::fs::remove_file(b.path()).is_ok() {
                            removed_blobs += 1;
                        }
                    }
                }
                let _ = std::fs::remove_dir(shard.path());
            }
        }
        (removed_manifests, removed_blobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree {
        root: PathBuf,
        snaps: Snapshots,
    }
    impl Tree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(crate::id::new_id("moco-snap"));
            std::fs::create_dir_all(root.join("work")).unwrap();
            let snaps = Snapshots::new(root.join("store"));
            Tree { root, snaps }
        }
        fn dir(&self) -> PathBuf {
            self.root.join("work")
        }
        fn put(&self, rel: &str, content: &str) {
            let p = self.dir().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        fn read(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.dir().join(rel)).ok()
        }
        fn cap(&self, prev: Option<&str>) -> String {
            self.snaps.capture(&self.dir(), prev).unwrap()
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn same_content_same_id_and_changes_change_it() {
        let t = Tree::new();
        t.put("a.txt", "one");
        t.put("sub/b.txt", "two");
        let s1 = t.cap(None);
        assert_eq!(t.cap(Some(&s1)), s1, "unchanged tree, same id (with or without the hash cache)");
        assert_eq!(t.cap(None), s1);
        t.put("a.txt", "ONE");
        let s2 = t.cap(Some(&s1));
        assert_ne!(s1, s2);
        let m = t.snaps.manifest(&s2).unwrap();
        assert_eq!(m.files.keys().collect::<Vec<_>>(), ["a.txt", "sub/b.txt"]);
        assert_eq!(m.files["a.txt"].size, 3);
    }

    #[test]
    fn same_size_edit_with_a_new_mtime_is_noticed() {
        let t = Tree::new();
        t.put("a.txt", "aaa");
        let s1 = t.cap(None);
        std::thread::sleep(Duration::from_millis(20));
        t.put("a.txt", "bbb"); // same size
        assert_ne!(t.cap(Some(&s1)), s1, "mtime differs, so the file is re-hashed");
    }

    #[test]
    fn ignore_rules_hidden_files_symlinks_and_size_limit() {
        let t = Tree::new();
        t.put(".gitignore", "build/\n*.log\n");
        t.put("build/out.bin", "x");
        t.put("run.log", "x");
        t.put("src/main.rs", "fn main() {}");
        t.put(".github/ci.yml", "on: push");
        t.put(".git/config", "[core]");
        t.put("big.dat", &"x".repeat(3 * 1024 * 1024));
        std::os::unix::fs::symlink(t.dir().join("src/main.rs"), t.dir().join("link.rs")).unwrap();
        let m = t.snaps.manifest(&t.cap(None)).unwrap();
        assert_eq!(m.files.keys().map(String::as_str).collect::<Vec<_>>(), [".github/ci.yml", ".gitignore", "src/main.rs"]);
    }

    #[test]
    fn too_many_files_is_an_error_not_a_hang() {
        let t = Tree::new();
        for i in 0..5 {
            t.put(&format!("f{i}"), "x");
        }
        let mut s = Snapshots::new(t.root.join("store2"));
        s.max_files = 3;
        assert_eq!(s.capture(&t.dir(), None), Err(SnapError::TooManyFiles(3)));
    }

    #[test]
    fn diff_reports_added_modified_deleted_with_counts_and_patches() {
        let t = Tree::new();
        t.put("keep.txt", "same\n");
        t.put("mod.txt", "a\nb\nc\n");
        t.put("del.txt", "gone\n");
        t.put("bin.dat", "x");
        std::fs::write(t.dir().join("bin.dat"), [0u8, 1, 2]).unwrap();
        let s1 = t.cap(None);
        t.put("mod.txt", "a\nB\nc\nd\n");
        t.put("new.txt", "fresh\n");
        std::fs::remove_file(t.dir().join("del.txt")).unwrap();
        std::fs::write(t.dir().join("bin.dat"), [0u8, 9]).unwrap();
        let s2 = t.cap(Some(&s1));
        let d = t.snaps.diff(&s1, &s2).unwrap();
        let summary: Vec<_> = d.iter().map(|f| (f.path.as_str(), f.status, f.additions, f.deletions, f.binary)).collect();
        assert_eq!(
            summary,
            [("bin.dat", Status::Modified, 0, 0, true), ("del.txt", Status::Deleted, 0, 1, false), ("mod.txt", Status::Modified, 2, 1, false), ("new.txt", Status::Added, 1, 0, false)]
        );
        let m = d.iter().find(|f| f.path == "mod.txt").unwrap();
        assert!(m.patch.contains("--- a/mod.txt") && m.patch.contains("+++ b/mod.txt") && m.patch.contains("-b\n+B\n") && m.patch.contains("+d"), "{}", m.patch);
        assert!(d.iter().find(|f| f.path == "new.txt").unwrap().patch.contains("--- /dev/null"));
        assert_eq!(t.snaps.diff(&s1, &s1).unwrap(), vec![]);
    }

    #[test]
    fn restore_reverts_edits_recreates_deletes_and_removes_new_files() {
        let t = Tree::new();
        t.put("a.txt", "original");
        t.put("dir/keep.txt", "keep");
        t.put("dir/gone.txt", "will be deleted");
        t.put(".gitignore", "ignored.txt\n");
        t.put("ignored.txt", "never touched");
        let script = t.dir().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s1 = t.cap(None);

        t.put("a.txt", "edited");
        std::fs::remove_file(t.dir().join("dir/gone.txt")).unwrap();
        t.put("brand/new/file.txt", "new");
        t.put("ignored.txt", "changed but ignored");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        let s2 = t.cap(Some(&s1));

        let r = t.snaps.restore(&t.dir(), &s1, Some(&s2)).unwrap();
        assert_eq!(r.before, s2, "the state before the restore is returned (so it can be undone)");
        assert_eq!(r.restored, ["a.txt", "dir/gone.txt", "run.sh"]);
        assert_eq!(r.deleted, ["brand/new/file.txt"]);
        assert_eq!((t.read("a.txt").as_deref(), t.read("dir/gone.txt").as_deref()), (Some("original"), Some("will be deleted")));
        assert!(!t.dir().join("brand").exists(), "directories emptied by the restore are removed");
        assert_eq!(t.read("ignored.txt").as_deref(), Some("changed but ignored"), "ignored files are never touched");
        assert_eq!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755, "mode restored");
        assert_eq!(t.cap(None), s1, "the tree is back to the snapshot exactly");
        // And forward again.
        t.snaps.restore(&t.dir(), &r.before, None).unwrap();
        assert_eq!(t.cap(None), s2);
        assert_eq!(t.read("brand/new/file.txt").as_deref(), Some("new"));
    }

    #[test]
    fn restore_rejects_unsafe_manifests_and_missing_snapshots() {
        let t = Tree::new();
        t.put("a.txt", "x");
        let evil = Manifest { files: BTreeMap::from([("../outside.txt".to_string(), Entry { hash: sha(b"x"), size: 1, mode: 0o644, mtime_ns: 0 })]), skipped: vec![] };
        write_atomic(&t.snaps.manifest_path("evil"), serde_json::to_string(&evil).unwrap().as_bytes()).unwrap();
        assert!(t.snaps.restore(&t.dir(), "evil", None).unwrap_err().to_string().contains("unsafe path"));
        assert!(matches!(t.snaps.restore(&t.dir(), "nope", None), Err(SnapError::Missing(_))));
        assert!(!t.root.join("outside.txt").exists());
    }

    #[test]
    fn restore_reports_large_files_it_cannot_revert() {
        let mut t = Tree::new();
        t.snaps.max_file_bytes = 100;
        t.put("a.txt", "a");
        t.put("big.json", &"x".repeat(200));
        let s1 = t.cap(None);
        assert_eq!(t.snaps.manifest(&s1).unwrap().skipped.iter().map(|s| (s.path.as_str(), s.reason)).collect::<Vec<_>>(), [("big.json", SkipReason::TooLarge)]);
        std::thread::sleep(Duration::from_millis(20));
        t.put("big.json", &"y".repeat(300)); // modified
        t.put("new-big.json", &"z".repeat(300)); // created
        t.put("a.txt", "edited");
        let s2 = t.cap(Some(&s1));
        let r = t.snaps.restore(&t.dir(), &s1, Some(&s2)).unwrap();
        assert_eq!(r.restored, ["a.txt"]);
        let paths: Vec<_> = r.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["big.json", "big.json", "new-big.json"], "modified (old and new version) and created");
        assert!(r.skipped.iter().all(|s| s.reason == SkipReason::TooLarge));
        assert!(t.dir().join("new-big.json").exists(), "not deleted: restore cannot know it, but it is reported");
        // An untouched large file is not reported.
        let s3 = t.cap(None);
        assert!(t.snaps.restore(&t.dir(), &s3, None).unwrap().skipped.is_empty());
    }

    #[test]
    fn restore_refuses_symlinked_parent_directories_and_symlink_destinations() {
        let t = Tree::new();
        t.put("src/x.txt", "original");
        t.put("d/y.txt", "y");
        let s1 = t.cap(None);
        let outside = t.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(t.dir().join("src")).unwrap();
        std::os::unix::fs::symlink(&outside, t.dir().join("src")).unwrap();
        let err = t.snaps.restore(&t.dir(), &s1, None).unwrap_err().to_string();
        assert!(err.contains("src") && err.contains("symlink"), "{err}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing written outside the project");
        std::fs::remove_file(t.dir().join("src")).unwrap();
        // A destination that is now a symlink to a directory gets a clear error.
        std::fs::remove_file(t.dir().join("d/y.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, t.dir().join("d/y.txt")).unwrap();
        let err = t.snaps.restore(&t.dir(), &s1, None).unwrap_err().to_string();
        assert!(err.contains("symlink to a directory"), "{err}");
        assert!(t.read("src/x.txt").is_none(), "a refused restore changes nothing");
    }

    #[test]
    fn prune_drops_old_unreferenced_snapshots_and_orphan_blobs() {
        let t = Tree::new();
        t.put("a.txt", "v1");
        let s1 = t.cap(None);
        t.put("a.txt", "v2");
        let s2 = t.cap(Some(&s1));
        assert_eq!(t.snaps.prune(&HashSet::new(), Duration::from_secs(3600)), (0, 0), "nothing is old enough");
        assert_eq!(t.snaps.prune(&HashSet::from([s2.clone()]), Duration::ZERO), (1, 0), "the orphan blob is inside the grace period");
        let mut t = t;
        t.snaps.blob_grace = Duration::ZERO;
        let (m, b) = t.snaps.prune(&HashSet::from([s2.clone()]), Duration::ZERO);
        assert_eq!((m, b), (0, 1), "the blob goes once the grace period is over (s1 went above); s2 is kept");
        assert!(t.snaps.manifest(&s1).is_err() && t.snaps.manifest(&s2).is_ok());
        assert_eq!(t.snaps.restore(&t.dir(), &s2, None).unwrap().restored.len(), 0);
    }

    #[test]
    fn concurrent_captures_of_new_content_do_not_collide() {
        let t = Tree::new();
        for i in 0..40 {
            t.put(&format!("f{i}.txt"), &format!("content {i}"));
        }
        std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| t.snaps.capture(&t.dir(), None))).collect();
            let ids: Vec<_> = hs.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
            assert!(ids.windows(2).all(|w| w[0] == w[1]));
        });
    }

    #[test]
    fn unchanged_capture_does_not_rewrite_the_manifest() {
        let t = Tree::new();
        t.put("a.txt", "one");
        let s1 = t.cap(None);
        let mp = t.snaps.manifest_path(&s1);
        std::fs::write(&mp, serde_json::to_string(&t.snaps.manifest(&s1).unwrap()).unwrap() + " ").unwrap(); // marker
        assert_eq!(t.cap(Some(&s1)), s1);
        assert!(std::fs::read_to_string(&mp).unwrap().ends_with(' '), "the existing manifest was left alone");
    }

    #[test]
    fn prune_spares_fresh_blobs_and_snapshots_in_use() {
        let mut t = Tree::new();
        t.snaps.blob_grace = Duration::from_millis(300);
        t.put("a.txt", "v1");
        let s1 = t.cap(None);
        std::thread::sleep(Duration::from_millis(400));
        // A capture in progress re-uses the old blob (its manifest is not written yet): the blob is refreshed.
        let bp = t.snaps.blob_path(&sha(b"v1"));
        assert!(touch(&bp));
        t.snaps.prune(&HashSet::new(), Duration::from_millis(200));
        assert!(t.snaps.manifest(&s1).is_err(), "old unreferenced manifest goes");
        assert!(bp.exists(), "a blob touched within the grace period survives");
        // Using a snapshot (restore, diff, capture with prev) marks it as in use.
        t.put("a.txt", "v2");
        let s2 = t.cap(None);
        std::thread::sleep(Duration::from_millis(400));
        t.snaps.restore(&t.dir(), &s2, None).unwrap();
        t.snaps.prune(&HashSet::new(), Duration::from_millis(200));
        assert!(t.snaps.manifest(&s2).is_ok(), "a snapshot in use is not pruned by age");
    }
}
