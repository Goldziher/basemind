//! Pre-extraction estimate of one document's peak resident memory.
//!
//! xberg's working set is not proportional to the file's size on disk: a 7 MB-of-XML `.docx`
//! measured ~2.5 GB over baseline, a 10-megapixel screenshot ~0.25 GB, a few-KB SVG almost nothing.
//! The footprint gate reacts to memory that is already allocated, and a single extraction can
//! allocate gigabytes faster than any sampler sees, so the scanner needs a number *before* it starts
//! to decide whether a document fits under `[resources] max_footprint_mb` at all. The estimate is
//! deliberately pessimistic (a wrong skip costs one document, a wrong admit costs the process) and
//! cheap: it reads a header, never decodes.
//!
//! The multipliers are calibrated against measured peaks, not derived; see the constants.

/// Working-set bytes per pixel for a raster image: the decoded bitmap, the binarised and scaled
/// copies OCR makes of it, and layout detection's tensors. Measured ~25 B/px on a 10 MP PNG; rounded
/// up to leave room for the transient the one-second sampler misses.
const BYTES_PER_PIXEL: u64 = 64;

/// Working-set bytes per *uncompressed* byte of an OOXML / OpenDocument package. Measured ~230 on a
/// 10.9 MB-uncompressed `.docx` (xberg builds a full document tree, then tables, then chunks).
const OFFICE_FACTOR: u64 = 256;

/// Working-set bytes per uncompressed byte of a plain archive extracted entry by entry.
const ARCHIVE_FACTOR: u64 = 8;

/// Working-set bytes per byte of file for everything else (PDF, HTML, text, email, ...).
const DEFAULT_FACTOR: u64 = 32;

/// Floor: even an empty document pays for runtime buffers.
const MIN_ESTIMATE: u64 = 32 * 1024 * 1024;

/// Estimated peak resident bytes extracting `bytes` (the whole file) of MIME type `mime`.
pub fn estimate_peak_bytes(bytes: &[u8], mime: &str) -> u64 {
    let size = bytes.len() as u64;
    let estimate = if let Some(pixels) = image_pixels(bytes) {
        pixels.saturating_mul(BYTES_PER_PIXEL)
    } else if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        let uncompressed = zip_uncompressed_bytes(bytes).unwrap_or_else(|| size.saturating_mul(16));
        let factor = if is_office_package(mime) {
            OFFICE_FACTOR
        } else {
            ARCHIVE_FACTOR
        };
        uncompressed.saturating_mul(factor)
    } else {
        size.saturating_mul(DEFAULT_FACTOR)
    };
    estimate.max(MIN_ESTIMATE)
}

/// OOXML (`.docx/.xlsx/.pptx`), OpenDocument and EPUB packages: zip containers xberg parses into a
/// document tree rather than unpacking entry by entry.
fn is_office_package(mime: &str) -> bool {
    mime.contains("officedocument")
        || mime.contains("opendocument")
        || mime.contains("msword")
        || mime.contains("ms-excel")
        || mime.contains("ms-powerpoint")
        || mime.contains("epub")
}

/// Width x height read from an image header, or `None` for a non-image or an unparseable header.
fn image_pixels(bytes: &[u8]) -> Option<u64> {
    let (width, height) = image_dimensions(bytes)?;
    Some(u64::from(width).saturating_mul(u64::from(height)))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?)))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 3)?;
    Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Pixel dimensions from the header of a PNG, GIF, JPEG, WebP or BMP.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be_u32(bytes, 16)?, be_u32(bytes, 20)?));
    }
    if bytes.starts_with(b"GIF8") {
        return Some((le_u16(bytes, 6)?, le_u16(bytes, 8)?));
    }
    if bytes.starts_with(b"BM") {
        // Height is signed (negative = top-down).
        let height = le_u32(bytes, 22)? as i32;
        return Some((le_u32(bytes, 18)?, height.unsigned_abs()));
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return match bytes.get(12..16)? {
            b"VP8 " => Some((le_u16(bytes, 26)? & 0x3fff, le_u16(bytes, 28)? & 0x3fff)),
            b"VP8L" => {
                let bits = le_u32(bytes, 21)?;
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => Some((le_u24(bytes, 24)? + 1, le_u24(bytes, 27)? + 1)),
            _ => None,
        };
    }
    if bytes.starts_with(b"\xff\xd8") {
        return jpeg_dimensions(bytes);
    }
    None
}

/// Walk JPEG markers to the first start-of-frame segment.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        match marker {
            0xff => at += 1,
            // Standalone markers carry no length.
            0x01 | 0xd0..=0xd9 => at += 2,
            // SOF0..SOF15 except DHT (c4), JPG (c8) and DAC (cc).
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                let height = u32::from(u16::from_be_bytes(bytes.get(at + 5..at + 7)?.try_into().ok()?));
                let width = u32::from(u16::from_be_bytes(bytes.get(at + 7..at + 9)?.try_into().ok()?));
                return Some((width, height));
            }
            _ => {
                let len = usize::from(u16::from_be_bytes(bytes.get(at + 2..at + 4)?.try_into().ok()?));
                at += 2 + len.max(2);
            }
        }
    }
    None
}

/// Sum of the uncompressed sizes recorded in a zip's central directory, without inflating anything.
/// A zip64 size (`0xFFFFFFFF`) counts as 4 GiB. `None` when the directory cannot be located.
fn zip_uncompressed_bytes(bytes: &[u8]) -> Option<u64> {
    const EOCD_SIG: &[u8] = b"PK\x05\x06";
    const CD_SIG: &[u8] = b"PK\x01\x02";
    const EOCD_MIN: usize = 22;
    let tail_start = bytes.len().saturating_sub(EOCD_MIN + usize::from(u16::MAX));
    let eocd = (tail_start..=bytes.len().checked_sub(EOCD_MIN)?)
        .rev()
        .find(|&i| bytes.get(i..i + 4) == Some(EOCD_SIG))?;
    let entries = le_u16(bytes, eocd + 10)? as usize;
    let mut at = le_u32(bytes, eocd + 16)? as usize;
    let mut total: u64 = 0;
    for _ in 0..entries {
        if bytes.get(at..at + 4)? != CD_SIG {
            return None;
        }
        let size = le_u32(bytes, at + 24)?;
        total = total.saturating_add(if size == u32::MAX { 1 << 32 } else { u64::from(size) });
        let name = le_u16(bytes, at + 28)? as usize;
        let extra = le_u16(bytes, at + 30)? as usize;
        let comment = le_u16(bytes, at + 32)? as usize;
        at = at.checked_add(46 + name + extra + comment)?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    /// A zip whose central directory lists `sizes` uncompressed entries. Only the directory is
    /// read, so no local headers or data are needed.
    fn zip_with_entries(sizes: &[u32]) -> Vec<u8> {
        let mut out = b"PK\x03\x04".to_vec();
        let cd_offset = out.len() as u32;
        for (i, size) in sizes.iter().enumerate() {
            let name = format!("part{i}.xml");
            let mut entry = vec![0u8; 46];
            entry[..4].copy_from_slice(b"PK\x01\x02");
            entry[24..28].copy_from_slice(&size.to_le_bytes());
            entry[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&entry);
            out.extend_from_slice(name.as_bytes());
        }
        let cd_size = out.len() as u32 - cd_offset;
        let mut eocd = vec![0u8; 22];
        eocd[..4].copy_from_slice(b"PK\x05\x06");
        eocd[10..12].copy_from_slice(&(sizes.len() as u16).to_le_bytes());
        eocd[12..16].copy_from_slice(&cd_size.to_le_bytes());
        eocd[16..20].copy_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&eocd);
        out
    }

    #[test]
    fn png_cost_scales_with_pixels_not_file_size() {
        let small = estimate_peak_bytes(&png(100, 100), "image/png");
        let huge = estimate_peak_bytes(&png(20_000, 20_000), "image/png");
        assert_eq!(small, MIN_ESTIMATE, "a tiny image is priced at the floor");
        assert_eq!(
            huge,
            20_000 * 20_000 * BYTES_PER_PIXEL,
            "the 44-byte header alone prices the bitmap"
        );
    }

    #[test]
    fn header_dimensions_are_read_for_every_supported_format() {
        assert_eq!(image_dimensions(&png(640, 480)), Some((640, 480)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x01, 0xe0, 0x01]);
        assert_eq!(image_dimensions(&gif), Some((320, 480)));

        let mut bmp = vec![0u8; 26];
        bmp[..2].copy_from_slice(b"BM");
        bmp[18..22].copy_from_slice(&300u32.to_le_bytes());
        bmp[22..26].copy_from_slice(&(-200i32).to_le_bytes());
        assert_eq!(image_dimensions(&bmp), Some((300, 200)));

        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x03, 0x00, 0x04, 0x00,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((1024, 768)));

        let mut webp_lossless = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
        let bits: u32 = 799 | (599 << 14);
        webp_lossless.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(image_dimensions(&webp_lossless), Some((800, 600)));
    }

    #[test]
    fn office_package_cost_follows_uncompressed_size() {
        let docx = zip_with_entries(&[7_000_000, 1_000_000]);
        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert_eq!(estimate_peak_bytes(&docx, mime), 8_000_000 * OFFICE_FACTOR);
    }

    #[test]
    fn a_highly_compressed_office_bomb_is_priced_by_what_it_inflates_to() {
        // A few hundred bytes of file, 1 GiB claimed uncompressed.
        let bomb = zip_with_entries(&[1 << 30]);
        assert!(bomb.len() < 256);
        let mime = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        assert!(estimate_peak_bytes(&bomb, mime) >= (1u64 << 30) * OFFICE_FACTOR);
    }

    #[test]
    fn plain_archives_use_the_cheaper_per_entry_factor() {
        let zip = zip_with_entries(&[10_000_000]);
        assert_eq!(
            estimate_peak_bytes(&zip, "application/zip"),
            10_000_000 * ARCHIVE_FACTOR
        );
    }

    #[test]
    fn other_documents_use_the_size_factor_and_garbage_does_not_panic() {
        let html = vec![b'a'; 4 * 1024 * 1024];
        assert_eq!(
            estimate_peak_bytes(&html, "text/html"),
            4 * 1024 * 1024 * DEFAULT_FACTOR
        );
        // Truncated / hostile headers degrade to the size-based estimate instead of panicking.
        for truncated in [&b"\x89PNG\r\n\x1a\n"[..], b"PK\x03\x04", b"\xff\xd8\xff", b"RIFF"] {
            assert!(estimate_peak_bytes(truncated, "application/octet-stream") >= MIN_ESTIMATE);
        }
    }
}
