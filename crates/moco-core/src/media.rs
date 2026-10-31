//! Recognizing images and PDFs by their bytes (not their file name) and enforcing the limits that keep a bad
//! attachment from poisoning a conversation: a rejected image would otherwise be resent, and rejected again, on every step.

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
/// Anthropic rejects images larger than this on either side.
pub const MAX_IMAGE_DIM: u32 = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Pdf => "PDF",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sniffed {
    pub kind: Kind,
    pub media_type: &'static str,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as u32)
}
fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16)
}

fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = b[i + 1];
        if m == 0xFF || m == 0x00 || (0xD0..=0xD8).contains(&m) || m == 0x01 {
            i += if m == 0xFF { 1 } else { 2 };
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
            let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

fn webp_dims(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8 " => Some((le16(b, 26)? & 0x3FFF, le16(b, 28)? & 0x3FFF)),
        b"VP8L" => {
            let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
            Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => Some((le24(b, 24)? + 1, le24(b, 27)? + 1)),
        _ => None,
    }
}

/// What `bytes` are, judged by their magic numbers: PNG, JPEG, GIF, WebP or PDF.
pub fn sniff(b: &[u8]) -> Option<Sniffed> {
    let img = |media_type, dims: Option<(u32, u32)>| Some(Sniffed { kind: Kind::Image, media_type, width: dims.map(|d| d.0), height: dims.map(|d| d.1) });
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return img("image/png", be32(b, 16).zip(be32(b, 20)));
    }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return img("image/jpeg", jpeg_dims(b));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return img("image/gif", le16(b, 6).zip(le16(b, 8)));
    }
    if b.len() > 16 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return img("image/webp", webp_dims(b));
    }
    if b.starts_with(b"%PDF-") {
        return Some(Sniffed { kind: Kind::Pdf, media_type: "application/pdf", width: None, height: None });
    }
    None
}

fn human(n: usize) -> String {
    if n >= 1024 * 1024 { format!("{:.1} MiB", n as f64 / 1024.0 / 1024.0) } else { format!("{} KiB", n.div_ceil(1024)) }
}

/// `Err(message)` when the attachment must not be sent. Messages say how to fix it.
pub fn check(s: &Sniffed, size: usize) -> Result<(), String> {
    match s.kind {
        Kind::Image => {
            if size > MAX_IMAGE_BYTES {
                return Err(format!("the image is {} (limit {}); shrink it first, e.g. `sips -Z 2000 file.png --out small.png` on macOS", human(size), human(MAX_IMAGE_BYTES)));
            }
            if let (Some(w), Some(h)) = (s.width, s.height) {
                if w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
                    return Err(format!("the image is {w}x{h} pixels (limit {MAX_IMAGE_DIM} per side); shrink it first, e.g. `sips -Z 2000 file.png --out small.png` on macOS"));
                }
                if w == 0 || h == 0 {
                    return Err("the image has no pixels".into());
                }
            }
        }
        Kind::Pdf => {
            if size > MAX_PDF_BYTES {
                return Err(format!("the PDF is {} (limit {}); split it or extract the pages you need", human(size), human(MAX_PDF_BYTES)));
            }
        }
    }
    Ok(())
}

/// "PNG image, 800x600, 120 KiB"-style description for notes and tool output.
pub fn describe(s: &Sniffed, size: usize) -> String {
    match (s.width, s.height) {
        (Some(w), Some(h)) => format!("{}, {w}x{h}, {}", s.media_type, human(size)),
        _ => format!("{}, {}", s.media_type, human(size)),
    }
}

/// Rough token cost of an attachment of `size` raw bytes (for context estimates before the provider reports usage).
pub fn tokens(media_type: &str, size: u64) -> u64 {
    if media_type == "application/pdf" { (size / 12).clamp(2_000, 100_000) } else { 1_500 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    #[test]
    fn png_gif_jpeg_webp_pdf_are_recognized_with_dimensions() {
        let p = sniff(&png(640, 480)).unwrap();
        assert_eq!((p.kind, p.media_type, p.width, p.height), (Kind::Image, "image/png", Some(640), Some(480)));
        let gif = [b"GIF89a".as_slice(), &[0x20, 0x01, 0x10, 0x00]].concat(); // 288 x 16
        assert_eq!((sniff(&gif).unwrap().width, sniff(&gif).unwrap().height), (Some(288), Some(16)));
        // JPEG: SOI, an APP0 segment to skip, then SOF0 with height 300, width 400.
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0, 0xFF, 0xC0, 0x00, 0x0B, 8, 0x01, 0x2C, 0x01, 0x90, 3, 0, 0, 0];
        let j = sniff(&jpeg).unwrap();
        assert_eq!((j.media_type, j.width, j.height), ("image/jpeg", Some(400), Some(300)));
        // WebP lossless: 100 x 50 packed as (w-1) | (h-1) << 14.
        let mut webp = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0".to_vec();
        webp.push(0x2F);
        webp.extend_from_slice(&((99u32) | (49u32 << 14)).to_le_bytes());
        let w = sniff(&webp).unwrap();
        assert_eq!((w.media_type, w.width, w.height), ("image/webp", Some(100), Some(50)));
        let pdf = sniff(b"%PDF-1.7\n...").unwrap();
        assert_eq!((pdf.kind, pdf.media_type), (Kind::Pdf, "application/pdf"));
    }

    #[test]
    fn extension_is_irrelevant_and_junk_is_not_media() {
        for junk in [&b"hello world"[..], b"", b"\x89PNG", b"RIFFxxxxWAVEfmt ", b"GIF", b"<html>", b"\xFF\xD8"] {
            assert_eq!(sniff(junk), None, "{junk:?}");
        }
        // A truncated PNG is still a PNG, just without dimensions; the size limit still applies.
        let s = sniff(b"\x89PNG\r\n\x1a\nxx").unwrap();
        assert_eq!((s.width, s.height), (None, None));
        assert!(check(&s, 100).is_ok());
    }

    #[test]
    fn limits_explain_how_to_fix_the_problem() {
        let big = sniff(&png(9000, 100)).unwrap();
        assert!(check(&big, 1000).unwrap_err().contains("9000x100") && check(&big, 1000).unwrap_err().contains("sips"));
        let heavy = sniff(&png(100, 100)).unwrap();
        assert!(check(&heavy, MAX_IMAGE_BYTES + 1).unwrap_err().contains("limit 5.0 MiB"));
        assert!(check(&heavy, MAX_IMAGE_BYTES).is_ok());
        assert!(check(&sniff(&png(0, 5)).unwrap(), 10).unwrap_err().contains("no pixels"));
        let pdf = sniff(b"%PDF-1.4").unwrap();
        assert!(check(&pdf, MAX_PDF_BYTES + 1).unwrap_err().contains("PDF is 10.0 MiB"));
        assert!(check(&pdf, 1024).is_ok());
        assert_eq!(describe(&heavy, 2048), "image/png, 100x100, 2 KiB");
        assert_eq!(describe(&pdf, 3 * 1024 * 1024), "application/pdf, 3.0 MiB");
        assert_eq!((tokens("image/png", 10), tokens("application/pdf", 12_000_000)), (1500, 100_000));
    }
}
