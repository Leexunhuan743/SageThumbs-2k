//! Getting an early video frame out of a stream without buffering the movie.
//!
//! A bounded head prefix covers *faststart* MP4 (index first). A big non-faststart
//! file keeps its `moov` at the very end, past any sane prefix, so this stitches the
//! head and that tail into a small valid MP4 that Media Foundation can decode a frame
//! from. We do the I/O in a few big seeks ourselves; MF's own random access through a
//! marshaled shell IStream is catastrophically slow.

use super::*;

/// Read up to a bounded PREFIX off the stream head in big sequential gulps, for the
/// in-memory video decode. A *faststart* MP4 keeps its `moov` index + first seconds of
/// frames here, so Media Foundation can seek/decode freely in RAM — sidestepping the
/// catastrophically slow random access (and marshaled per-read overhead) MF otherwise
/// suffers reading the multi-GB original through the shell's `IStream`. Returns
/// None for a too-short read; a non-faststart file (moov at the end) simply won't decode
/// from the prefix and the caller falls back. Rewinds the stream to 0 afterwards.
pub(super) unsafe fn video_prefix(stream: &IStream) -> Option<Vec<u8>> {
    const PREFIX: usize = 64 * 1024 * 1024;
    stream_prefix(stream, PREFIX)
}

/// Remux a big *non-faststart* MP4 (`moov` at the very end, past the prefix) into a small
/// in-memory MP4 MF can decode an early frame from. We do the I/O ourselves in a few big
/// seeks/reads (NOT MF's slow random access through the shell IStream): keep the file head
/// (ftyp + mdat header + the first frames of mdat) verbatim, rewrite mdat's box size so it
/// ends where we append the real `moov` pulled from the tail. The moov's sample offsets are
/// absolute and point into the early mdat we kept byte-for-byte, so they still resolve;
/// only the early keyframe (≤ our 3 s seek) needs to live within the retained head. Returns
/// None unless this really is a moov-after-mdat MP4 within sane bounds.
pub(super) unsafe fn mp4_remux_moov(stream: &IStream) -> Option<Vec<u8>> {
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
