//! Content-addressed storage for attachments (images, PDFs) so messages hold a small reference instead of megabytes
//! of base64. Files live in `<app dir>/data/blobs/<first two hex chars>/<rest>`; identical content is stored once and
//! forks share it.
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn path(app_dir: &Path, hash: &str) -> Option<PathBuf> {
    (hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())).then(|| app_dir.join("data/blobs").join(&hash[..2]).join(&hash[2..]))
}

/// Store `bytes`, returning their SHA-256 (hex).
pub fn put(app_dir: &Path, bytes: &[u8]) -> std::io::Result<String> {
    let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    let p = path(app_dir, &hash).expect("a sha256 hex digest is a valid blob name");
    if !p.exists() {
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &p)?;
    }
    Ok(hash)
}

/// The bytes for a hash; `None` if missing, or if `hash` is not a valid digest (so a stored value can never name another file).
pub fn get(app_dir: &Path, hash: &str) -> Option<Vec<u8>> {
    std::fs::read(path(app_dir, hash)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_dedup_and_safe_names() {
        let dir = std::env::temp_dir().join(crate::id::new_id("blobs"));
        let h = put(&dir, b"hello").unwrap();
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(put(&dir, b"hello").unwrap(), h, "same content, same blob");
        assert_eq!(get(&dir, &h).unwrap(), b"hello");
        assert_eq!(std::fs::read_dir(dir.join("data/blobs")).unwrap().count(), 1);
        for bad in ["../../etc/passwd", "", "abc", &"g".repeat(64), &format!("{}/x", &h[..62])] {
            assert_eq!(get(&dir, bad), None, "{bad}");
        }
        assert_eq!(get(&dir, &"0".repeat(64)), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
