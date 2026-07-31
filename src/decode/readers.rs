//! Getting decodable BYTES (or a decoded image) from a PATH.
//!
//! The bounded whole-file read, the head-preview prefix rescues for containers whose
//! baked thumbnail sits in the first bytes, and the streaming decodes that skip the
//! in-memory caps entirely (OpenEXR). The by-PATH twin of [`crate::streamsrc`], which
//! does the same job for the shell's `IStream`.

use super::*;

/// Resolve a user-configured whole-file limit against the non-negotiable decode
/// ceiling. Settings represents "Unlimited" as `u64::MAX`; that removes the
/// smaller user preference, not this process-wide allocation/parse safety cap.
pub(crate) fn effective_input_cap(configured_max: u64) -> u64 {
    configured_max.min(limits::MAX_INPUT_BYTES)
}

/// Read a whole file into memory, refusing anything past [`limits::MAX_INPUT_BYTES`]
/// (checked via metadata BEFORE allocating). The Explorer thumbnail path (its
/// stream cap) and the path-reading verbs (`verbs::encode::read_capped`) already
/// share this DoS budget; this is the same guard for the front ends that read by
/// path directly — the `st2k` CLI's `thumbnail`/`ocr` verbs (and, through them, the
/// MCP tools), which otherwise `std::fs::read` an arbitrarily large file wholesale
/// before decoding. So "too big to load" means the same thing on every path.
pub fn read_capped(path: &str) -> std::io::Result<Vec<u8>> {
    let len = std::fs::metadata(path)?.len();
    if len > limits::MAX_INPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "input is {len} bytes, over the {} byte limit",
                limits::MAX_INPUT_BYTES
            ),
        ));
    }
    std::fs::read(path)
}

/// The scaled-EXR edge used by the by-path front ends (`st2k thumbnail`, the Quick
/// preview viewer). Both consume the result at screen scale, and 2048 keeps a 12K
/// render pass crisp in a maximized viewer while still bounding the work.
pub const EXR_PATH_EDGE: u32 = 2048;

/// Does this head start with the OpenEXR magic? The stream cascade uses it to
/// route an EXR into [`exr_scaled_from_reader`] before anything buffers it.
pub fn is_exr_magic(head: &[u8]) -> bool {
    exrscale::is_exr_magic(head)
}

/// Is this file an OpenEXR? Cheap magic peek used to route a path/stream into the
/// streaming scaled decoder BEFORE anything tries to buffer it.
pub(super) fn file_is_exr(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && exrscale::is_exr_magic(&magic)
}

/// Decode an OpenEXR from a seekable source to a display-ready 8-bit sRGB image at
/// most `target_edge` px on its long side, WITHOUT buffering the file or ever
/// materializing the full-resolution float image (see [`exrscale`]). Returns `Err`
/// for anything outside that decoder's supported subset, which is the caller's cue
/// to fall through to the ordinary tiers.
pub fn exr_scaled_from_reader<R: Read + std::io::Seek>(
    src: R,
    target_edge: u32,
) -> Result<DynamicImage> {
    let float = exrscale::decode_scaled(src, target_edge)?;
    Ok(tone_map_float(&float))
}

/// The by-path decodes that STREAM off the file handle instead of buffering it,
/// scaled to `target_edge` as they read. `None` means "not one of these" (or the
/// streaming decoder declined the file), and the caller should take the ordinary
/// [`read_preview_capped`] + [`decode_preview`] route unchanged.
///
/// Today the only such rescue is OpenEXR, whose 12K+ render passes routinely blow
/// past both the user's MaxSize and [`limits::MAX_INPUT_BYTES`] and so never
/// reached a decoder at all.
pub fn decode_preview_streamed(path: &str, target_edge: u32) -> Option<DynamicImage> {
    if !file_is_exr(path) {
        return None;
    }
    match std::fs::File::open(path)
        .map_err(|_| Error::from(E_FAIL))
        .and_then(|f| exr_scaled_from_reader(f, target_edge))
    {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debug(&format!("scaled EXR decode failed: {e}"));
            None
        }
    }
}

/// Preview-fidelity decode BY PATH: [`decode_preview_streamed`] first, then the
/// ordinary bounded read + tiered decode. Behaviour for every format the streaming
/// tier doesn't claim is byte-for-byte what it was.
pub fn decode_preview_path(path: &str, target_edge: u32) -> Result<DynamicImage> {
    if let Some(img) = decode_preview_streamed(path, target_edge) {
        return Ok(img);
    }
    let bytes = read_preview_capped(path).map_err(|_| Error::from(E_FAIL))?;
    decode_preview(&bytes)
}

/// Bounded head prefix that's ample for every [`crate::container::has_head_preview`]
/// format: a Blender `TEST` thumbnail block sits ~100 bytes in, and a Photoshop
/// image-resources section (baked preview, resource 1036) is at most a few MB past
/// the fixed header. 16 MiB covers both with wide margin while staying a trivial
/// read/allocation next to the 100 MB+ files this path exists for.
pub const HEAD_PREVIEW_BYTES: usize = 16 * 1024 * 1024;

/// PREVIEW-fidelity variant of [`read_capped`] for the thumbnail/view verbs: a file
/// over the byte limit is still readable when its baked preview lives in the head
/// (`.blend` / PSD-PSB — see [`crate::container::has_head_preview`]); we then return
/// only a [`HEAD_PREVIEW_BYTES`] prefix, which the container tier extracts the
/// preview from (every extractor is bounds-checked, so a truncated tail just means
/// "no preview found", never a mis-decode). Seek-streamable containers (CBZ/ZIP/CB7,
/// Clip Studio `.clip`) instead get their cover pulled over the file handle — the
/// same [`crate::container::archive_cover_seek`] dispatch the thumbnail provider
/// uses on its oversized IStream path — and the returned COVER bytes flow through
/// the decode tiers like any image file. Anything else keeps [`read_capped`]'s
/// hard refusal. NOT for full-fidelity verbs (convert/rotate/strip) — a truncated
/// read there would corrupt output.
pub fn read_preview_capped(path: &str) -> std::io::Result<Vec<u8>> {
    read_preview_capped_at(path, limits::MAX_INPUT_BYTES, HEAD_PREVIEW_BYTES)
}

/// [`read_preview_capped`] with the caps as parameters so tests can exercise the
/// oversized branch without staging multi-hundred-MB files.
pub(super) fn read_preview_capped_at(
    path: &str,
    max: u64,
    prefix: usize,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let len = std::fs::metadata(path)?.len();
    if len <= max {
        // UNDER-CAP head-preview fast path (opaque PSD/PSB, plain .blend): the
        // baked preview lives in the head, so read a bounded prefix instead of
        // the whole (possibly ~100 MB) document — the by-path twin of the
        // thumbnail provider's IStream fast path (`streamsrc::head_preview_fast`).
        // Committed only when the prefix actually yields a preview; any miss
        // falls back to the full read below, byte-for-byte as before.
        if let Some(head) = head_preview_file_fast(path, len, prefix) {
            return Ok(head);
        }
        return std::fs::read(path);
    }
    // Sniff just the magic before committing to a rescue, so a plain oversized
    // file is rejected without touching more than 8 bytes of it.
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if crate::container::has_head_preview(&magic) {
        let mut head = vec![0u8; prefix.min(len as usize)];
        head[..8].copy_from_slice(&magic);
        f.read_exact(&mut head[8..])?;
        return Ok(head);
    }
    // The magic sets are disjoint, so this runs only when the head path didn't.
    if let Some(cover) = crate::container::archive_cover_seek(&mut f, &magic) {
        return Ok(cover);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("input is {len} bytes, over the {max} byte limit"),
    ))
}

/// The under-cap fast path of [`read_preview_capped_at`]: bounded-prefix read +
/// probe for a head-preview container. Returns the prefix only when it is
/// strictly smaller than the file AND [`crate::container::extract_cover`] — the
/// same extractor the decode tiers will run — finds a preview inside it. Any
/// miss (not a head-preview magic, transparent PSD, malformed sections, I/O
/// error) returns None and the caller does the normal whole-file read.
pub(super) fn head_preview_file_fast(path: &str, len: u64, prefix_cap: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    // G-code carries no magic bytes, so it is reachable only by extension.
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let wanted =
        crate::container::head_preview_len(&magic, ext.as_deref(), &mut f, prefix_cap as u64)?
            .min(prefix_cap as u64);
    if wanted >= len {
        return None; // prefix would be the whole file — the normal read is equivalent
    }
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = vec![0u8; wanted as usize];
    f.read_exact(&mut buf).ok()?;
    crate::container::extract_cover(&buf)
        .is_some()
        .then_some(buf)
}
