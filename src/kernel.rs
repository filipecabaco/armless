//! Turning what distributions ship as an arm64 kernel into a raw `Image`:
//! gzip (`Image.gz`, Alpine's and Debian's `vmlinuz`), zstd, or EFI zboot
//! (`MZ` + "zimg", with a gzip or zstd payload).

const IMAGE_MAGIC_OFFSET: usize = 0x38;
const IMAGE_MAGIC: u32 = 0x644d_5241; // "ARM\x64"

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("not gzip data".into());
    }
    let flags = data[3];
    let mut i = 10;
    if flags & 4 != 0 {
        let n = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        i += 2 + n;
    }
    for bit in [8, 16] {
        if flags & bit != 0 {
            while i < data.len() && data[i] != 0 {
                i += 1;
            }
            i += 1;
        }
    }
    if flags & 2 != 0 {
        i += 2;
    }
    let body = data.get(i..).ok_or("truncated gzip header")?;
    miniz_oxide::inflate::decompress_to_vec(body).map_err(|e| format!("gzip: {e:?}"))
}

fn decompress(kind: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    match kind {
        "gzip" => gunzip(data),
        "zstd" | "zstd22" => {
            crate::zstd::decompress(data).ok_or_else(|| "corrupt zstd kernel".into())
        },
        other => Err(format!("unsupported kernel compression: {other}")),
    }
}

/// A raw arm64 `Image` from a possibly compressed kernel.
pub fn prepare(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = if bytes.starts_with(&[0x1f, 0x8b]) {
        gunzip(bytes)?
    } else if crate::zstd::is_zstd(bytes) {
        decompress("zstd", bytes)?
    } else if bytes.len() > 56 && bytes.starts_with(b"MZ") && &bytes[4..8] == b"zimg" {
        let offset = u32_at(bytes, 8) as usize;
        let size = u32_at(bytes, 12) as usize;
        let kind = String::from_utf8_lossy(&bytes[24..56]);
        let kind = kind.trim_end_matches('\0');
        let payload = bytes
            .get(offset..offset + size)
            .ok_or("truncated zboot image")?;
        decompress(kind, payload)?
    } else {
        bytes.to_vec()
    };
    if image.len() < 64 || u32_at(&image, IMAGE_MAGIC_OFFSET) != IMAGE_MAGIC {
        return Err("not an arm64 Linux Image (after decompression)".into());
    }
    Ok(image)
}
