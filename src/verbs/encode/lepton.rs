//! Lepton (.lep) ENCODING — the lossless JPEG recompression writer, mirror of the
//! decode tier in `crate::decode::lepton`. A `.lep` container holds a bit-exact
//! copy of the original JPEG bytes, so the encode entry points recompress only
//! JPEG-family sources losslessly (`encode_lepton_file`); every other source
//! needs pixels, and a JPEG re-encode (at the caller's quality) is LOSSY, so the
//! convert verbs refuse non-JPEG sources outright (distinct
//! [`LEPTON_NEEDS_JPEG_SOURCE`]) while the EDIT paths (resize/rotate of an
//! existing `.lep`) accept the lossy re-encode — the same trade the `.jpg` edit
//! paths already make.
//!
//! Bomb guards mirror the decode side: input JPEG ≤ 128 MiB (a container
//! declaring more is rejected by OUR OWN decoder — encoding it would be
//! self-inconsistent), dims ≤ the crate's `max_jpeg_width/height` (16384), and a
//! named cross-process semaphore caps concurrent encodes at 2 (~2 GiB worst-case
//! coefficient buffers) exactly like the magick gate caps its children.

use std::path::Path;

use image::DynamicImage;
use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::E_FAIL;

use lepton_jpeg::{encode_lepton_verify, EnabledFeatures, LeptonThreadPriority, SimpleThreadPool};

use super::slots::write_atomic;

/// JPEG size budget for the lepton container — mirrors the decode tier's
/// `MAX_JPEG_FILE_SIZE` (decode/lepton.rs). The encoder does not enforce this on
/// its input (it writes the actual size into the container's declared field), so
/// we gate BEFORE encoding: a larger JPEG would produce a `.lep` that
/// SageThumbs-2k itself cannot decode.
const MAX_JPEG_FILE_SIZE: usize = 128 * 1024 * 1024;

/// Distinct HRESULT for "Lepton output requires a JPEG source" — the CLI can
/// surface the actual reason instead of the blanket `E_FAIL` every other failure
/// returns (same pattern as `ocr::OCR_IMAGE_TOO_LARGE`). The menu/dialog paths
/// only log; no i18n key involved (verb errors are hard-coded English, like every
/// other verb).
pub(crate) const LEPTON_NEEDS_JPEG_SOURCE: HRESULT = HRESULT(0x8FFF_0001u32 as i32);

/// Distinct HRESULT for "the JPEG source exceeds the 128 MiB lepton container
/// budget" — a JPEG this big cannot be recompressed losslessly AND would be
/// rejected by our own decoder as a container. Surfaced (instead of the blanket
/// `E_FAIL`) so the CLI can say the file was refused for SIZE, not for being a
/// non-JPEG source.
pub(crate) const LEPTON_SOURCE_TOO_LARGE: HRESULT = HRESULT(0x8FFF_0002u32 as i32);

/// The encode feature set: the decode tier's features (decode/lepton.rs) with the
/// two STRICTER gates that only make sense for files we produce ourselves —
/// `reject_dqts_with_zeros` and `accept_invalid_dht` flip to their strict
/// settings, so a malformed source JPEG is a clean error instead of a
/// best-effort decode. Everything else is identical (progressive, 16384 dims,
/// 128 MiB, 2 processors), so anything we encode is decodable by our own tier.
pub(crate) fn lepton_encode_features() -> EnabledFeatures {
    EnabledFeatures {
        progressive: true,
        reject_dqts_with_zeros: true,
        max_jpeg_width: 16384,
        max_jpeg_height: 16384,
        use_16bit_dc_estimate: true,
        use_16bit_adv_predict: true,
        accept_invalid_dht: false,
        max_partitions: 8,
        max_processor_threads: 2,
        max_jpeg_file_size: MAX_JPEG_FILE_SIZE as u32,
        stop_reading_at_eoi: false,
    }
}

/// JPEG-family source extensions — the ONLY sources that can be recompressed
/// losslessly. `.mpo` is deliberately NOT here: it is a real JPEG byte stream the
/// SOI gate would otherwise accept, but recompressing a multi-picture file would
/// silently produce a container for the first frame's bytes.
pub fn jpeg_source_ext(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "jpe" | "jfif")
}

/// Raw-bytes gate for the lossless path: within the container size budget and
/// starting with the JPEG SOI marker.
pub(crate) fn is_lossless_jpeg(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() <= MAX_JPEG_FILE_SIZE && bytes.starts_with(&[0xFF, 0xD8])
}

/// A JPEG-family source that is TOO BIG for a lepton container (> 128 MiB): the
/// lossless recompression cannot take it, and neither can the fallbacks (the
/// decode tier caps the container at the same budget), so it must be refused
/// outright rather than misreported as a non-JPEG source.
pub(crate) fn jpeg_exceeds_lepton_budget(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() > MAX_JPEG_FILE_SIZE && bytes.starts_with(&[0xFF, 0xD8])
}

/// Session-wide cap on concurrent lepton encodes. Each encode can transiently
/// hold ~1 GiB for a max-size (16384²) image — the coefficient buffers are
/// DIMENSION-driven, not file-size-driven — so an unbounded fan-out (the Convert
/// dialog's `available_parallelism` workers, each spawning `st2k.exe`) could
/// exhaust memory. A NAMED semaphore bounds the total across the DLL and every
/// `st2k.exe` child (they share the one kernel object by name), mirroring the
/// magick gate in decode.rs.
mod lepton_gate {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateSemaphoreW(
            attrs: *const c_void,
            initial: i32,
            max: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        fn ReleaseSemaphore(handle: *mut c_void, count: i32, prev: *mut i32) -> i32;
    }

    /// Max concurrent encodes. A single max-size (16384²) encode can transiently
    /// hold ~2 GiB — the dimension-driven coefficient buffers (~1.5 GiB at
    /// 4:4:4) plus the in-call verify roundtrip re-allocation — so 2 concurrent
    /// encodes can reach ~4 GiB worst case. The gate is a best-effort bound
    /// (same pattern as the magick gate), not a hard limit.
    const MAX: i32 = 2;
    /// Bounded acquire deadline (ms), same rationale as the magick gate: a leaked
    /// permit (host process hard-killed mid-encode) must self-heal rather than
    /// wedge the gate to 0 for the whole logon session. 5s is ample for a real
    /// slot to free (an encode is usually <1s); on timeout we proceed UNCAPPED.
    const GATE_WAIT_MS: u32 = 5_000;
    const WAIT_OBJECT_0: u32 = 0;

    fn handle() -> Option<*mut c_void> {
        static H: OnceLock<usize> = OnceLock::new();
        let h = *H.get_or_init(|| {
            // A stable Local\ name → per-logon-session sharing across every process
            // (the DLL + all the st2k.exe children it spawns).
            let name: Vec<u16> = "Local\\SageThumbs2K_LeptonEncodeGate\0"
                .encode_utf16()
                .collect();
            unsafe { CreateSemaphoreW(std::ptr::null(), MAX, MAX, name.as_ptr()) as usize }
        });
        (h != 0).then_some(h as *mut c_void)
    }

    /// Held while an encode runs; releases one slot on drop.
    pub(super) struct Permit(*mut c_void);
    impl Drop for Permit {
        fn drop(&mut self) {
            unsafe { ReleaseSemaphore(self.0, 1, std::ptr::null_mut()) };
        }
    }

    /// Acquire an encode slot, waiting at most [`GATE_WAIT_MS`]. `None` on
    /// failure/timed-out/wedged — the caller proceeds UNCAPPED (best-effort
    /// memory cap, never a blocker).
    pub(super) fn acquire() -> Option<Permit> {
        let h = handle()?;
        (unsafe { WaitForSingleObject(h, GATE_WAIT_MS) } == WAIT_OBJECT_0).then(|| Permit(h))
    }
}

/// Encode raw JPEG bytes to a `.lep` file at `out` (atomic temp + rename, no
/// partial file on failure). This is the LOSSLESS path: the recompression is
/// bit-exact, so EXIF/ICC markers survive untouched (unlike every pixel path,
/// which bakes orientation and folds ICC to sRGB — see `encode_to_opts`).
///
/// The caller is responsible for the source gates
/// ([`jpeg_source_ext`]/[`is_lossless_jpeg`]/size); this function re-verifies the
/// size budget so a hostile caller cannot produce a self-inconsistent container.
pub(crate) fn encode_lepton_file(jpeg: &[u8], out: &Path) -> Result<()> {
    if jpeg.is_empty() || jpeg.len() > MAX_JPEG_FILE_SIZE || !jpeg.starts_with(&[0xFF, 0xD8]) {
        return Err(Error::from(E_FAIL));
    }
    let _permit = lepton_gate::acquire();
    // Per-call pool, like the decode tier: workers exit with the call, so no
    // threads linger inside dllhost/explorer (the project's no-global-pool rule).
    let pool = SimpleThreadPool::new(LeptonThreadPriority::Normal);
    let (lep, _) = encode_lepton_verify(jpeg, &lepton_encode_features(), &pool).map_err(|e| {
        crate::safety::log_debug(&format!("lepton encode error: {e:?}"));
        Error::from(E_FAIL)
    })?;
    write_atomic(out, |tmp| {
        std::fs::write(tmp, &lep).map_err(|_| Error::from(E_FAIL))
    })
}

/// Encode a DECODED image as `.lep` via an in-memory JPEG re-encode at `quality`
/// (LOSSY — the edit/convert path for `.lep` sources and resizes; the only way to
/// represent pixels that are not a raw JPEG). Writes into `tmp` (a
/// [`write_atomic`] temp path). Alpha is flattened onto white, exactly like the
/// JPEG encode arm of `encode_to_opts`.
pub(crate) fn encode_image_to_lepton(img: &DynamicImage, quality: u8, tmp: &Path) -> Result<()> {
    let img = super::flatten_onto_white(img);
    let mut jpeg = Vec::new();
    img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut jpeg, quality,
    ))
    .map_err(|_| Error::from(E_FAIL))?;
    let _permit = lepton_gate::acquire();
    let pool = SimpleThreadPool::new(LeptonThreadPriority::Normal);
    let (lep, _) = encode_lepton_verify(&jpeg, &lepton_encode_features(), &pool).map_err(|e| {
        crate::safety::log_debug(&format!("lepton re-encode error: {e:?}"));
        Error::from(E_FAIL)
    })?;
    std::fs::write(tmp, &lep).map_err(|_| Error::from(E_FAIL))
}
