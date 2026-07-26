//! Shared shell-`IStream` source acquisition for the thumbnail + preview handlers.
//!
//! Both handlers receive the same kind of shell `IStream` and need the same
//! "get me something decodable WITHOUT buffering a multi-GB file" cascade:
//! video frame-grab tiers, seek-only audio album art, streamed archive covers,
//! the head-preview prefix rescue, and the bounded whole-file read. This module
//! owns that cascade ([`stream_source`]) plus the low-level `IStream` helpers
//! it is built from, so the two handlers can't drift apart. Everything here
//! runs on the CALLING (COM apartment) thread — the marshaled stream is
//! apartment-bound and must not be touched from a worker.

use core::ffi::c_void;

use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATFLAG_NONAME, STATSTG, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};

use crate::{decode, safety};

// The whole-file read ceiling, shared with the path-reading verbs via
// `decode::limits::MAX_INPUT_BYTES` (one DoS budget, not two copies).
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

/// What [`stream_source`] hands back: a video frame Media Foundation already
/// decoded (no bytes to re-decode), bounded raw bytes for the caller's tiered
/// byte decoder, or a generic archive's picked cover images for the
/// contact-sheet compositor (`decode::thumbnail_from_covers`).
pub enum StreamSource {
    Frame(image::DynamicImage),
    Bytes(Vec<u8>),
    Covers(Vec<Vec<u8>>),
}

/// Turn the shell's `IStream` into a decodable source without ever buffering an
/// unbounded file. `who` prefixes the debug-log lines ("GetThumbnail" /
/// "DoPreview"); `max_file_bytes` is the user's MaxSize cap. Purpose-built
/// previews may sidestep it when their cost is inherently small (one video frame,
/// album art, or a comic's declared cover); generic ZIP/RAR/7z contact sheets
/// honor it because discovering pictures in an arbitrary project archive can
/// itself be expensive.
///
/// The cascade, in order:
/// 1. VIDEO — an MP4/MOV/M4V video shares the ISO-BMFF `ftyp` box with M4A/M4B
///    audio, so the audio probe below would otherwise claim it and, finding no
///    cover art, bail before the frame-grab ever ran (every .mp4 got a blank
///    icon, then the shell cached that failure forever). The `is_video_magic`
///    sniff keys off the container brand, so it tells a video file from audio /
///    HEIC. If it IS video but no tier decodes a frame, stop (default icon /
///    blank pane) rather than buffering the whole file only to fail decoding it
///    as an image — EXCEPT ambiguous OggS, which falls through to the audio path.
/// 2. AUDIO — the album art lives in the metadata, so seek straight to it and
///    read ONLY the art (not the whole file). Sidesteps the size cap AND avoids
///    buffering; artless audio stops here (raw audio bytes are not a decodable
///    image, a full read + decode would just burn time and fail).
/// 3. OVERSIZED (past the cap) — streamed container cover (CBZ central
///    directory + one entry; Clip Studio `.clip` tail database) or the
///    head-preview prefix rescue (.blend/PSD-PSB baked previews sit in the
///    first bytes); otherwise skip.
/// 4. Everything else: bounded whole-file read.
pub unsafe fn stream_source(
    stream: &IStream,
    max_file_bytes: u64,
    who: &str,
) -> Result<StreamSource> {
    if peek_is_video(stream) {
        // Decode by FILE PATH when we can recover it: Media Foundation reading a
        // multi-GB movie through the shell's IStream is catastrophically slow
        // (30 s+, a pegged core, past Explorer's timeout), while opening the file
        // directly is <1 s. We otherwise decode video IN MEMORY off a bounded
        // read, NEVER streaming the multi-GB original through the shell IStream.
        // Tiers, each fast or a fast miss:
        //   1. by file path, if the host exposes one (non-sandboxed callers) —
        //      MF seeks the real file to the true 30% representative frame;
        //   2. SMART TARGETED READ (MP4/MOV): parse the moov index, build a tiny
        //      one-keyframe MP4 for the sync sample nearest ~30%, decode that —
        //      single-digit MB (index + one keyframe), a representative frame, and
        //      it works regardless of moov position (faststart or moov-at-end);
        //   3. SMART TARGETED READ (Matroska/WebM): the EBML analog — read the Cues
        //      index, build a tiny one-cluster MKV for the keyframe nearest ~30%;
        //   4. GENERAL targeted read (AVI/WMV/… + any unmapped MP4/MKV): let MF's own
        //      demuxer seek the real index to ~30% over a block-caching IStream that
        //      coalesces its reads (no per-format parser, any container MF decodes);
        //   5. a faststart MP4 / small / unindexed video decodes from its head prefix;
        //   6. a big *non*-faststart MP4 (moov at the very end) is remuxed —
        //      head frames + tail moov stitched into a small valid MP4.
        // Tiers 5–6 stay as fallbacks for anything tier 4's demuxer can't seek.
        let frame = stream_path(stream)
            .and_then(|p| crate::video::frame_from_path(&p))
            .or_else(|| {
                crate::mp4::keyframe_mini_mp4(
                    &mut IStreamReader {
                        stream: stream.clone(),
                    },
                    0.30,
                )
                .and_then(|buf| crate::video::frame_from_bytes(&buf))
            })
            .or_else(|| {
                crate::mkv::keyframe_mini_mkv(
                    &mut IStreamReader {
                        stream: stream.clone(),
                    },
                    0.30,
                )
                .and_then(|buf| crate::video::frame_from_bytes(&buf))
            })
            .or_else(|| {
                // MF demuxes AVI/WMV/etc. directly; the block-caching stream makes its
                // seek-to-30% reads cheap (the old shell-IStream meltdown was thousands
                // of tiny marshaled reads — here they coalesce into a few big ones).
                stream_size(stream)
                    .and_then(|size| crate::video::frame_from_block_stream(stream, size, 0.30))
            })
            .or_else(|| video_prefix(stream).and_then(|buf| crate::video::frame_from_bytes(&buf)))
            .or_else(|| {
                mp4_remux_moov(stream).and_then(|buf| crate::video::frame_from_bytes(&buf))
            });
        if let Some(frame) = frame {
            safety::log_debug(&format!(
                "{who}: video frame {}x{}",
                frame.width(),
                frame.height()
            ));
            return Ok(StreamSource::Frame(frame));
        }
        // No decodable frame. OggS is ambiguous — an audio-only .ogg/.opus matches
        // the video magic too, so fall THROUGH to the album-art path below instead of
        // failing. A genuine video container the OS can't decode stops here.
        if !peek_is_ogg(stream) {
            safety::log_debug(&format!("{who}: video with no decodable frame"));
            return Err(Error::from(E_FAIL));
        }
        safety::log_debug(&format!("{who}: OggS not video — trying album art"));
    }

    match audio_art(stream) {
        AudioArt::Art(art) => return Ok(StreamSource::Bytes(art)),
        AudioArt::NoArt => {
            safety::log_debug(&format!("{who}: audio file has no embedded art"));
            return Err(Error::from(E_FAIL));
        }
        AudioArt::NotAudio => {}
    }

    // GENERIC archive (a registered .zip/.rar/.7z — NOT the cbz/epub/office/… zips,
    // which keep their dedicated cover paths): identify the image entries from the
    // archive's file LIST (central directory / headers — never a full decompress),
    // then pull only those. Gated on the Stat-recovered file extension so the
    // magic alone can't reroute a comic, and on MaxSize before any archive parser
    // runs so a huge project backup remains a cheap stock icon.
    match generic_archive(stream, max_file_bytes, who) {
        ArchiveProbe::NotGeneric => {}
        ArchiveProbe::NoCover => {
            // A recognized generic archive with no readable image: fail now so
            // Explorer shows the stock zip icon — buffering the whole file just to
            // fail the image tiers on raw archive bytes would prove nothing.
            safety::log_debug(&format!("{who}: generic archive with no image entries"));
            return Err(Error::from(E_FAIL));
        }
        ArchiveProbe::Found(src) => return Ok(src),
    }

    // HEAD-PREVIEW fast path (opaque PSD/PSB, plain .blend): the baked preview
    // lives in the file's head, so reading the whole (possibly ~100 MB) document
    // through the marshaled IStream just to slice out a ~160px JPEG is the
    // dominant per-thumbnail cost in a big PSD folder — Explorer extracts
    // serially, so every file pays it in turn. Read a bounded prefix (exact
    // resources-section end for PSD) and commit to it ONLY when the same
    // extractor the decode tier runs actually finds a preview in it; otherwise
    // fall through to the normal paths (a PSD with no baked thumbnail still
    // renders via the full tiers exactly as before). Runs for ANY size — the win
    // is under-cap files, which used to pay the whole-file read; an oversized
    // hit just gets the exact prefix instead of the blanket rescue below.
    // Transparent PSDs skip this (preview_prefix_len bows out) — their composite
    // needs the full bytes.
    if let Some(prefix) = head_preview_fast(stream) {
        safety::log_debug(&format!(
            "{who}: head-preview fast path ({} bytes)",
            prefix.len()
        ));
        return Ok(StreamSource::Bytes(prefix));
    }

    // CAMERA-RAW embedded-preview fast path.  A number of RAW families put a
    // display JPEG near the front of the file, followed by tens or hundreds of
    // MiB of sensor data. Do not turn every TIFF into this path: it needs a RAW
    // extension or RAW-specific container metadata, plus a structurally valid
    // preview. On a miss (including a preview beyond this bounded prefix) the
    // old whole-file path remains the correctness backstop.
    if let Some(raw) = raw_preview_fast(stream, max_file_bytes) {
        match raw {
            RawFastSource::Preview(preview) => {
                safety::log_debug(&format!(
                    "{who}: RAW embedded-preview fast path ({} bytes)",
                    preview.len()
                ));
                return Ok(StreamSource::Bytes(preview));
            }
            RawFastSource::Prefix(prefix, size) => {
                // No early JPEG: reuse the bytes already fetched while probing and
                // read only the remaining tail. This preserves the old full-decode
                // fallback without rereading the first 16 MiB.
                stream.Seek(prefix.len() as i64, STREAM_SEEK_SET, None)?;
                return Ok(StreamSource::Bytes(read_all_append(
                    stream,
                    decode::effective_input_cap(max_file_bytes) as usize,
                    Some(size),
                    prefix,
                )?));
            }
        }
    }

    // Not audio, not video: skip oversized files cheaply via the stream length
    // before reading into memory. The effective cap is the user's MaxSize but
    // never above the hard MAX_BYTES ceiling ("0 = unlimited" means "up to
    // MAX_BYTES").
    let max = decode::effective_input_cap(max_file_bytes);
    let size = stream_size(stream);
    match size {
        // Oversized: the whole-file read is a DoS risk, so we skip it —
        // EXCEPT a seek-streamable container: a giant ZIP comic archive
        // (CBZ) reads only its central directory + one cover entry
        // over the IStream, and a Clip Studio .clip seeks to the SQLite
        // database at its tail and reads only that. (CBR can't — `rars`
        // needs the full buffer — so a huge .cbr still gets the default
        // icon.) Head-preview containers (.blend / PSD-PSB) get a second
        // rescue: their baked thumbnail sits in the first bytes, so a
        // bounded prefix read suffices no matter the file size (issue #1).
        Some(size) if size > max => {
            if let Some(cover) = archive_cover_streamed(stream) {
                safety::log_debug(&format!("{who}: streamed cover from {size}-byte archive"));
                return Ok(StreamSource::Bytes(cover));
            }
            if let Some(prefix) = head_preview_prefix(stream) {
                safety::log_debug(&format!(
                    "{who}: head-preview prefix ({} bytes) of {size}-byte file",
                    prefix.len()
                ));
                return Ok(StreamSource::Bytes(prefix));
            }
            safety::log_debug(&format!("{who}: skip, {size} bytes over limit"));
            Err(Error::from(E_FAIL))
        }
        None if peek_is_7z(stream) => {
            // A provider stream with neither a recoverable name nor a Stat size
            // cannot prove this is a bounded CB7 rather than a huge project 7z.
            // Do not pull up to hundreds of MiB over a marshaled/network stream
            // merely to discover that after the fact.
            safety::log_debug(&format!(
                "{who}: refusing name-less 7z with unavailable stream size"
            ));
            Err(Error::from(E_FAIL))
        }
        _ => {
            let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            Ok(StreamSource::Bytes(read_all(stream, max as usize, size)?))
        }
    }
}

/// The stream's total size in bytes via `IStream::Stat`, or None if the stream
/// doesn't support it (then the general read is bounded by the effective user +
/// hard cap, while expensive name-less 7z input fails closed).
unsafe fn stream_size(stream: &IStream) -> Option<u64> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
    Some(stat.cbSize)
}

/// Read just enough to recognize a 7z signature and rewind. Used only for the
/// unknown-size fail-closed gate; ZIP remains eligible for its deliberate
/// seek-only CBZ cover rescue.
unsafe fn peek_is_7z(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut signature = [0u8; 6];
    let mut got = 0u32;
    let result = stream.Read(
        signature.as_mut_ptr() as *mut c_void,
        signature.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    result.is_ok()
        && got as usize == signature.len()
        && signature == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]
}

/// Sniff the stream head for a video container we can frame-grab (Matroska/WebM, MP4/MOV,
/// AVI, ASF/WMV, …). Rewinds to 0 either way so the subsequent MF / whole-file read starts
/// clean. HEIC/AVIF and M4A/M4B share MP4's `ftyp` box but are excluded by `is_video_magic`.
unsafe fn peek_is_video(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // 208 bytes: enough to verify the MPEG-TS / M2TS sync-byte STRIDE (a second 0x47 at
    // offset 188 / 196) so we don't false-match any file that merely starts with 'G'.
    let mut head = [0u8; 208];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    hr.is_ok() && crate::video::is_video_magic(&head[..got])
}

/// Is the stream head the Ogg container magic (`OggS`)? Ogg carries both video (.ogv) and
/// audio (Vorbis/Opus/Speex), so a video frame-grab miss on an Ogg means it's audio-only —
/// the caller then falls back to the album-art path instead of failing.
unsafe fn peek_is_ogg(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 4];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, 4, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    hr.is_ok() && got == 4 && &head == b"OggS"
}

/// Recover the backing file path from the shell's `IStream` via `IStream::Stat`
/// (`STATFLAG_DEFAULT` fills `pwcsName` — file-backed shell streams report the full path).
/// Returned only when it names an existing file, so a stream with no / non-file name simply
/// falls back to streaming. `pwcsName` is a CoTaskMem allocation we own and must free.
unsafe fn stream_path(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    let s = s?;
    if std::path::Path::new(&s).is_file() {
        Some(s)
    } else {
        None
    }
}

/// File-name extension reported by a shell stream, without requiring that the
/// name be an absolute, currently-existing path.  Virtual shell sources often
/// report only a display name; that is still enough for a conservative format
/// gate, whereas [`stream_path`] intentionally rejects it for direct file I/O.
unsafe fn stream_extension(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let name = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    name.and_then(|name| {
        std::path::Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
    })
}

const RAW_PREFIX_BYTES: usize = 16 * 1024 * 1024;
const RAW_SNIFF_BYTES: usize = 1024 * 1024;

enum RawFastSource {
    Preview(Vec<u8>),
    Prefix(Vec<u8>, u64),
}

fn raw_preview_size_allowed(size: u64, max_file_bytes: u64) -> bool {
    size > RAW_PREFIX_BYTES as u64 && size <= decode::effective_input_cap(max_file_bytes)
}

/// Read only a bounded RAW head and return its best complete embedded JPEG.
/// A RAW with no early preview retains the prefix so the existing bounded
/// whole-file fallback can append only the unread tail.
unsafe fn raw_preview_fast(stream: &IStream, max_file_bytes: u64) -> Option<RawFastSource> {
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

fn is_raw_extension(ext: &str) -> bool {
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
fn looks_like_raw_container(head: &[u8], raw_extension: bool) -> bool {
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

fn tiff_has_raw_ifd_marker(head: &[u8]) -> bool {
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

/// Read up to a bounded PREFIX off the stream head in big sequential gulps, for the
/// in-memory video decode. A *faststart* MP4 keeps its `moov` index + first seconds of
/// frames here, so Media Foundation can seek/decode freely in RAM — sidestepping the
/// catastrophically slow random access (and marshaled per-read overhead) MF otherwise
/// suffers reading the multi-GB original through the shell's `IStream`. Returns
/// None for a too-short read; a non-faststart file (moov at the end) simply won't decode
/// from the prefix and the caller falls back. Rewinds the stream to 0 afterwards.
unsafe fn video_prefix(stream: &IStream) -> Option<Vec<u8>> {
    const PREFIX: usize = 64 * 1024 * 1024;
    stream_prefix(stream, PREFIX)
}

/// Read up to `max` bytes off the stream head in big sequential gulps, rewinding to 0
/// before and after. Shared by the video-prefix decode and the head-preview rescue —
/// the bounded read is the same, only the cap differs.
unsafe fn stream_prefix(stream: &IStream, max: usize) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let cap = stream_size(stream).map_or(max, |sz| (sz as usize).min(max));
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    while filled < cap {
        let mut got: u32 = 0;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            (cap - filled) as u32,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    buf.truncate(filled);
    (filled >= 64).then_some(buf)
}

/// The head-preview fast path (see the call site in [`stream_source`]): bounded-
/// prefix read + probe for an opaque PSD/PSB or plain `.blend`, any file size.
/// Returns the prefix only when it is strictly smaller than the file (no byte
/// savings otherwise) AND [`crate::container::extract_cover`] — the same extractor
/// the decode tier will run on it — actually finds a preview inside. Any miss
/// returns None and the caller proceeds exactly as before this path existed.
/// Rewinds via `stream_prefix` on the hit path and explicitly on the miss paths.
unsafe fn head_preview_fast(stream: &IStream) -> Option<Vec<u8>> {
    let size = stream_size(stream)?;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    if hr.is_err() || (got as usize) < head.len() {
        return None;
    }
    // G-code carries no magic bytes, so it is reachable only by extension — the
    // same Stat-recovered name the generic-archive probe uses. A stream with no
    // recoverable name (rare virtual sources) simply misses that one member.
    let ext = stream_path(stream).map(|p| p.rsplit('.').next().unwrap_or("").to_ascii_lowercase());
    let wanted = crate::container::head_preview_len(
        &head,
        ext.as_deref(),
        &mut IStreamReader {
            stream: stream.clone(),
        },
        decode::HEAD_PREVIEW_BYTES as u64,
    );
    // The length probe seeks the SHARED stream around; park it back at 0 before
    // any return. Every downstream consumer re-seeks anyway — this is insurance
    // for future ones that might not.
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let wanted = wanted?.min(decode::HEAD_PREVIEW_BYTES as u64);
    if wanted >= size {
        return None; // prefix would be the whole file — the normal read is equivalent
    }
    let prefix = stream_prefix(stream, wanted as usize)?;
    crate::container::extract_cover(&prefix)
        .is_some()
        .then_some(prefix)
}

/// For an OVERSIZED file (past the in-memory cap): if its magic marks a container
/// whose baked preview lives in the head — Blender `.blend` (`TEST` block ~100 bytes
/// in) or Photoshop PSD/PSB (image resource 1036 just past the header) — read a
/// bounded [`decode::HEAD_PREVIEW_BYTES`] prefix and thumbnail from THAT, instead of
/// skipping to the default icon. Big Blender scenes and PSBs routinely exceed the
/// 100 MB default cap while their thumbnails sit in the first kilobytes (GitHub
/// issue #1). Every container extractor is bounds-checked, so a truncated tail just
/// means "no preview found" (default icon — same as before), never a mis-decode.
unsafe fn head_preview_prefix(stream: &IStream) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < head.len() || !crate::container::has_head_preview(&head) {
        return None;
    }
    stream_prefix(stream, decode::HEAD_PREVIEW_BYTES)
}

/// Read exactly `buf.len()` bytes starting at the stream's current position (looping over
/// short reads). None if the stream ends early.
unsafe fn read_full(stream: &IStream, buf: &mut [u8]) -> Option<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut got: u32 = 0;
        let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            want,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    (filled == buf.len()).then_some(())
}

/// Remux a big *non-faststart* MP4 (`moov` at the very end, past the prefix) into a small
/// in-memory MP4 MF can decode an early frame from. We do the I/O ourselves in a few big
/// seeks/reads (NOT MF's slow random access through the shell IStream): keep the file head
/// (ftyp + mdat header + the first frames of mdat) verbatim, rewrite mdat's box size so it
/// ends where we append the real `moov` pulled from the tail. The moov's sample offsets are
/// absolute and point into the early mdat we kept byte-for-byte, so they still resolve;
/// only the early keyframe (≤ our 3 s seek) needs to live within the retained head. Returns
/// None unless this really is a moov-after-mdat MP4 within sane bounds.
unsafe fn mp4_remux_moov(stream: &IStream) -> Option<Vec<u8>> {
    // Early mdat retained — must reach the frame we grab. mp4 mdat interleaving isn't
    // always video-first: a real 24-min/14 GB sample put its first video chunk ~58 MB in,
    // so the ~3 s seek frame landed ~86 MB in. 128 MB covers that with margin; a file that
    // buries video even deeper just fast-fails to the default icon (no hang).
    const HEAD_KEEP: u64 = 128 * 1024 * 1024;
    const MOOV_MAX: u64 = 96 * 1024 * 1024; // sanity cap on the tail moov we'll pull

    let total = stream_size(stream)?;
    // Walk top-level boxes to find mdat (offset + header length) and moov (offset + size).
    let mut pos: u64 = 0;
    let mut mdat: Option<(u64, u64)> = None; // (offset, header_len)
    let mut moov: Option<(u64, u64)> = None; // (offset, full_size)
    while pos + 8 <= total {
        if stream.Seek(pos as i64, STREAM_SEEK_SET, None).is_err() {
            return None;
        }
        let mut hdr = [0u8; 16];
        let mut got: u32 = 0;
        if stream
            .Read(hdr.as_mut_ptr() as *mut c_void, 16, Some(&mut got))
            .is_err()
        {
            return None;
        }
        if (got as usize) < 8 {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let (full, hlen) = if size32 == 1 {
            if (got as usize) < 16 {
                break;
            }
            (u64::from_be_bytes(hdr[8..16].try_into().ok()?), 16u64)
        } else if size32 == 0 {
            (total - pos, 8) // extends to EOF
        } else {
            (size32, 8)
        };
        if full < hlen {
            break;
        }
        match &hdr[4..8] {
            b"mdat" => mdat = Some((pos, hlen)),
            b"moov" => {
                moov = Some((pos, full));
                break;
            }
            _ => {}
        }
        pos = pos.checked_add(full)?;
    }

    let (mdat_off, mdat_hlen) = mdat?;
    let (moov_off, moov_size) = moov?;
    // Only worth it for moov-AFTER-mdat (faststart is already handled by the prefix path).
    if moov_off <= mdat_off || moov_size == 0 || moov_size > MOOV_MAX {
        return None;
    }

    // Retain ftyp + mdat header + early mdat, ending before the moov.
    let keep = HEAD_KEEP.min(moov_off).min(total);
    if keep <= mdat_off + mdat_hlen {
        return None;
    }
    let mut head = vec![0u8; keep as usize];
    if stream.Seek(0, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut head)?;

    // Rewrite mdat's size so the box ends exactly at `keep` (data offset is unchanged).
    let new_mdat = keep - mdat_off;
    let o = mdat_off as usize;
    if mdat_hlen == 16 {
        head[o + 8..o + 16].copy_from_slice(&new_mdat.to_be_bytes());
    } else {
        head[o..o + 4].copy_from_slice(&(new_mdat as u32).to_be_bytes());
    }

    // Pull the moov from the tail (one seek + bulk read) and append it.
    let mut moov_buf = vec![0u8; moov_size as usize];
    if stream.Seek(moov_off as i64, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut moov_buf)?;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);

    head.extend_from_slice(&moov_buf);
    Some(head)
}

/// Outcome of the generic-archive probe: not a generic archive at all (continue
/// the normal cascade), a generic archive with no readable image (fail to the
/// stock icon), or the picked cover image(s).
enum ArchiveProbe {
    NotGeneric,
    NoCover,
    Found(StreamSource),
}

/// A generic project archive is parsed only when its total size is known and
/// inside both the user's preference and the hard decoder ceiling. Unlike a
/// dedicated comic/ebook cover, there is no safe reason to probe an unbounded
/// ZIP/7z directory over an opaque provider stream.
fn checked_generic_archive_size(size: Option<u64>, max_file_bytes: u64) -> Option<u64> {
    let size = size?;
    (size <= decode::effective_input_cap(max_file_bytes)).then_some(size)
}

/// The generic-archive (.zip/.rar/.7z) branch of [`stream_source`]. Fires only
/// when BOTH the magic is an archive signature AND the Stat-recovered file name
/// carries a generic-archive extension — cbz/epub/office/kra packages share the
/// zip magic and must keep their dedicated single-cover paths, and a stream with
/// no recoverable name (rare virtual sources) also falls through to those. ZIP
/// and 7z read the entry list + picked entries over the seekable IStream; RAR
/// must buffer because `rars` accepts no reader. All three honor the caller's
/// MaxSize BEFORE parsing. This is deliberately stricter than dedicated comic/
/// ebook cover extraction: generic project archives can have huge encoded headers,
/// tens of thousands of paths, and no meaningful image at all.
unsafe fn generic_archive(stream: &IStream, max_file_bytes: u64, who: &str) -> ArchiveProbe {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < head.len() || !crate::container::is_generic_archive_magic(&head[..got])
    {
        return ArchiveProbe::NotGeneric;
    }
    let is_generic_ext = stream_path(stream)
        .map(|p| {
            let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            matches!(ext.as_str(), "zip" | "rar" | "7z")
        })
        .unwrap_or(false);
    if !is_generic_ext {
        return ArchiveProbe::NotGeneric;
    }

    // This gate is intentionally before ArchiveReader/ZipArchive/RAR parsing.
    // A real 909 MB solid 7z on an SMB share had 18,037 entries and a 235 KB
    // encoded header; despite the decompression budget, merely parsing it issued
    // thousands of tiny remote reads and blocked the shell for minutes. Apply
    // the hard decoder ceiling as well as the user's MaxSize: Settings represents
    // "0 / unlimited" as u64::MAX, but it is only unlimited within that ceiling.
    let max = decode::effective_input_cap(max_file_bytes);
    let reported_size = stream_size(stream);
    let Some(size) = checked_generic_archive_size(reported_size, max_file_bytes) else {
        let detail = reported_size
            .map(|n| format!("{n} > {max} bytes"))
            .unwrap_or_else(|| "stream size unavailable".to_string());
        safety::log_debug(&format!(
            "{who}: refusing generic archive before parse ({detail})"
        ));
        return ArchiveProbe::NoCover;
    };

    // Contact sheet (up to 4 images) or classic single cover, per Settings.
    let want = if crate::settings::archive_collage() {
        4
    } else {
        1
    };

    let covers = if crate::container::archive_needs_buffer(&head) {
        // RAR: same bounded whole-file read as the normal path, then the one-pass
        // multi-target extraction over the buffer.
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        let Ok(bytes) = read_all(stream, MAX_BYTES, Some(size)) else {
            return ArchiveProbe::NoCover;
        };
        crate::container::archive_covers(&bytes, want)
    } else {
        crate::container::archive_covers_seek(
            IStreamReader {
                stream: stream.clone(),
            },
            &head,
            want,
        )
    };

    match covers {
        None => ArchiveProbe::NoCover,
        Some(covers) if covers.is_empty() => ArchiveProbe::NoCover,
        Some(mut covers) if covers.len() == 1 => {
            // One image: the normal aspect-preserving single-cover pipeline.
            safety::log_debug(&format!("{who}: generic archive single cover"));
            ArchiveProbe::Found(StreamSource::Bytes(covers.swap_remove(0)))
        }
        Some(covers) => {
            safety::log_debug(&format!("{who}: generic archive {} covers", covers.len()));
            ArchiveProbe::Found(StreamSource::Covers(covers))
        }
    }
}

/// For an OVERSIZED file (past the in-memory cap), sniff whether it's a seek-
/// streamable container — a ZIP comic archive (CBZ: central directory + one
/// cover entry) or a Clip Studio `.clip` (the tail SQLite database holding the
/// canvas preview) — and, if so, pull just the cover over the IStream, never the
/// whole file. Oversized 7z/CB7 is deliberately excluded: unlike ZIP, even
/// discovering its entries may require decoding a large encoded header through
/// a name-less shell stream, where we cannot distinguish a comic from an
/// arbitrary project backup. Returns None for everything else (including CBR,
/// which `rars` can't read without a full buffer), so the caller skips it.
unsafe fn archive_cover_streamed(stream: &IStream) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < head.len() {
        return None;
    }
    if crate::container::is_7z(&head[..got]) {
        return None;
    }
    crate::container::archive_cover_seek(
        IStreamReader {
            stream: stream.clone(),
        },
        &head[..got],
    )
}

/// Result of the audio-art probe. The three cases are distinct so the caller can
/// tell "this isn't audio" (take the normal whole-file path) from "this IS audio
/// but carries no usable art" (stop — the raw audio bytes are not a decodable
/// image, so a full read + decode would just burn time and fail).
enum AudioArt {
    NotAudio,
    NoArt,
    Art(Vec<u8>),
}

/// Sniff the stream for audio and, if so, extract only the embedded art via a
/// seek-only read (lofty seeks to the metadata — we never buffer the whole file,
/// so even a multi-GB audiobook thumbnails). Rewinds the stream to 0 either way.
unsafe fn audio_art(stream: &IStream) -> AudioArt {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 16];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // Never trust the IStream-reported count past the buffer it filled.
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < 12 || !crate::container::looks_like_audio(&head[..got]) {
        return AudioArt::NotAudio;
    }
    match crate::container::audio_art_from_reader(IStreamReader {
        stream: stream.clone(),
    }) {
        Some(art) => AudioArt::Art(art),
        None => AudioArt::NoArt,
    }
}

/// `std::io` Read + Seek over a COM IStream, so lofty can parse tags by seeking
/// instead of us draining the file into memory.
struct IStreamReader {
    stream: IStream,
}

impl std::io::Read for IStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut got: u32 = 0;
        unsafe {
            self.stream.Read(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut got),
            )
        }
        .ok()
        .map_err(std::io::Error::other)?;
        // Never trust the IStream-reported count past the buffer it filled (the
        // sibling reads at `audio_art`/`read_all` clamp the same way) — returning
        // more than `buf.len()` violates the `Read` contract on a hostile stream.
        Ok((got as usize).min(buf.len()))
    }
}

impl std::io::Seek for IStreamReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let (origin, off): (STREAM_SEEK, i64) = match pos {
            std::io::SeekFrom::Start(n) => (STREAM_SEEK_SET, n as i64),
            std::io::SeekFrom::Current(n) => (STREAM_SEEK_CUR, n),
            std::io::SeekFrom::End(n) => (STREAM_SEEK_END, n),
        };
        let mut newpos: u64 = 0;
        unsafe { self.stream.Seek(off, origin, Some(&mut newpos)) }
            .map_err(std::io::Error::other)?;
        Ok(newpos)
    }
}

/// Drain an IStream into a Vec, bounded by `max`.
unsafe fn read_all(stream: &IStream, max: usize, size_hint: Option<u64>) -> Result<Vec<u8>> {
    read_all_append(stream, max, size_hint, Vec::new())
}

/// Continue draining an IStream after an already-read prefix, bounded by `max`.
/// The caller positions the stream immediately after `out` before entering.
unsafe fn read_all_append(
    stream: &IStream,
    max: usize,
    size_hint: Option<u64>,
    mut out: Vec<u8>,
) -> Result<Vec<u8>> {
    if out.len() > max {
        return Err(Error::from(E_FAIL));
    }
    // Pre-size from the (already size-checked) stream length to skip the doubling
    // realloc churn on multi-MB images. Cap the upfront reservation so a stream that
    // lies about its size can't trick us into a giant allocation — the growth loop +
    // the `max` check below still bound the true read.
    let cap = size_hint.map_or(0, |h| (h as usize).min(max).min(64 << 20));
    if cap > out.len() {
        out.reserve(cap - out.len());
    }
    // 1 MiB chunks: the stream is marshaled (often cross-process), so per-Read
    // overhead is real — 64 KiB chunks cost a 100 MB file ~1,600 round trips.
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut got: u32 = 0;
        let hr = stream.Read(
            chunk.as_mut_ptr() as *mut c_void,
            chunk.len() as u32,
            Some(&mut got),
        );
        // S_OK and S_FALSE are both successes; a failing HRESULT is a real transport
        // error (network/cloud-placeholder stream), NOT end-of-stream — don't mistake
        // it for EOF and silently feed a truncated buffer to the decoder.
        hr.ok()?;
        if got == 0 {
            break; // success + 0 bytes == genuine EOF
        }
        let n = (got as usize).min(chunk.len()); // never trust got > buffer
        if n > max - out.len() {
            return Err(Error::from(E_FAIL));
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::psd_testutil::synthetic_psd;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_NONE,
    };
    use windows::Win32::UI::Shell::{SHCreateMemStream, SHCreateStreamOnFileEx};

    /// Run the full source cascade on `bytes` (100 MB cap, like the default
    /// MaxSize) and return the byte payload it hands the decode tiers.
    fn source_bytes(bytes: &[u8]) -> Vec<u8> {
        let stream = unsafe { SHCreateMemStream(Some(bytes)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, 100 << 20, "test") } {
            Ok(StreamSource::Bytes(b)) => b,
            other => panic!(
                "expected StreamSource::Bytes, got {}",
                match other {
                    Ok(StreamSource::Frame(_)) => "Frame".into(),
                    Ok(StreamSource::Covers(_)) => "Covers".into(),
                    Ok(StreamSource::Bytes(_)) => unreachable!(),
                    Err(e) => format!("Err({e})"),
                }
            ),
        }
    }

    #[test]
    fn unlimited_setting_still_obeys_the_hard_archive_cap() {
        assert_eq!(decode::effective_input_cap(u64::MAX), MAX_BYTES as u64);
        assert_eq!(
            decode::effective_input_cap((MAX_BYTES as u64) + 1),
            MAX_BYTES as u64
        );
        assert_eq!(decode::effective_input_cap(1 << 20), 1 << 20);
    }

    fn substantial_jpeg() -> Vec<u8> {
        let image = image::RgbImage::from_fn(320, 240, |x, y| {
            // Deterministic high-detail pixels keep the encoded preview well
            // above the 16 KiB real-preview floor.
            image::Rgb([
                ((x * 37 + y * 13) & 255) as u8,
                ((x * 11 + y * 53) & 255) as u8,
                ((x * 71 + y * 19) & 255) as u8,
            ])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&image::DynamicImage::ImageRgb8(image))
            .expect("encode test JPEG");
        assert!(out.len() >= decode::MIN_RAW_PREVIEW);
        out
    }

    fn mark_synthetic_tiff_raw(bytes: &mut [u8]) {
        assert!(bytes.len() >= 26);
        bytes[..8].copy_from_slice(b"II\x2A\0\x08\0\0\0");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&0x0106u16.to_le_bytes()); // PhotometricInterpretation
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes()); // SHORT
        bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
        bytes[18..20].copy_from_slice(&32_803u16.to_le_bytes()); // CFA
    }

    #[test]
    fn raw_prefix_carver_returns_a_complete_early_preview() {
        let jpeg = substantial_jpeg();
        let mut raw = b"II\x2A\0raw-header".to_vec();
        raw.extend_from_slice(&[0x55; 4096]);
        raw.extend_from_slice(&jpeg);
        raw.extend_from_slice(&[0xA5; 4096]);

        let got = decode::largest_embedded_jpeg(&raw, decode::MIN_RAW_PREVIEW)
            .expect("early RAW preview");
        assert_eq!(got, jpeg.as_slice());
        assert!(
            image::load_from_memory(got).is_ok(),
            "must return a full JPEG"
        );
    }

    #[test]
    fn raw_fast_path_gate_rejects_plain_tiff_and_non_raw() {
        assert!(is_raw_extension("pef"));
        assert!(!is_raw_extension("tif"));
        assert!(looks_like_raw_container(b"II\x2A\0rest", true));
        assert!(!looks_like_raw_container(b"II\x2A\0rest", false));
        let mut raw_tiff = [0u8; 26];
        mark_synthetic_tiff_raw(&mut raw_tiff);
        assert!(looks_like_raw_container(&raw_tiff, false));
        assert!(looks_like_raw_container(b"FUJIFILMCCD-RAW", false));
        assert!(!looks_like_raw_container(b"not a camera raw", true));
    }

    #[test]
    fn raw_fast_path_respects_the_configured_input_cap() {
        let large_raw = (RAW_PREFIX_BYTES as u64) + 1;
        assert!(raw_preview_size_allowed(large_raw, u64::MAX));
        assert!(!raw_preview_size_allowed(large_raw, 1024 * 1024));
    }

    #[test]
    fn unnamed_raw_stream_returns_only_its_early_preview_and_honors_max_size() {
        let jpeg = substantial_jpeg();
        let mut raw = vec![0u8; RAW_PREFIX_BYTES + 1];
        mark_synthetic_tiff_raw(&mut raw);
        let jpeg_start = 4096;
        raw[jpeg_start..jpeg_start + jpeg.len()].copy_from_slice(&jpeg);

        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, u64::MAX, "test") } {
            Ok(StreamSource::Bytes(bytes)) => assert_eq!(bytes, jpeg),
            _ => panic!("unnamed RAW stream should use its embedded preview"),
        }

        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        assert!(
            unsafe { stream_source(&stream, 1024 * 1024, "test") }.is_err(),
            "RAW fast path must not bypass the configured MaxSize"
        );
        drop(stream);

        raw[jpeg_start..jpeg_start + jpeg.len()].fill(0);
        *raw.last_mut().unwrap() = 0xA5;
        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, u64::MAX, "test") } {
            Ok(StreamSource::Bytes(bytes)) => {
                assert_eq!(bytes.len(), raw.len());
                assert_eq!(bytes.last(), Some(&0xA5));
                assert_eq!(&bytes[..26], &raw[..26]);
            }
            _ => panic!("RAW without an early preview should keep the full-read fallback"),
        }
    }

    #[test]
    fn real_large_pef_stream_uses_bounded_preview_when_corpus_is_available() {
        let Some(parent) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
            return;
        };
        let path = parent.join("test-corpus-real").join("sample.pef");
        if !path.exists() {
            return;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let stream = unsafe {
            SHCreateStreamOnFileEx(
                PCWSTR(wide.as_ptr()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )
            .expect("SHCreateStreamOnFileEx")
        };
        match unsafe { stream_source(&stream, u64::MAX, "test") } {
            Ok(StreamSource::Bytes(bytes)) => {
                assert!(bytes.len() >= decode::MIN_RAW_PREVIEW);
                assert!(
                    bytes.len() < RAW_PREFIX_BYTES,
                    "must return only the embedded preview, not the RAW prefix"
                );
                let thumb = decode::decode_thumbnail_opts(&bytes, 256, true)
                    .expect("embedded PEF preview should decode");
                assert_eq!(
                    (thumb.width, thumb.height),
                    (256, 171),
                    "shell fast path should match the corpus PEF thumbnail orientation"
                );
            }
            _ => panic!("large PEF should return its bounded embedded preview"),
        }
        drop(stream);
        if com {
            unsafe { CoUninitialize() };
        }
    }

    #[test]
    fn generic_archive_requires_a_known_size_before_parse() {
        assert_eq!(checked_generic_archive_size(None, u64::MAX), None);
        assert_eq!(
            checked_generic_archive_size(Some(decode::limits::MAX_INPUT_BYTES + 1), u64::MAX),
            None
        );
        assert_eq!(
            checked_generic_archive_size(Some(4096), u64::MAX),
            Some(4096)
        );
        assert_eq!(checked_generic_archive_size(Some(4096), 1024), None);
    }

    #[test]
    fn sevenz_unknown_size_probe_is_signature_exact_and_rewinds() {
        const SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(b"payload");
        let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");

        assert!(unsafe { peek_is_7z(&stream) });
        let mut first = [0u8; 6];
        let mut got = 0u32;
        unsafe {
            stream
                .Read(
                    first.as_mut_ptr() as *mut c_void,
                    first.len() as u32,
                    Some(&mut got),
                )
                .unwrap();
        }
        assert_eq!(got as usize, first.len());
        assert_eq!(first, SIGNATURE);

        let not_7z =
            unsafe { SHCreateMemStream(Some(b"PK\x03\x04zip")) }.expect("SHCreateMemStream");
        assert!(!unsafe { peek_is_7z(&not_7z) });
    }

    #[test]
    fn under_cap_opaque_psd_reads_only_the_head_prefix() {
        // 6 MB of layer data behind the resources section: the fast path must
        // hand the decode tiers the exact head prefix, not the whole file.
        let (psd, head_len) = synthetic_psd(3, true, 6 << 20);
        let got = source_bytes(&psd);
        assert_eq!(
            got.len(),
            head_len,
            "fast path should stop at the resources section"
        );
        assert_eq!(&got[..], &psd[..head_len]);
        // And the prefix must actually decode to the baked thumbnail.
        assert!(crate::container::extract_cover(&got).is_some());
    }

    #[test]
    fn psd_without_baked_thumbnail_falls_back_to_the_whole_file() {
        let (psd, _) = synthetic_psd(3, false, 1 << 20);
        let got = source_bytes(&psd);
        assert_eq!(
            got.len(),
            psd.len(),
            "no baked preview -> the pre-fast-path whole read"
        );
    }

    #[test]
    fn under_cap_dwg_reads_only_the_preview_section() {
        // 4 MB of "object database" behind the preview records: the fast path
        // stops right after the PNG record's payload.
        let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
        let got = source_bytes(&dwg);
        assert_eq!(
            got.len(),
            head_len,
            "DWG fast path should stop after the record payload"
        );
        assert!(crate::container::extract_cover(&got).is_some());
    }

    #[test]
    fn dwg_without_a_preview_section_falls_back_to_the_whole_file() {
        // RASTERPREVIEW=0 / pre-R13: no sentinel, so no fast path. The whole-file
        // read then fails the decode tiers exactly as it did before this path.
        let (dwg, _) = crate::container::dwg_testutil::synthetic_dwg(false, 1 << 20);
        let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, 100 << 20, "test") } {
            Ok(StreamSource::Bytes(b)) => assert_eq!(b.len(), dwg.len()),
            Ok(_) => panic!("expected Bytes"),
            Err(_) => panic!("expected the whole-file read, not a failure"),
        }
    }

    /// The fast path sits BEFORE the size-cap branch, so a preview-bearing DWG
    /// past the user's MaxSize now thumbnails off its exact prefix. Previously it
    /// fell to the oversized arm, which has no DWG rescue (`has_head_preview`
    /// covers only blend/PSD/gzip) and returned E_FAIL — the stock icon. This is a
    /// new capability, not just a speedup.
    #[test]
    fn oversized_dwg_now_thumbnails_via_the_exact_prefix() {
        let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
        let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
        // A 1 MiB cap puts this 4 MB+ file firmly over the limit.
        match unsafe { stream_source(&stream, 1 << 20, "test") } {
            Ok(StreamSource::Bytes(b)) => {
                assert_eq!(b.len(), head_len);
                assert!(crate::container::extract_cover(&b).is_some());
            }
            other => panic!(
                "oversized DWG should now yield its preview, got {}",
                other.is_ok()
            ),
        }
    }

    #[test]
    fn transparent_psd_falls_back_to_the_whole_file() {
        // 4 channels in RGB mode = alpha: the composite path needs every byte,
        // so the fast path must bow out even though a baked thumbnail exists.
        let (psd, _) = synthetic_psd(4, true, 1 << 20);
        let got = source_bytes(&psd);
        assert_eq!(got.len(), psd.len());
    }

    /// A shell stream may expose no filename, so the generic-extension gate
    /// cannot distinguish `.7z` from `.cb7`. Oversized 7z must still stop at
    /// MaxSize instead of falling into the old cap-bypassing CB7 rescue.
    #[test]
    fn nameless_oversized_7z_is_not_streamed_past_max_size() {
        const ARCHIVE: &[u8] = include_bytes!("../tests/fixtures/sevenz/solid_order.7z");
        let path = std::env::temp_dir().join(format!("st2k_generic_cap_{}.7z", std::process::id()));
        std::fs::write(&path, ARCHIVE).expect("write fixture");
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let open = || unsafe {
            SHCreateStreamOnFileEx(
                PCWSTR(wide.as_ptr()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )
            .expect("SHCreateStreamOnFileEx")
        };

        let stream = open();
        let stat_path = unsafe { stream_path(&stream) };
        assert!(
            stat_path.is_none(),
            "fixture must exercise the name-less shell-stream path, got {stat_path:?}"
        );
        assert!(
            unsafe { stream_source(&stream, 1, "test") }.is_err(),
            "over-MaxSize name-less 7z must keep the stock icon"
        );
        drop(stream);

        if com {
            unsafe { CoUninitialize() };
        }
        let _ = std::fs::remove_file(path);
    }
}
