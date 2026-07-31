//! Camera-RAW recognition + the bounded embedded-preview read.
//!
//! A RAW file puts a display JPEG near the front and then tens or hundreds of MiB of
//! sensor data, so the cascade reads only the head. Deciding that a stream really IS a
//! RAW (and not an ordinary TIFF, which must NOT take this shortcut) is the bulk of
//! this module: an Explorer stream often has no filename, so there is a structural
//! IFD parse behind the extension check.

use super::*;

pub(super) const RAW_PREFIX_BYTES: usize = 16 * 1024 * 1024;

pub(super) const RAW_SNIFF_BYTES: usize = 1024 * 1024;

pub(super) enum RawFastSource {
    Preview(Vec<u8>),
    Prefix(Vec<u8>, u64),
}

pub(super) fn raw_preview_size_allowed(size: u64, max_file_bytes: u64) -> bool {
    size > RAW_PREFIX_BYTES as u64 && size <= decode::effective_input_cap(max_file_bytes)
}

/// Read only a bounded RAW head and return its best complete embedded JPEG.
/// A RAW with no early preview retains the prefix so the existing bounded
/// whole-file fallback can append only the unread tail.
pub(super) unsafe fn raw_preview_fast(
    stream: &IStream,
    max_file_bytes: u64,
) -> Option<RawFastSource> {
    let raw_extension = stream_extension(stream).is_some_and(|ext| is_raw_extension(&ext));
    let size = stream_size(stream)?;
    if !raw_preview_size_allowed(size, max_file_bytes) {
        return None; // no I/O or allocation saving versus the normal bounded read
    }

    // Explorer commonly hands IInitializeWithStream an unnamed file stream. Prefer
    // the extension when its STATSTG exposes one, but retain a conservative content
    // fallback for those normal unnamed streams: RAW-specific signatures or
    // structurally parsed CFA/DNG IFD markers. A plain TIFF does not qualify.
    let sniff = stream_prefix(stream, RAW_SNIFF_BYTES)?;
    if !looks_like_raw_container(&sniff, raw_extension) {
        return None;
    }

    let prefix = stream_prefix(stream, RAW_PREFIX_BYTES)?;
    match decode::largest_embedded_jpeg(&prefix, decode::MIN_RAW_PREVIEW) {
        Some(jpeg) => Some(RawFastSource::Preview(jpeg.to_vec())),
        None => Some(RawFastSource::Prefix(prefix, size)),
    }
}

pub(super) fn is_raw_extension(ext: &str) -> bool {
    matches!(
        ext,
        "3fr"
            | "arw"
            | "bay"
            | "cap"
            | "cr2"
            | "cr3"
            | "crw"
            | "dcr"
            | "dcs"
            | "dng"
            | "drf"
            | "erf"
            | "fff"
            | "iiq"
            | "k25"
            | "kdc"
            | "mdc"
            | "mef"
            | "mos"
            | "mrw"
            | "nef"
            | "nrw"
            | "orf"
            | "ori"
            | "pef"
            | "ptx"
            | "pxn"
            | "raf"
            | "rw2"
            | "rwl"
            | "sr2"
            | "srf"
            | "srw"
            | "x3f"
    )
}

/// Common RAW signatures. Generic TIFF/BigTIFF magic is accepted only with a RAW
/// extension or RAW-specific metadata, because an Explorer stream often has no name.
pub(super) fn looks_like_raw_container(head: &[u8], raw_extension: bool) -> bool {
    let tiff = head.starts_with(b"II\x2A\0")
        || head.starts_with(b"MM\0\x2A")
        || head.starts_with(b"II\x2B\0")
        || head.starts_with(b"MM\0\x2B");
    (tiff && (raw_extension || tiff_has_raw_ifd_marker(head)))
        || head.starts_with(b"FUJIFILMCCD-RAW")
        || head.starts_with(b"FFF\0")
        || head.starts_with(b"FOVb")
        || head.starts_with(b"\0MRM")
        || head.starts_with(b"IIRO")
        || head.starts_with(b"MMOR")
        || head.starts_with(b"IIU\0")
        || (head.len() >= 12
            && &head[4..8] == b"ftyp"
            && (&head[8..12] == b"crx " || &head[8..12] == b"cr3 "))
}

pub(super) fn tiff_has_raw_ifd_marker(head: &[u8]) -> bool {
    let little = match head.get(..4) {
        Some(b"II\x2A\0") => true,
        Some(b"MM\0\x2A") => false,
        _ => return false, // BigTIFF needs an extension; its IFD layout differs.
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let end = offset.checked_add(2)?;
        let bytes: [u8; 2] = head.get(offset..end)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let end = offset.checked_add(4)?;
        let bytes: [u8; 4] = head.get(offset..end)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };

    let Some(ifd) = u32_at(4).map(|value| value as usize) else {
        return false;
    };
    let Some(count) = u16_at(ifd).map(|value| (value as usize).min(4096)) else {
        return false;
    };
    for index in 0..count {
        let Some(entry) = index
            .checked_mul(12)
            .and_then(|offset| ifd.checked_add(2)?.checked_add(offset))
        else {
            return false;
        };
        let Some(tag) = u16_at(entry) else {
            return false;
        };
        // CFA/DNG tags are structurally parsed from IFD0, not searched as arbitrary
        // byte strings. This prevents a normal camera-authored TIFF's EXIF/XMP maker
        // text from accidentally rerouting it through the RAW shortcut.
        if matches!(
            tag,
            0x828D | 0x828E | 0xC612 | 0xC614 | 0xC616 | 0xC61A | 0xC627
        ) {
            return true;
        }
        let Some(type_offset) = entry.checked_add(2) else {
            return false;
        };
        let Some(count_offset) = entry.checked_add(4) else {
            return false;
        };
        let Some(value_offset) = entry.checked_add(8) else {
            return false;
        };
        if tag == 0x0106 && u32_at(count_offset) == Some(1) {
            let Some(field_type) = u16_at(type_offset) else {
                return false;
            };
            let value = match field_type {
                3 => match u16_at(value_offset) {
                    Some(value) => value as u32,
                    None => return false,
                },
                4 => match u32_at(value_offset) {
                    Some(value) => value,
                    None => return false,
                },
                _ => continue,
            };
            // TIFF/EP CFA and LinearRaw photometric interpretations.
            if matches!(value, 32_803 | 34_892) {
                return true;
            }
        }
    }
    false
}
