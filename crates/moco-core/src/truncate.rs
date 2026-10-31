//! Bound tool output before the model sees it: 2000 lines / 50 KiB, full text spilled to a file.
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
    std::fs::create_dir_all(spill_dir)?;
    let path = spill_dir.join(format!("{}.txt", crate::id::new_id("out")));
    std::fs::write(&path, text)?;
    let notice = format!("\n\n[showing lines 1-{n} of {total_lines}; full output saved to {}]", path.display());
    Ok(Truncated { text: kept + &notice, output_path: Some(path) })
}

#[cfg(test)]
mod tests {
    use super::*;

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
