//! Bound tool output before the model sees it: 2000 lines / 50 KiB, full text spilled to a file.
use crate::compat::*;
use std::path::{Path, PathBuf};

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Truncated {
    pub text: String,
    pub output_path: Option<PathBuf>,
}

/// Keep whole lines up to the limits. If anything is dropped, write the full text to
/// `spill_dir/<id>.txt` and append a notice. A single over-long first line is cut at a char boundary.
pub fn truncate(text: &str, spill_dir: &Path) -> std::io::Result<Truncated> {
    let total_lines = text.split('\n').count();
    if total_lines <= MAX_LINES && text.len() <= MAX_BYTES {
        return Ok(Truncated { text: text.to_string(), output_path: None });
    }
    let (mut kept, mut bytes, mut n) = (String::new(), 0usize, 0usize);
    for line in text.split('\n') {
        let add = line.len() + usize::from(n > 0);
        if n >= MAX_LINES || bytes + add > MAX_BYTES {
            break;
        }
        if n > 0 {
            kept.push('\n');
        }
        kept.push_str(line);
        bytes += add;
        n += 1;
    }
    if n == 0 {
        let mut cut = MAX_BYTES.min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        kept = text[..cut].to_string();
        n = 1;
    }
    crate::auth::create_private_dir(spill_dir)?;
    let path = spill_dir.join(format!("{}.txt", crate::id::new_id("out")));
    std::io::Write::write_all(&mut crate::auth::create_private(&path, false)?, text.as_bytes())?;
    let notice = format!("\n\n[showing lines 1-{n} of {total_lines}; full output saved to {}]", path.display());
    Ok(Truncated { text: kept + &notice, output_path: Some(path) })
}

/// Spill files older than this are deleted at startup (see `prune_spill_dir`).
pub const SPILL_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Delete `*.txt` files in `dir` last modified more than `max_age` ago, except those `keep` says to leave alone
/// (logs of running jobs). Returns how many were removed. Errors on single files are ignored.
pub fn prune_spill_dir(dir: &Path, max_age: std::time::Duration, keep: &dyn Fn(&Path) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "txt") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age > max_age);
        if old && !keep(&path) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_old_spill_files_but_not_new_or_kept_ones() {
        let d = dir();
        std::fs::create_dir_all(&d).unwrap();
        let age = |name: &str, days: u64| {
            let p = d.join(name);
            std::fs::write(&p, "x").unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 3600)).unwrap();
            p
        };
        let (old, fresh, kept, other) = (age("out_old.txt", 10), age("out_new.txt", 1), age("shl_running.txt", 10), age("notes.dat", 10));
        let n = prune_spill_dir(&d, SPILL_MAX_AGE, &|p| p == kept);
        assert_eq!(n, 1);
        assert!(!old.exists() && fresh.exists() && kept.exists() && other.exists());
        let _ = std::fs::remove_dir_all(d);
    }

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("moco-trunc-{}", crate::id::new_id("t")))
    }

    #[test]
    fn small_passes_through() {
        let t = truncate("a\nb", &dir()).unwrap();
        assert_eq!((t.text.as_str(), t.output_path), ("a\nb", None));
    }

    #[test]
    fn line_limit() {
        let d = dir();
        let text = (1..=2500).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let t = truncate(&text, &d).unwrap();
        assert!(t.text.contains("[showing lines 1-2000 of 2500;"));
        assert!(t.text.starts_with("1\n2\n"));
        assert_eq!(std::fs::read_to_string(t.output_path.unwrap()).unwrap(), text);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn byte_limit_keeps_whole_lines() {
        let d = dir();
        let text = vec!["x".repeat(1000); 100].join("\n");
        let t = truncate(&text, &d).unwrap();
        assert!(t.text.contains("[showing lines 1-51 of 100;"), "{}", &t.text[t.text.len() - 80..]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn one_giant_line_is_cut_on_char_boundary() {
        let d = dir();
        let t = truncate(&"é".repeat(60_000), &d).unwrap();
        assert!(t.text.contains("[showing lines 1-1 of 1;"));
        assert!(t.output_path.is_some());
        let _ = std::fs::remove_dir_all(d);
    }
}
