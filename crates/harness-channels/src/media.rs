//! 通道共用的媒体落盘与类型嗅探。
//!
//! 通道**只负责把文件存下来**：要不要识别、怎么处理，由模型决定（`DECISIONS.md` D17）。
//! 按内容摘要命名 → 同一张图发两次只存一份。

use std::path::Path;

use harness_kernel::Attachment;

/// 存到 `dir/<digest><ext>`；已存在则不重写。返回可直接挂给模型的 `Attachment`。
pub fn save_media(
    dir: &Path,
    bytes: &[u8],
    name: &str,
    mime: &str,
) -> std::io::Result<Attachment> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let digest = format!("{h:016x}");
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| e.len() <= 5 && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let path = dir.join(format!("{digest}{ext}"));
    if !path.exists() {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(bytes)?;
    }
    Ok(Attachment {
        path: path.to_string_lossy().into_owned(),
        name: name.to_owned(),
        mime: mime.to_owned(),
        digest,
        bytes: bytes.len() as u64,
    })
}

/// 按文件头猜 mime。微信**不给** mime，而视觉注入只认 jpeg/png/webp/gif——
/// 猜错就等于"模型看不见这张图"，所以宁可多认几种。
pub fn sniff_mime(bytes: &[u8], name: &str) -> String {
    let head = bytes.get(..16).unwrap_or(bytes);
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        return "image/png".into();
    }
    if head.starts_with(b"GIF8") {
        return "image/gif".into();
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // HEIC/HEIF：视觉不支持，明确标出来，模型会走"只能看路径"的路径
        let brand = &head[8..12];
        if brand == b"heic" || brand == b"heix" || brand == b"mif1" || brand == b"msf1" {
            return "image/heic".into();
        }
    }
    if head.starts_with(b"%PDF") {
        return "application/pdf".into();
    }
    if head.starts_with(b"PK\x03\x04") {
        // zip 系（docx/xlsx/pptx 都是 zip）：按扩展名细分
        return match ext(name).as_str() {
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            _ => "application/zip",
        }
        .into();
    }
    if head.starts_with(b"#!") || head.starts_with(b"\x7fELF") {
        return "application/octet-stream".into();
    }
    // 兜底：看扩展名
    let e = ext(name);
    match e.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .into()
}

fn ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_headers() {
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0], "x"), "image/jpeg");
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 1], "x"), "image/png");
        assert_eq!(sniff_mime(b"%PDF-1.7", "x"), "application/pdf");
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 ", "x"), "image/webp");
    }

    #[test]
    fn falls_back_to_extension() {
        assert_eq!(sniff_mime(b"", "a.pdf"), "application/pdf");
        assert_eq!(sniff_mime(b"", "a.unknown"), "application/octet-stream");
    }

    #[test]
    fn save_media_dedupes_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let a = save_media(dir.path(), b"same", "a.txt", "text/plain").unwrap();
        let b = save_media(dir.path(), b"same", "b.txt", "text/plain").unwrap();
        assert_eq!(a.path, b.path, "同样内容只存一份");
        assert!(std::path::Path::new(&a.path).exists());
    }
}
