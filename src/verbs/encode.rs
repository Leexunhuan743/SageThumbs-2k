//! Decode → (optional resize / flatten) → encode primitives: the `Target` /
//! `Resize` / `ConvertOpts` descriptors, the size-capped reader, the atomic
//! encode-to-file path, and the per-file convert / transform / resize / email
//! entry points the menu actions and the CLI dispatch to.

use std::{
    io::{Seek, Write},
    path::{Path, PathBuf},
};

use image::{DynamicImage, ImageFormat};
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use super::menu::{EmailSize, Transform};
use crate::decode;

/// A conversion target: the image-crate format and the file extension to use.
#[derive(Clone, Copy)]
pub struct Target {
    pub format: ImageFormat,
    pub ext: &'static str,
    /// `Some(q)` selects LOSSY WebP at quality `q` (libwebp, the `webp-lossy`
    /// feature) — used by the quick "Convert into ▸ WebP" verb so it produces the
    /// small files WebP exists for. `None` keeps the pure-Rust lossless encoder.
    /// Ignored for every non-WebP format. The Convert… dialog drives its own WebP
    /// quality through [`ConvertOpts::webp_quality`], so the `Target` it builds
    /// leaves this `None`.
    pub webp_quality: Option<u8>,
}

/// JPEG quality used by the shrink-for-email presets (a sensible "looks fine in
/// an email, stays small" middle ground, independent of the saved Options value).
const EMAIL_JPEG_QUALITY: u8 = 82;

/// Composite onto white and drop alpha. JPEG has no alpha channel, and a plain
/// `to_rgb8()` would expose whatever color transparent pixels happened to carry
/// (black/colored halos), so blend over white instead.
pub(crate) fn flatten_onto_white(img: &DynamicImage) -> DynamicImage {
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = src.0;
        let a = a as u32;
        let over = |c: u8| (((c as u32) * a + 255 * (255 - a) + 127) / 255) as u8;
        *dst = image::Rgb([over(r), over(g), over(b)]);
    }
    DynamicImage::ImageRgb8(rgb)
}

/// A reserved, collision-free output path. Creating it makes an EMPTY placeholder
/// file with `create_new`, so concurrent workers — even in separate processes (the
/// DLL pre-reserves a name, then `st2k.exe` writes it) — can never pick the same
/// name. (A plain `while path.exists()` check is a TOCTOU race once batches run in
/// parallel.) The writer renames its finished temp ON TOP of the placeholder
/// (`write_atomic` does exactly this), turning it into the real file.
///
/// On drop the placeholder is removed IFF it's still a zero-byte file: an
/// abandoned/failed reservation never litters, while a real (non-empty) output is
/// never touched. No explicit "commit" is needed — a successful write leaves a
/// non-empty file behind, which drop keeps.
pub(crate) struct OutSlot(PathBuf);

impl OutSlot {
    /// The reserved path — hand this to the encoder / `st2k`.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Consume the slot without running its drop cleanup, returning the reserved
    /// path. For callers where the placeholder is replaced by something OTHER than
    /// an encoder write — e.g. `fs::rename`/`fs::copy` landing an existing file on
    /// top of it — where a legitimately empty source would otherwise trip the
    /// zero-byte-placeholder heuristic and get deleted right after the move.
    pub(crate) fn release(self) -> PathBuf {
        let path = self.0.clone();
        std::mem::forget(self);
        path
    }
}

impl Drop for OutSlot {
    fn drop(&mut self) {
        // Remove only a still-empty placeholder: a successful write replaced it with
        // a non-empty file (keep it); a failed/abandoned one left it at zero bytes
        // (clean it up). Image encoders never produce a 0-byte success.
        if std::fs::metadata(&self.0).map(|m| m.len() == 0).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Atomically reserve the first free path produced by `name(n)` for n = 0, 1, 2…,
/// by creating an empty placeholder with `create_new`. See [`OutSlot`].
pub(crate) fn reserve(name: impl Fn(u32) -> PathBuf) -> OutSlot {
    let mut n = 0u32;
    loop {
        let cand = name(n);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&cand) {
            Ok(_) => return OutSlot(cand), // the placeholder handle closes here
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            // Couldn't create (permission / missing dir): hand the name back anyway —
            // the encode will surface the real error. Don't loop on a non-Exists error.
            Err(_) => return OutSlot(cand),
        }
    }
}

/// Reserve a free `<stem>.<ext>` next to `src` (`<stem> (n).<ext>` if taken),
/// atomically (see [`reserve`]). Replaces the old existence-check picker.
pub(crate) fn unique_output(src: &Path, ext: &str) -> OutSlot {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let ext = ext.to_string();
    reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        dir.join(name)
    })
}

/// Read a file into memory, refusing anything past the shared
/// `decode::limits::MAX_INPUT_BYTES` ceiling (checked via metadata before the
/// allocation) so a multi-GB file can't be loaded wholesale. Delegates to
/// [`crate::decode::read_capped`] — the SAME DoS budget the thumbnail path uses —
/// and flattens the io error to `E_FAIL` for the verb call sites.
pub(crate) fn read_capped(path: &str) -> Result<Vec<u8>> {
    crate::decode::read_capped(path).map_err(|_| Error::from(E_FAIL))
}

/// Output extensions written through the bundled ImageMagick.
///
/// The authoritative writer list lives beside ImageMagick's explicit coder
/// mapping. Keeping this as a forwarding predicate prevents exact conversion,
/// transforms, and resizes from disagreeing about formats such as PSD or DDS.
pub(crate) fn ext_needs_magick(ext: &str) -> bool {
    decode::magick_output_supported(ext)
}

/// Map file extensions to formats this build can actually WRITE natively.
///
/// `ImageFormat::from_extension` is deliberately not used for output routing:
/// it also recognizes decoder-only formats (notably DDS/PCX), which previously
/// let a generic PNG fallback create PNG bytes under the source extension.
fn native_output_format(ext: &str) -> Option<ImageFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        "pam" | "ppm" | "pnm" => Some(ImageFormat::Pnm),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        "tga" => Some(ImageFormat::Tga),
        "bmp" => Some(ImageFormat::Bmp),
        "ico" => Some(ImageFormat::Ico),
        "hdr" => Some(ImageFormat::Hdr),
        "exr" => Some(ImageFormat::OpenExr),
        "ff" => Some(ImageFormat::Farbfeld),
        "qoi" => Some(ImageFormat::Qoi),
        _ => None,
    }
}

/// Extension an edit/resize output may truthfully keep.
///
/// Unknown and decoder-only sources fall back to PNG. This helper is shared by
/// the in-process writer and out-of-process routing so their reserved/reported
/// paths cannot drift.
pub(crate) fn edit_output_ext(source_ext: &str) -> &str {
    if ext_needs_magick(source_ext) || native_output_format(source_ext).is_some() {
        source_ext
    } else {
        "png"
    }
}

/// Decode `path` and re-encode it as `target` next to the original, choosing a
/// non-colliding name (never overwrites the source or an existing file) and
/// writing via a temp file + rename so a failed encode leaves no partial file.
/// Returns the output path on success.
pub fn convert_file(path: &str, target: Target) -> Result<std::path::PathBuf> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?;

    let slot = unique_output(Path::new(path), target.ext);

    // Magick-only targets (AVIF/JXL): write to the same-volume temp and replace
    // the reserved placeholder only after a clean child exit, exactly like the
    // native encoders below.
    if ext_needs_magick(target.ext) {
        // The quick "Convert into ▸ AVIF/JXL" verb: magick's default quality (None) — kept
        // byte-identical to before. The Convert… dialog carries an explicit quality instead.
        write_atomic(slot.path(), |tmp| {
            decode::encode_via_magick(&img, tmp, target.ext, None)
        })?;
        preserve_src_time(Path::new(path), slot.path());
        return Ok(slot.path().to_path_buf());
    }

    let img = if matches!(target.format, ImageFormat::Jpeg) {
        flatten_onto_white(&img)
    } else {
        img
    };

    // Honor the target's WebP-quality (lossy for the quick WebP verb), and the
    // saved JPEG/PNG settings — same as `encode_to`, plus the lossy-WebP selector.
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            target.format,
            crate::settings::jpeg_quality(),
            crate::settings::png_level(),
            target.webp_quality,
            target.ext,
            tmp,
        )
    })?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Apply a [`Transform`] and write the result as a NEW file ("<name> (edited)")
/// next to the original — never overwrites the source (a JPEG would re-compress).
/// Keeps the source format. Returns the output path.
pub fn transform_file(path: &str, t: Transform) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();

    // LOSSLESS path for baseline JPEGs: rotate/flip the DCT coefficients directly
    // (no decode-to-pixels, no re-quantize → zero quality loss). Falls through to
    // the lossy re-encode below if the JPEG is outside the supported scope
    // (progressive, non-block-aligned dimensions, etc.).
    if matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
        let op = match t {
            Transform::Right90 => crate::jpegtran::Op::Rot90,
            Transform::Left90 => crate::jpegtran::Op::Rot270,
            Transform::Rotate180 => crate::jpegtran::Op::Rot180,
            Transform::FlipH => crate::jpegtran::Op::FlipH,
            Transform::FlipV => crate::jpegtran::Op::FlipV,
        };
        if let Some(out_bytes) = crate::jpegtran::transform(&bytes, op) {
            let slot = reserve_unique_suffix(src, "edited", &ext);
            write_atomic(slot.path(), |tmp| {
                std::fs::write(tmp, &out_bytes).map_err(|_| Error::from(E_FAIL))
            })?;
            preserve_src_time(src, slot.path());
            return Ok(slot.path().to_path_buf());
        }
    }

    // Pixel fallback: keep the extension only when a real writer exists. Exotic
    // writable formats go through Magick; decoder-only/unknown inputs get an
    // honest PNG sibling instead of PNG bytes disguised by the source suffix.
    let img = decode::decode_full(&bytes)?;
    let out_img = match t {
        Transform::Right90 => img.rotate90(),
        Transform::Left90 => img.rotate270(),
        Transform::Rotate180 => img.rotate180(),
        Transform::FlipH => img.fliph(),
        Transform::FlipV => img.flipv(),
    };
    let out_ext = edit_output_ext(&ext);
    let native_format = if ext_needs_magick(out_ext) {
        None
    } else {
        Some(native_output_format(out_ext).unwrap_or(ImageFormat::Png))
    };
    let slot = reserve_unique_suffix(src, "edited", out_ext);
    write_atomic(slot.path(), |tmp| {
        if let Some(format) = native_format {
            encode_to(&out_img, format, out_ext, tmp)
        } else {
            decode::encode_via_magick(&out_img, tmp, out_ext, None)
        }
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// Resize via a menu preset and write a new "(resized)" file next to the source,
/// keeping the original format. Never upscales. Returns the output path.
pub fn resize_file(path: &str, r: Resize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let img = apply_resize(decode::decode_full(&bytes)?, r);
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    let out_ext = edit_output_ext(&ext);
    let native_format = if ext_needs_magick(out_ext) {
        None
    } else {
        Some(native_output_format(out_ext).unwrap_or(ImageFormat::Png))
    };
    let slot = reserve_unique_suffix(src, "resized", out_ext);
    write_atomic(slot.path(), |tmp| {
        if let Some(format) = native_format {
            encode_to(&img, format, out_ext, tmp)
        } else {
            decode::encode_via_magick(&img, tmp, out_ext, None)
        }
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// `<out>.st2ktmp` — the temp path a write goes to before the atomic rename.
pub(crate) fn with_tmp_suffix(out: &Path) -> PathBuf {
    let mut s = out.to_path_buf().into_os_string();
    s.push(".st2ktmp");
    PathBuf::from(s)
}

/// Atomic write: run `write` against a same-volume `<out>.st2ktmp`, then rename
/// it over `out`. Owns the temp naming ([`with_tmp_suffix`]), the on-error temp
/// cleanup (a failed/partial write leaves no `.st2ktmp` and never an `out`), and
/// a short bounded rename retry (strip.rs-style: 5×40 ms) so a transient
/// Explorer/thumbnail-cache lock (os error 5/32) doesn't fail an otherwise good
/// write. `write` receives the temp path and must produce the finished file there.
pub(crate) fn write_atomic(out: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = with_tmp_suffix(out);
    write(&tmp).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    crate::fsutil::rename_retrying(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })
}

/// If "preserve original file date" is enabled (Options), stamp the source file's
/// modified-time onto a freshly-saved output. Best-effort — never fails a save.
pub(crate) fn preserve_src_time(src: &Path, out: &Path) {
    if !crate::settings::preserve_file_date() {
        return;
    }
    if let Ok(mtime) = std::fs::metadata(src).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(out) {
            let _ = f.set_modified(mtime);
        }
    }
}

/// Reserve a free `<stem> (<suffix>).<ext>` next to `src` (`<stem> (<suffix> n)`
/// if taken), atomically (see [`reserve`]). Used by the IN-PROCESS edit verbs
/// (rotate/resize/email) and the DLL's routed resize/email (which pass the reserved
/// path to `st2k`).
pub(crate) fn reserve_unique_suffix(src: &Path, suffix: &str, ext: &str) -> OutSlot {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let (suffix, ext) = (suffix.to_string(), ext.to_string());
    reserve(move |n| {
        let name = if n == 0 {
            format!("{stem} ({suffix}).{ext}")
        } else {
            format!("{stem} ({suffix} {}).{ext}", n + 1)
        };
        dir.join(name)
    })
}

/// PREDICT (read-only, no reservation) the `<stem> (<suffix>).<ext>` name that
/// [`reserve_unique_suffix`] would currently pick. Used ONLY by the DLL's routed
/// `st2k rotate`, where `st2k` auto-names the sibling itself — the DLL must guess
/// the name to reveal it WITHOUT creating a placeholder (a placeholder would push
/// st2k's own picker to `(… 2)`). Rotate names derive from the distinct source
/// stem, so parallel rotates over a selection don't collide on the prediction.
pub(crate) fn predict_unique_suffix(src: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cand = dir.join(format!("{stem} ({suffix}).{ext}"));
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({suffix} {n}).{ext}"));
        n += 1;
    }
    cand
}

/// Encode `img` to `path` as `format`, honoring the user's saved JPEG quality /
/// PNG compression settings (Options). WebP stays lossless (the quick verbs have
/// no quality knob).
fn encode_to(img: &DynamicImage, format: ImageFormat, target_ext: &str, path: &Path) -> Result<()> {
    encode_to_opts(
        img,
        format,
        crate::settings::jpeg_quality(),
        crate::settings::png_level(),
        None,
        target_ext,
        path,
    )
}

#[inline]
fn u8_to_f32(value: u8) -> f32 {
    value as f32 / u8::MAX as f32
}

#[inline]
fn u16_to_f32(value: u16) -> f32 {
    value as f32 / u16::MAX as f32
}

#[inline]
fn u8_to_u16(value: u8) -> u16 {
    u16::from(value) * 257
}

#[inline]
fn u16_to_u8(value: u16) -> u8 {
    ((u32::from(value) + 128) / 257) as u8
}

#[inline]
fn f32_to_u8(value: f32) -> u8 {
    let normalized = if value.is_nan() || value >= 1.0 {
        1.0
    } else {
        value.max(0.0)
    };
    (normalized * u8::MAX as f32).round() as u8
}

#[inline]
fn f32_to_u16(value: f32) -> u16 {
    let normalized = if value.is_nan() || value >= 1.0 {
        1.0
    } else {
        value.max(0.0)
    };
    (normalized * u16::MAX as f32).round() as u16
}

/// Sample one pixel without materializing a converted full-frame image.
///
/// Matching the concrete variants is load-bearing for HDR inputs:
/// `DynamicImage::get_pixel` returns RGBA8 and would clamp an RGB32F/RGBA32F
/// source to SDR before EXR/HDR encoding.
#[inline]
fn rgba_f32_at(img: &DynamicImage, x: u32, y: u32) -> [f32; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u8_to_f32(l);
            [l, l, l, 1.0]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u8_to_f32(l);
            [l, l, l, u8_to_f32(a)]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), 1.0]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), u8_to_f32(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u16_to_f32(l);
            [l, l, l, 1.0]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u16_to_f32(l);
            [l, l, l, u16_to_f32(a)]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u16_to_f32(r), u16_to_f32(g), u16_to_f32(b), 1.0]
        }
        DynamicImage::ImageRgba16(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u16_to_f32(r), u16_to_f32(g), u16_to_f32(b), u16_to_f32(a)]
        }
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, 1.0]
        }
        DynamicImage::ImageRgba32F(pixels) => pixels.get_pixel(x, y).0,
        _ => {
            let [r, g, b, a] = image::GenericImageView::get_pixel(img, x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), u8_to_f32(a)]
        }
    }
}

#[inline]
fn rgba_u16_at(img: &DynamicImage, x: u32, y: u32) -> [u16; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u8_to_u16(l);
            [l, l, l, u16::MAX]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u8_to_u16(l);
            [l, l, l, u8_to_u16(a)]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u16::MAX]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, l, l, u16::MAX]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, l, l, a]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, u16::MAX]
        }
        DynamicImage::ImageRgba16(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), u16::MAX]
        }
        DynamicImage::ImageRgba32F(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), f32_to_u16(a)]
        }
        _ => {
            let [r, g, b, a] = image::GenericImageView::get_pixel(img, x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
    }
}

#[inline]
fn rgba_u8_at(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, l, l, u8::MAX]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, l, l, a]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, u8::MAX]
        }
        DynamicImage::ImageRgba8(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u16_to_u8(l);
            [l, l, l, u8::MAX]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u16_to_u8(l);
            [l, l, l, u16_to_u8(a)]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u16_to_u8(r), u16_to_u8(g), u16_to_u8(b), u8::MAX]
        }
        DynamicImage::ImageRgba16(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u16_to_u8(r), u16_to_u8(g), u16_to_u8(b), u16_to_u8(a)]
        }
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u8(r), f32_to_u8(g), f32_to_u8(b), u8::MAX]
        }
        DynamicImage::ImageRgba32F(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [f32_to_u8(r), f32_to_u8(g), f32_to_u8(b), f32_to_u8(a)]
        }
        _ => image::GenericImageView::get_pixel(img, x, y).0,
    }
}

fn encode_exr_bounded<W: Write + Seek>(
    writer: &mut W,
    img: &DynamicImage,
) -> exr::error::UnitResult {
    use exr::prelude::{Encoding, Image, SpecificChannels, Vec2, WritableImage};

    let channels = SpecificChannels::rgba(|position: Vec2<usize>| {
        let [r, g, b, a] = rgba_f32_at(img, position.x() as u32, position.y() as u32);
        (r, g, b, a)
    });
    let image = Image::from_encoded_channels(
        (img.width() as usize, img.height() as usize),
        Encoding::SMALL_FAST_LOSSLESS,
        channels,
    );

    // SMALL_FAST_LOSSLESS is PIZ over 256x256 tiles. Coupled with the sequential
    // writer, memory is bounded to one f32 compression tile instead of a second
    // width*height RGBA32F frame (or one tile per Rayon worker). Keeping f32
    // channels also preserves the source's full precision and values > f16::MAX.
    image.write().non_parallel().to_buffered(writer)
}

#[inline]
fn float_rgb_to_rgbe([r, g, b]: [f32; 3]) -> [u8; 4] {
    // Largest finite value representable by normalized RGBE: the greatest f32
    // below 2^127. Larger finite values and +Inf saturate here; NaN, negatives,
    // and -Inf carry no radiance and become zero.
    const RGBE_MAX: f32 = f32::from_bits(0x7E_FF_FF_FF);
    let sanitize = |value: f32| {
        if value.is_nan() || value <= 0.0 {
            0.0
        } else if !value.is_finite() || value > RGBE_MAX {
            RGBE_MAX
        } else {
            value
        }
    };
    let [r, g, b] = [sanitize(r), sanitize(g), sanitize(b)];
    let maximum = r.max(g).max(b);
    if maximum <= 0.0 {
        return [0; 4];
    }

    // This intentionally matches image's Radiance encoder conversion.
    let exponent = maximum.log2().floor() as i32 + 1;
    // Exponent byte 0 denotes black. Values below the smallest normalized
    // Radiance value therefore underflow cleanly instead of wrapping a negative
    // exponent through `as u8`.
    if exponent < -127 {
        return [0; 4];
    }
    let exponent = exponent.clamp(-127, 127);
    let scale = 2.0_f32.powi(exponent);
    [
        (r / scale * 256.0).trunc() as u8,
        (g / scale * 256.0).trunc() as u8,
        (b / scale * 256.0).trunc() as u8,
        (exponent + 128) as u8,
    ]
}

#[inline]
fn rgbe_at(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    let [r, g, b, _] = rgba_f32_at(img, x, y);
    float_rgb_to_rgbe([r, g, b])
}

#[inline]
fn escape_raw_rgbe_marker(mut pixel: [u8; 4], first_in_scanline: bool) -> [u8; 4] {
    // The legacy/raw decoder does not have an escape byte. Any literal
    // [1,1,1,E] is interpreted as "repeat the previous pixel E times" (and is
    // illegal as the first pixel), so perturb one 8-bit mantissa by one quantum.
    if pixel[..3] == [1, 1, 1] {
        pixel[2] = 2;
    }
    // The first pixel also selects the codec. [2,2,B<128,E] means new
    // per-component RLE, not a literal pixel.
    if first_in_scanline && pixel[0] == 2 && pixel[1] == 2 && pixel[2] < 128 {
        pixel[1] = 3;
    }
    pixel
}

fn write_hdr_component_rle<W: Write>(
    writer: &mut W,
    scanline: &[[u8; 4]],
    component: usize,
) -> std::io::Result<()> {
    const MAX_RUN: usize = 127;
    const MAX_LITERAL: usize = 128;

    let mut index = 0;
    let mut literal = [0u8; MAX_LITERAL];
    while index < scanline.len() {
        let value = scanline[index][component];
        let run = scanline[index..]
            .iter()
            .take(MAX_RUN)
            .take_while(|pixel| pixel[component] == value)
            .count();
        if run >= 3 {
            writer.write_all(&[128 + run as u8, value])?;
            index += run;
            continue;
        }

        let mut literal_len = 0;
        while index < scanline.len() && literal_len < MAX_LITERAL {
            let value = scanline[index][component];
            let run = scanline[index..]
                .iter()
                .take(MAX_RUN)
                .take_while(|pixel| pixel[component] == value)
                .count();
            if run >= 3 {
                break;
            }

            let take = run.min(MAX_LITERAL - literal_len);
            for pixel in &scanline[index..index + take] {
                literal[literal_len] = pixel[component];
                literal_len += 1;
            }
            index += take;
        }

        debug_assert!(literal_len > 0);
        writer.write_all(&[literal_len as u8])?;
        writer.write_all(&literal[..literal_len])?;
    }
    Ok(())
}

fn encode_hdr_bounded<W: Write>(writer: &mut W, img: &DynamicImage) -> std::io::Result<()> {
    let width = img.width() as usize;
    let height = img.height() as usize;
    writer.write_all(b"#?RADIANCE\n")?;
    writer.write_all(b"# Rust HDR encoder\n")?;
    writer.write_all(b"FORMAT=32-bit_rle_rgbe\n\n")?;
    writeln!(writer, "-Y {height} +X {width}")?;

    if !(8..=32_767).contains(&width) {
        // Radiance's new component-RLE marker cannot represent these widths.
        // Old readers accept a raw row-major RGBE stream after the same header.
        for y in 0..img.height() {
            for x in 0..img.width() {
                let pixel = escape_raw_rgbe_marker(rgbe_at(img, x, y), x == 0);
                writer.write_all(&pixel)?;
            }
        }
        return Ok(());
    }

    // The new RLE format stores one scanline, then compresses its R/G/B/E
    // components separately. Its format-level width ceiling makes this at most
    // 128 KiB regardless of the image height.
    let mut scanline = vec![[0u8; 4]; width];
    let marker = [2, 2, (width / 256) as u8, (width % 256) as u8];
    for y in 0..img.height() {
        for (x, pixel) in scanline.iter_mut().enumerate() {
            *pixel = rgbe_at(img, x as u32, y);
        }
        writer.write_all(&marker)?;
        for component in 0..4 {
            write_hdr_component_rle(writer, &scanline, component)?;
        }
    }
    Ok(())
}

fn encode_farbfeld_streaming<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
) -> std::io::Result<()> {
    writer.write_all(b"farbfeld")?;
    writer.write_all(&img.width().to_be_bytes())?;
    writer.write_all(&img.height().to_be_bytes())?;
    for y in 0..img.height() {
        for x in 0..img.width() {
            let channels = rgba_u16_at(img, x, y);
            let mut bytes = [0u8; 8];
            for (slot, channel) in bytes.chunks_exact_mut(2).zip(channels) {
                slot.copy_from_slice(&channel.to_be_bytes());
            }
            writer.write_all(&bytes)?;
        }
    }
    Ok(())
}

fn pam_layout(img: &DynamicImage) -> (usize, &'static str, bool) {
    match img {
        DynamicImage::ImageLuma8(_) => (1, "GRAYSCALE", false),
        DynamicImage::ImageLumaA8(_) => (2, "GRAYSCALE_ALPHA", false),
        DynamicImage::ImageRgb8(_) => (3, "RGB", false),
        DynamicImage::ImageRgba8(_) => (4, "RGB_ALPHA", false),
        DynamicImage::ImageLuma16(_) => (1, "GRAYSCALE", true),
        DynamicImage::ImageLumaA16(_) => (2, "GRAYSCALE_ALPHA", true),
        DynamicImage::ImageRgb16(_) => (3, "RGB", true),
        DynamicImage::ImageRgba16(_) => (4, "RGB_ALPHA", true),
        // PAM has no floating-point sample representation. Preserve the channel
        // model and map floats to clamped unsigned 16-bit samples.
        DynamicImage::ImageRgb32F(_) => (3, "RGB", true),
        DynamicImage::ImageRgba32F(_) => (4, "RGB_ALPHA", true),
        #[allow(unreachable_patterns)]
        _ => (4, "RGB_ALPHA", false),
    }
}

#[inline]
fn pam_u16_at(img: &DynamicImage, x: u32, y: u32) -> [u16; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [u8_to_u16(l), 0, 0, 0]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(l), u8_to_u16(a), 0, 0]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), 0]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, 0, 0, 0]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, a, 0, 0]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, 0]
        }
        DynamicImage::ImageRgba16(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), 0]
        }
        DynamicImage::ImageRgba32F(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), f32_to_u16(a)]
        }
        _ => {
            let [r, g, b, a] = image::GenericImageView::get_pixel(img, x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
    }
}

fn encode_pam_streaming<W: Write>(writer: &mut W, img: &DynamicImage) -> std::io::Result<()> {
    let (depth, tuple_type, wide) = pam_layout(img);
    writeln!(writer, "P7")?;
    writeln!(writer, "WIDTH {}", img.width())?;
    writeln!(writer, "HEIGHT {}", img.height())?;
    writeln!(writer, "DEPTH {depth}")?;
    writeln!(writer, "MAXVAL {}", if wide { 65_535 } else { 255 })?;
    writeln!(writer, "TUPLTYPE {tuple_type}")?;
    writeln!(writer, "ENDHDR")?;
    for y in 0..img.height() {
        for x in 0..img.width() {
            let samples = pam_u16_at(img, x, y);
            for sample in &samples[..depth] {
                if wide {
                    writer.write_all(&sample.to_be_bytes())?;
                } else {
                    writer.write_all(&[u16_to_u8(*sample)])?;
                }
            }
        }
    }
    Ok(())
}

fn encode_ppm_streaming<W: Write>(writer: &mut W, img: &DynamicImage) -> std::io::Result<()> {
    // PPM is always RGB and therefore drops alpha, but it can retain 16-bit
    // integer precision. Float inputs are clamped into that same 0..65535 range.
    let wide = matches!(
        img,
        DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
            | DynamicImage::ImageRgb32F(_)
            | DynamicImage::ImageRgba32F(_)
    );
    writeln!(writer, "P6")?;
    writeln!(writer, "{} {}", img.width(), img.height())?;
    writeln!(writer, "{}", if wide { 65_535 } else { 255 })?;
    for y in 0..img.height() {
        for x in 0..img.width() {
            if wide {
                let [r, g, b, _] = rgba_u16_at(img, x, y);
                writer.write_all(&r.to_be_bytes())?;
                writer.write_all(&g.to_be_bytes())?;
                writer.write_all(&b.to_be_bytes())?;
            } else {
                let [r, g, b, _] = rgba_u8_at(img, x, y);
                writer.write_all(&[r, g, b])?;
            }
        }
    }
    Ok(())
}

/// Encode with EXPLICIT JPEG quality / PNG level (the Convert… dialog passes its
/// slider values; the verbs pass the saved settings). `webp_quality = Some(q)`
/// selects lossy WebP (libwebp) at quality `q`; `None` keeps WebP lossless (the
/// pure-Rust image encoder). ICO is capped to 256px.
fn encode_to_opts(
    img: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    target_ext: &str,
    path: &Path,
) -> Result<()> {
    // Only the (optional) lossy-WebP arm consults this; without that feature, WebP
    // is encoded losslessly via `image` and the quality is irrelevant.
    #[cfg(not(feature = "webp-lossy"))]
    let _ = webp_quality;
    let file = std::fs::File::create(path).map_err(|_| Error::from(E_FAIL))?;
    let mut w = std::io::BufWriter::new(file);
    // ICO frames are at most 256×256; downscale (preserving aspect) to fit.
    let resized;
    let img = if matches!(format, ImageFormat::Ico) && (img.width() > 256 || img.height() > 256) {
        resized = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
        &resized
    } else {
        img
    };
    let res = match format {
        ImageFormat::Jpeg => img
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, jpeg_quality))
            .map_err(|_| Error::from(E_FAIL)),
        // Lossy WebP via libwebp (image-webp only encodes lossless). Smaller
        // files for photos; alpha is preserved. Optional: without `webp-lossy`,
        // WebP falls through to the lossless `other` arm (the `image` encoder).
        #[cfg(feature = "webp-lossy")]
        ImageFormat::WebP if webp_quality.is_some() => {
            // libwebp rejects edges > 16383. `encode()` looks infallible but
            // .unwrap()s internally, and the worker thread has no catch_unwind
            // (panic=abort) — so an oversized image would abort the whole batch.
            // Fail this one file cleanly instead.
            let (pw, ph) = (img.width(), img.height());
            if pw == 0 || ph == 0 || pw > 16383 || ph > 16383 {
                return Err(Error::from(E_FAIL));
            }
            let rgba = img.to_rgba8();
            let mem = webp::Encoder::from_rgba(rgba.as_raw(), pw, ph)
                .encode(webp_quality.unwrap().clamp(1, 100) as f32);
            w.write_all(&mem).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Png => {
            // image's PNG encoder takes a coarse Fast/Default/Best level, not
            // the legacy 0–9 zlib scale, so map onto it.
            let ct = match png_level {
                0..=2 => image::codecs::png::CompressionType::Fast,
                7..=9 => image::codecs::png::CompressionType::Best,
                _ => image::codecs::png::CompressionType::Default,
            };
            img.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
                &mut w,
                ct,
                image::codecs::png::FilterType::Adaptive,
            ))
            .map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::OpenExr => {
            encode_exr_bounded(&mut w, img).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Hdr => {
            encode_hdr_bounded(&mut w, img).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Farbfeld => {
            encode_farbfeld_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Pnm => {
            let is_pam = target_ext.eq_ignore_ascii_case("pam");
            let is_ppm = target_ext.eq_ignore_ascii_case("ppm");
            if is_pam {
                encode_pam_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
            } else if is_ppm {
                encode_ppm_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
            } else {
                // Preserve the prior dynamic behavior for PBM/PGM/general-PNM
                // transforms, whose subtype depends on their pixel type.
                img.write_to(&mut w, ImageFormat::Pnm)
                    .map_err(|_| Error::from(E_FAIL))
            }
        }
        other => img.write_to(&mut w, other).map_err(|_| Error::from(E_FAIL)),
    };
    res?;
    // Flush the buffered tail explicitly: BufWriter::drop discards flush errors,
    // so a disk-full on the final block would otherwise let the caller rename a
    // TRUNCATED temp file over the destination (breaking the atomic-write promise).
    w.flush().map_err(|_| Error::from(E_FAIL))?;
    Ok(())
}

/// Resize applied by the Convert… dialog.
#[derive(Clone, Copy)]
pub enum Resize {
    None,
    /// Fit within `w`×`h` preserving aspect; never upscales (the menu presets —
    /// "Fit 1920×1080" means shrink-to-fit, not blow up a small image).
    Fit(u32, u32),
    /// Scale to fit `w`×`h` preserving aspect, UP or down — the Convert dialog's
    /// explicit "Defined size": typing dimensions bigger than the source means
    /// "make it bigger" (user feedback).
    FitUp(u32, u32),
    /// Scale by `0`% (1..=1000).
    Percent(u32),
}

/// Convert options chosen in the Convert… dialog.
#[derive(Clone, Copy)]
pub struct ConvertOpts {
    pub target: Target,
    pub jpeg_quality: u8,
    pub png_level: u32,
    /// `Some(q)` = lossy WebP at quality q; `None` = lossless WebP (ignored for
    /// non-WebP formats).
    pub webp_quality: Option<u8>,
    pub resize: Resize,
}

pub(crate) fn apply_resize(img: DynamicImage, r: Resize) -> DynamicImage {
    match r {
        Resize::None => img,
        Resize::Fit(w, h) if img.width() > w || img.height() > h => {
            img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3)
        }
        Resize::Fit(..) => img,
        // `image::resize` scales in BOTH directions (aspect preserved), which is
        // exactly the explicit-dimensions contract.
        Resize::FitUp(w, h) => img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3),
        Resize::Percent(p) => {
            let s = p.clamp(1, 1000) as f64 / 100.0;
            let w = ((img.width() as f64 * s).round() as u32).max(1);
            let h = ((img.height() as f64 * s).round() as u32).max(1);
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        }
    }
}

/// Convert `path` into `out_dir` per `opts` (the Convert… dialog path). Picks a
/// non-colliding name, writes atomically. Returns the output path.
pub fn convert_file_opts(path: &str, opts: ConvertOpts, out_dir: &Path) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, opts.resize);
    if matches!(opts.target.format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let ext = opts.target.ext.to_string();
    let dir = out_dir.to_path_buf();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        dir.join(name)
    });
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            opts.target.format,
            opts.jpeg_quality,
            opts.png_level,
            opts.webp_quality,
            opts.target.ext,
            tmp,
        )
    })?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Convert `input` to the EXACT `out` path (format inferred from its extension),
/// at `quality`, with `resize`. Used by the `st2k` CLI where the caller names the
/// output file. `webp_quality = Some(q)` selects lossy WebP at quality `q` (the
/// menu's quick WebP verb routes here with `Some(80)` when the `st2k.exe` helper
/// runs the conversion out-of-process); `None` keeps WebP lossless. PNG output uses
/// the saved `settings::png_level()` (default 9) — the SAME level the in-process
/// `convert_file` uses, so a helper-routed PNG convert is byte-identical to the
/// in-process one (it used to hard-code level 6 here, diverging whenever the user's
/// PNG setting wasn't 6).
pub fn convert_to(input: &str, out: &Path, quality: u8, webp_quality: Option<u8>, resize: Resize) -> Result<()> {
    let ext = out
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| Error::from(E_FAIL))?
        .to_ascii_lowercase();
    // Route every explicitly supported Magick target through its named coder.
    if ext_needs_magick(&ext) {
        // None = magick's default quality, so the quick verb's out-of-process (`st2k convert`)
        // path stays byte-identical to its in-process twin. The Convert… dialog uses
        // `convert_to_magick_in` with an explicit quality instead.
        return convert_to_magick(input, out, resize, None);
    }
    // Validate the requested writer before touching the input. Besides avoiding
    // wasted decode work, this guarantees an unknown suffix fails even when the
    // input path is missing or hostile.
    let format = native_output_format(&ext).ok_or_else(|| Error::from(E_FAIL))?;
    let bytes = read_capped(input)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, resize);
    if matches!(format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    write_atomic(out, |tmp| {
        encode_to_opts(
            &img,
            format,
            quality,
            crate::settings::png_level(),
            webp_quality,
            &ext,
            tmp,
        )
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` to the EXACT `out` path via the bundled ImageMagick — for the
/// exotic Convert targets the `image` crate can't encode (PSD/DDS/JP2/…).
/// Decodes with OUR pipeline (so every input format works), applies `resize`, then
/// hands magick a PNG to write `out` through an explicit, allowlisted coder.
pub fn convert_to_magick(input: &str, out: &Path, resize: Resize, quality: Option<u8>) -> Result<()> {
    let target_ext = out
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| Error::from(E_FAIL))?;
    if !decode::magick_output_supported(target_ext) {
        return Err(Error::from(E_FAIL));
    }
    let bytes = read_capped(input)?;
    let img = apply_resize(decode::decode_full(&bytes)?, resize);
    write_atomic(out, |tmp| {
        decode::encode_via_magick(&img, tmp, target_ext, quality)
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` into `out_dir` via the bundled ImageMagick at extension `ext`,
/// picking a collision-free reserved name (race-safe under parallel batches).
/// Wraps [`convert_to_magick`] so the Convert… dialog's exotic targets carry no
/// naming logic. Returns the output path.
pub fn convert_to_magick_in(
    input: &str,
    out_dir: &Path,
    ext: &str,
    resize: Resize,
    quality: Option<u8>,
) -> Result<PathBuf> {
    if !decode::magick_output_supported(ext) {
        return Err(Error::from(E_FAIL));
    }
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = out_dir.to_path_buf();
    let e = ext.to_string();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{e}") } else { format!("{stem} ({n}).{e}") };
        dir.join(name)
    });
    convert_to_magick(input, slot.path(), resize, quality)?;
    Ok(slot.path().to_path_buf())
}

/// One image → a single-page PDF in `out_dir` (collision-free reserved name).
/// Wraps [`crate::topdf::combine_to_pdf`] so the Convert… dialog's PDF target
/// carries no naming logic. Returns the output path.
pub fn convert_image_to_pdf_in(input: &str, out_dir: &Path, quality: u8) -> Result<PathBuf> {
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = out_dir.to_path_buf();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.pdf") } else { format!("{stem} ({n}).pdf") };
        dir.join(name)
    });
    let one = [input.to_string()];
    crate::topdf::combine_to_pdf(&one, slot.path(), quality)?;
    preserve_src_time(Path::new(input), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Decode `path`, cap its longest edge to the preset, and write a small
/// "(email)" JPEG sibling (flattened onto white — JPEG has no alpha). Never
/// upscales; never touches the original. Returns the output path.
pub fn shrink_for_email(path: &str, size: EmailSize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let edge = size.max_edge();
    let img = flatten_onto_white(&apply_resize(decode::decode_full(&bytes)?, Resize::Fit(edge, edge)));
    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "email", "jpg");
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            ImageFormat::Jpeg,
            EMAIL_JPEG_QUALITY,
            6,
            None,
            "jpg",
            tmp,
        )
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// JPEG quality search bounds for [`compress_to_size`].
const COMPRESS_Q_MIN: u8 = 20;
const COMPRESS_Q_MAX: u8 = 95;

/// Encode `img` to in-memory JPEG bytes at `quality` — the probe the size search uses.
fn jpeg_bytes(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality))
        .map_err(|_| Error::from(E_FAIL))?;
    Ok(buf)
}

/// Highest-quality JPEG of `img` at or under `target` bytes (binary search on quality),
/// or `None` if even [`COMPRESS_Q_MIN`] overshoots — then the caller downscales + retries.
fn jpeg_under(img: &DynamicImage, target: u64) -> Result<Option<Vec<u8>>> {
    let floor = jpeg_bytes(img, COMPRESS_Q_MIN)?;
    if floor.len() as u64 > target {
        return Ok(None);
    }
    let (mut lo, mut hi) = (COMPRESS_Q_MIN, COMPRESS_Q_MAX);
    let mut best = floor;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let b = jpeg_bytes(img, mid)?;
        if b.len() as u64 <= target {
            best = b;
            lo = mid + 1; // fits — try higher quality
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    Ok(Some(best))
}

/// Compress `path` into a JPEG at or under `target_bytes`, by binary-searching JPEG
/// quality and — if even the lowest quality overshoots — progressively downscaling (20%
/// a step, down to a ~32px floor). The "(compressed)" sibling never upscales and never
/// overwrites the original. With an unreasonably tiny target it ships the smallest it can
/// make (which may slightly exceed it). Reusable by the CLI and a future menu/dialog.
pub fn compress_to_size(path: &str, target_bytes: u64) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    // JPEG has no alpha → flatten transparency onto white, like shrink-for-email.
    let mut img = flatten_onto_white(&decode::decode_full(&bytes)?);
    let target = target_bytes.max(1);

    let mut chosen = None;
    for _ in 0..8 {
        if let Some(b) = jpeg_under(&img, target)? {
            chosen = Some(b);
            break;
        }
        let (w, h) = (img.width(), img.height());
        if w.min(h) <= 32 {
            break; // already tiny — stop shrinking
        }
        img = img.resize(
            (w * 4 / 5).max(1),
            (h * 4 / 5).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }
    let data = match chosen {
        Some(b) => b,
        None => jpeg_bytes(&img, COMPRESS_Q_MIN)?, // best-effort floor
    };

    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "compressed", "jpg");
    write_atomic(slot.path(), |tmp| {
        std::fs::write(tmp, &data).map_err(|_| Error::from(E_FAIL))
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

#[cfg(test)]
mod bounded_native_encoder_tests {
    use super::*;
    use exr::prelude::{Compression, MetaData, SampleType, Vec2};
    use image::GenericImageView;
    use std::io::Cursor;

    fn hdr_fixture(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba32F(image::Rgba32FImage::from_fn(width, height, |x, y| {
            if x < width / 2 {
                image::Rgba([100_000.0, 2.0, 0.5, 0.25])
            } else {
                image::Rgba([
                    1.0 + x as f32 / width as f32,
                    0.25 + y as f32 / height as f32,
                    4.0,
                    0.75,
                ])
            }
        }))
    }

    #[test]
    fn bounded_native_encoders_have_valid_headers_roundtrip_and_sizes() {
        let img = hdr_fixture(64, 32);
        let pixel_count = u64::from(img.width()) * u64::from(img.height());

        let mut exr = Cursor::new(Vec::new());
        encode_exr_bounded(&mut exr, &img).unwrap();
        let exr = exr.into_inner();
        assert!(exr.starts_with(&[0x76, 0x2f, 0x31, 0x01]));
        assert!(
            exr.len() < (pixel_count * 16) as usize,
            "constant-heavy tiled f32 EXR should compress below its raw f32 pixels"
        );
        let metadata = MetaData::read_from_buffered(Cursor::new(&exr), true).unwrap();
        let header = &metadata.headers[0];
        assert_eq!(header.compression, Compression::PIZ);
        assert!(
            header
                .channels
                .list
                .iter()
                .all(|channel| channel.sample_type == SampleType::F32),
            "all EXR output channels must be f32"
        );
        match header.blocks {
            exr::meta::BlockDescription::Tiles(description) => {
                assert_eq!(description.tile_size, Vec2(256, 256));
            }
            _ => panic!("EXR output must use bounded tiles"),
        }
        let decoded_exr =
            image::load_from_memory_with_format(&exr, ImageFormat::OpenExr).unwrap();
        let decoded_exr = decoded_exr.to_rgba32f();
        let exr_pixel = decoded_exr.get_pixel(0, 0).0;
        assert!(
            (exr_pixel[0] - 100_000.0).abs() < 1.0,
            "f32 EXR value above f16::MAX was clipped"
        );
        assert!((exr_pixel[3] - 0.25).abs() < 0.01, "EXR alpha changed");

        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let hdr_header = format!(
            "#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y {} +X {}\n",
            img.height(),
            img.width()
        );
        assert!(hdr.starts_with(hdr_header.as_bytes()));
        assert_eq!(
            &hdr[hdr_header.len()..hdr_header.len() + 4],
            &[2, 2, 0, 64],
            "new Radiance per-component RLE marker is missing"
        );
        assert!(
            hdr.len() < hdr_header.len() + (pixel_count * 4) as usize,
            "constant-heavy HDR should be smaller than raw RGBE"
        );
        let decoded_hdr = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
        let decoded_hdr = decoded_hdr.to_rgb32f();
        assert!(
            decoded_hdr.get_pixel(0, 0).0[0] > 99_000.0,
            "float HDR range was clipped before RGBE encoding"
        );

        let mut farbfeld = Vec::new();
        encode_farbfeld_streaming(&mut farbfeld, &img).unwrap();
        assert!(farbfeld.starts_with(b"farbfeld"));
        assert_eq!(farbfeld.len(), 16 + (pixel_count * 8) as usize);
        let decoded_farbfeld =
            image::load_from_memory_with_format(&farbfeld, ImageFormat::Farbfeld).unwrap();
        assert_eq!(decoded_farbfeld.dimensions(), img.dimensions());

        let mut pam = Vec::new();
        encode_pam_streaming(&mut pam, &img).unwrap();
        let pam_header_end = pam
            .windows(b"ENDHDR\n".len())
            .position(|window| window == b"ENDHDR\n")
            .map(|index| index + b"ENDHDR\n".len())
            .unwrap();
        assert!(pam.starts_with(b"P7\n"));
        assert!(pam[..pam_header_end]
            .windows(b"MAXVAL 65535".len())
            .any(|window| window == b"MAXVAL 65535"));
        assert_eq!(pam.len(), pam_header_end + (pixel_count * 8) as usize);
        let decoded_pam = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
        assert_eq!(decoded_pam.dimensions(), img.dimensions());
        assert_eq!(decoded_pam.to_rgba16().get_pixel(0, 0).0[3], 16_384);

        let mut ppm = Vec::new();
        encode_ppm_streaming(&mut ppm, &img).unwrap();
        let ppm_header = format!("P6\n{} {}\n65535\n", img.width(), img.height());
        assert!(ppm.starts_with(ppm_header.as_bytes()));
        assert_eq!(ppm.len(), ppm_header.len() + (pixel_count * 6) as usize);
        let decoded_ppm = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm).unwrap();
        assert_eq!(decoded_ppm.dimensions(), img.dimensions());
    }

    #[test]
    fn hdr_short_scanlines_use_raw_compatible_fallback() {
        let img = hdr_fixture(7, 2);
        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let header = b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 7\n";
        assert!(hdr.starts_with(header));
        assert_eq!(hdr.len(), header.len() + 7 * 2 * 4);
        assert_ne!(&hdr[header.len()..header.len() + 4], &[2, 2, 0, 7]);
        let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
        assert_eq!(decoded.dimensions(), (7, 2));
    }

    #[test]
    fn pam_and_ppm_preserve_16_bit_samples_and_pam_channel_models() {
        let fixtures = [
            (
                DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Luma([0x1234]),
                )),
                1usize,
                "GRAYSCALE",
                vec![0x12, 0x34],
            ),
            (
                DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::LumaA([0x1234, 0xABCD]),
                )),
                2,
                "GRAYSCALE_ALPHA",
                vec![0x12, 0x34, 0xAB, 0xCD],
            ),
            (
                DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Rgb([0x1234, 0x5678, 0x9ABC]),
                )),
                3,
                "RGB",
                vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
            ),
            (
                DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
                )),
                4,
                "RGB_ALPHA",
                vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
            ),
        ];

        for (img, depth, tuple_type, expected_body) in fixtures {
            let mut pam = Vec::new();
            encode_pam_streaming(&mut pam, &img).unwrap();
            let header =
                format!("P7\nWIDTH 1\nHEIGHT 1\nDEPTH {depth}\nMAXVAL 65535\nTUPLTYPE {tuple_type}\nENDHDR\n");
            assert!(pam.starts_with(header.as_bytes()));
            assert_eq!(&pam[header.len()..], expected_body);
            let decoded = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
            assert_eq!(decoded.dimensions(), (1, 1));
        }

        let rgba = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
        ));
        let mut ppm = Vec::new();
        encode_ppm_streaming(&mut ppm, &rgba).unwrap();
        let header = b"P6\n1 1\n65535\n";
        assert!(ppm.starts_with(header));
        assert_eq!(&ppm[header.len()..], &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        let decoded = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm)
            .unwrap()
            .to_rgb16();
        assert_eq!(decoded.get_pixel(0, 0).0, [0x1234, 0x5678, 0x9ABC]);
    }

    #[test]
    fn hdr_rle_width_boundary_and_raw_marker_escape_are_valid() {
        let rle_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32_767,
            1,
            image::Rgb([64, 32, 16]),
        ));
        let mut rle = Vec::new();
        encode_hdr_bounded(&mut rle, &rle_img).unwrap();
        let rle_header =
            b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32767\n";
        assert_eq!(&rle[rle_header.len()..rle_header.len() + 4], &[2, 2, 127, 255]);
        assert_eq!(
            image::load_from_memory_with_format(&rle, ImageFormat::Hdr)
                .unwrap()
                .dimensions(),
            (32_767, 1)
        );

        let raw_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32_768,
            1,
            image::Rgb([64, 32, 16]),
        ));
        let mut raw = Vec::new();
        encode_hdr_bounded(&mut raw, &raw_img).unwrap();
        let raw_header =
            b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32768\n";
        assert_eq!(raw.len(), raw_header.len() + 32_768 * 4);
        assert_ne!(&raw[raw_header.len()..raw_header.len() + 4], &[2, 2, 128, 0]);
        assert_eq!(
            image::load_from_memory_with_format(&raw, ImageFormat::Hdr)
                .unwrap()
                .dimensions(),
            (32_768, 1)
        );

        assert_eq!(escape_raw_rgbe_marker([1, 1, 1, 42], true), [1, 1, 2, 42]);
        assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], true), [2, 3, 7, 99]);
        assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], false), [2, 2, 7, 99]);
    }

    #[test]
    fn float_to_integer_samples_preserve_saturation_semantics() {
        assert_eq!(f32_to_u8(f32::NAN), u8::MAX);
        assert_eq!(f32_to_u8(f32::INFINITY), u8::MAX);
        assert_eq!(f32_to_u8(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_u8(-0.25), 0);
        assert_eq!(f32_to_u8(0.5), 128);
        assert_eq!(f32_to_u8(1.25), u8::MAX);

        assert_eq!(f32_to_u16(f32::NAN), u16::MAX);
        assert_eq!(f32_to_u16(f32::INFINITY), u16::MAX);
        assert_eq!(f32_to_u16(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_u16(-0.25), 0);
        assert_eq!(f32_to_u16(0.5), 32_768);
        assert_eq!(f32_to_u16(1.25), u16::MAX);
    }

    #[test]
    fn hdr_non_finite_and_out_of_range_samples_saturate_safely() {
        assert_eq!(
            float_rgb_to_rgbe([f32::NAN, f32::NEG_INFINITY, -1.0]),
            [0, 0, 0, 0]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::INFINITY, 0.0, 0.0]),
            [255, 0, 0, 255]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::MAX, f32::MAX, f32::MAX]),
            [255, 255, 255, 255]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::from_bits(1), 0.0, 0.0]),
            [0, 0, 0, 0],
            "unrepresentable subnormal radiance should underflow to black"
        );

        let img = DynamicImage::ImageRgb32F(
            image::Rgb32FImage::from_raw(
                5,
                1,
                vec![
                    f32::NAN,
                    f32::NEG_INFINITY,
                    -1.0,
                    f32::INFINITY,
                    0.0,
                    0.0,
                    f32::MAX,
                    f32::MAX,
                    f32::MAX,
                    1.0,
                    0.5,
                    0.25,
                    f32::from_bits(1),
                    0.0,
                    0.0,
                ],
            )
            .unwrap(),
        );
        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr)
            .unwrap()
            .to_rgb32f();
        assert_eq!(decoded.dimensions(), (5, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [0.0, 0.0, 0.0]);
        assert_eq!(decoded.get_pixel(4, 0).0, [0.0, 0.0, 0.0]);
        assert!(
            decoded
                .pixels()
                .flat_map(|pixel| pixel.0)
                .all(|component| component.is_finite() && component >= 0.0)
        );
        assert!(decoded.get_pixel(1, 0).0[0] > 1.0e38);
        assert!(decoded.get_pixel(2, 0).0[0] > 1.0e38);
    }

    #[test]
    fn output_extension_routing_is_explicit_and_honest() {
        for ext in [
            "avif", "jxl", "psd", "dds", "jp2", "pcx", "sgi", "pfm", "dpx", "fits", "xpm",
            "pict", "ras", "palm",
        ] {
            assert!(ext_needs_magick(ext), "{ext} must route through Magick");
            assert_eq!(edit_output_ext(ext), ext);
        }
        for (ext, format) in [
            ("png", ImageFormat::Png),
            ("jpg", ImageFormat::Jpeg),
            ("jpeg", ImageFormat::Jpeg),
            ("jpe", ImageFormat::Jpeg),
            ("jfif", ImageFormat::Jpeg),
            ("gif", ImageFormat::Gif),
            ("webp", ImageFormat::WebP),
            ("pam", ImageFormat::Pnm),
            ("ppm", ImageFormat::Pnm),
            ("pnm", ImageFormat::Pnm),
            ("tiff", ImageFormat::Tiff),
            ("tif", ImageFormat::Tiff),
            ("tga", ImageFormat::Tga),
            ("bmp", ImageFormat::Bmp),
            ("ico", ImageFormat::Ico),
            ("hdr", ImageFormat::Hdr),
            ("exr", ImageFormat::OpenExr),
            ("ff", ImageFormat::Farbfeld),
            ("qoi", ImageFormat::Qoi),
        ] {
            assert_eq!(native_output_format(ext), Some(format), "{ext}");
            assert_eq!(edit_output_ext(ext), ext);
        }
        for ext in ["", "heic", "svg", "pbm", "pgm", "unknown"] {
            assert_eq!(native_output_format(ext), None, "{ext}");
            assert!(!ext_needs_magick(ext), "{ext}");
            assert_eq!(edit_output_ext(ext), "png", "{ext}");
        }
    }

    #[test]
    fn exact_unknown_conversion_rejects_without_replacing_destination() {
        let dir = std::env::temp_dir().join(format!(
            "st2k-exact-unknown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            3,
            2,
            image::Rgb([20, 80, 160]),
        ))
        .save(&input)
        .unwrap();
        let output = dir.join("existing.unknown");
        std::fs::write(&output, b"original destination").unwrap();

        assert!(convert_to(
            input.to_str().unwrap(),
            &output,
            90,
            None,
            Resize::None
        )
        .is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"original destination");
        assert!(!with_tmp_suffix(&output).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_source_edits_use_png_name_and_signature() {
        let dir = std::env::temp_dir().join(format!(
            "st2k-edit-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.heic");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            12,
            8,
            image::Rgb([20, 80, 160]),
        ))
        .save_with_format(&input, ImageFormat::Png)
        .unwrap();

        let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
        assert_eq!(edited.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert!(std::fs::read(&edited).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image::open(&edited).unwrap().dimensions(), (8, 12));

        let resized = resize_file(input.to_str().unwrap(), Resize::Fit(6, 4)).unwrap();
        assert_eq!(resized.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert!(std::fs::read(&resized).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image::open(&resized).unwrap().dimensions(), (6, 4));
        let _ = std::fs::remove_dir_all(dir);
    }

    // Needs ImageMagick (bundled on a full install, or on PATH).
    #[test]
    #[ignore]
    fn exact_psd_and_magick_backed_edits_have_psd_signatures() {
        if !decode::magick_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "st2k-magick-routing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            30,
            image::Rgb([30, 160, 90]),
        ))
        .save(&input)
        .unwrap();

        let psd = dir.join("existing.psd");
        std::fs::write(&psd, b"old destination").unwrap();
        convert_to(
            input.to_str().unwrap(),
            &psd,
            90,
            None,
            Resize::None,
        )
        .unwrap();
        assert!(std::fs::read(&psd).unwrap().starts_with(b"8BPS"));
        assert!(!with_tmp_suffix(&psd).exists());

        let edited = transform_file(psd.to_str().unwrap(), Transform::Right90).unwrap();
        assert_eq!(edited.extension().and_then(|ext| ext.to_str()), Some("psd"));
        assert!(std::fs::read(&edited).unwrap().starts_with(b"8BPS"));

        let resized = resize_file(psd.to_str().unwrap(), Resize::Fit(20, 15)).unwrap();
        assert_eq!(resized.extension().and_then(|ext| ext.to_str()), Some("psd"));
        assert!(std::fs::read(&resized).unwrap().starts_with(b"8BPS"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
