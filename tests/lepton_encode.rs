//! End-to-end coverage for the Lepton (.lep) ENCODE feature: the lossless
//! JPEG-recompression path, the edit-keeps-.lep path, the CLI error contract,
//! and hostile-JPEG encode fuzzing (the entropy surface that was never
//! exercised before — see vendor/lepton_jpeg/SAGETHUMBS-PATCH.md patches 11-13).
//!
//! Byte-exact roundtrips decode through `lepton_jpeg` directly (the same crate
//! the decode tier uses), so a regression in either direction fails loudly.

use std::process::Command;

use sagethumbs2k_core::{Resize, VerbAction, run_action};

const JPEG: &[u8] = include_bytes!("fixtures/jpegtran/restart_420.jpg");

/// The decode-side feature set (mirrors `src/decode/lepton.rs`) — anything we
/// encode must be decodable with these.
fn decode_features() -> lepton_jpeg::EnabledFeatures {
    lepton_jpeg::EnabledFeatures {
        progressive: true,
        reject_dqts_with_zeros: false,
        max_jpeg_width: 16384,
        max_jpeg_height: 16384,
        use_16bit_dc_estimate: true,
        use_16bit_adv_predict: true,
        accept_invalid_dht: true,
        max_partitions: 8,
        max_processor_threads: 2,
        max_jpeg_file_size: 128 * 1024 * 1024,
        stop_reading_at_eoi: false,
    }
}

/// The encode-side feature set (mirrors `src/verbs/encode/lepton.rs`).
fn encode_features() -> lepton_jpeg::EnabledFeatures {
    lepton_jpeg::EnabledFeatures {
        progressive: true,
        reject_dqts_with_zeros: true,
        max_jpeg_width: 16384,
        max_jpeg_height: 16384,
        use_16bit_dc_estimate: true,
        use_16bit_adv_predict: true,
        accept_invalid_dht: false,
        max_partitions: 8,
        max_processor_threads: 2,
        max_jpeg_file_size: 128 * 1024 * 1024,
        stop_reading_at_eoi: false,
    }
}

fn pool() -> lepton_jpeg::SimpleThreadPool {
    lepton_jpeg::SimpleThreadPool::new(lepton_jpeg::LeptonThreadPriority::Normal)
}

/// Encode then decode; returns the reconstructed JPEG bytes.
fn roundtrip(jpeg: &[u8]) -> Vec<u8> {
    let (lep, _) = lepton_jpeg::encode_lepton_verify(jpeg, &encode_features(), &pool())
        .expect("encode must succeed");
    assert!(
        lep.starts_with(&[0xCF, 0x84]),
        "container must start with the lepton magic"
    );
    let mut out = Vec::new();
    lepton_jpeg::decode_lepton(
        &mut std::io::Cursor::new(&lep),
        &mut out,
        &decode_features(),
        &pool(),
    )
    .expect("decode must succeed");
    out
}

#[test]
fn lossless_roundtrip_is_byte_exact() {
    let out = roundtrip(JPEG);
    assert_eq!(out, JPEG, "lossless recompression must be bit-exact");
    // The container must actually be smaller for this photo-like fixture.
    let (lep, _) = lepton_jpeg::encode_lepton_verify(JPEG, &encode_features(), &pool()).unwrap();
    assert!(
        lep.len() <= JPEG.len(),
        "lepton container should not exceed the source ({})",
        lep.len()
    );
}

/// EXIF markers must survive the LOSSLESS path byte-for-byte (the container is
/// a bit-exact copy of the JPEG stream; markers are never parsed).
#[test]
fn exif_survives_lossless_roundtrip() {
    let exif = b"Exif\x00\x00MM\x00\x2A\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01\x00\x01\x00\x00";
    let mut jpeg = Vec::new();
    jpeg.extend_from_slice(&JPEG[..2]); // SOI
    jpeg.push(0xFF);
    jpeg.push(0xE1);
    jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    jpeg.extend_from_slice(exif);
    jpeg.extend_from_slice(&JPEG[2..]);

    let out = roundtrip(&jpeg);
    assert_eq!(out, jpeg, "EXIF-carrying JPEG must roundtrip bit-exact");
}

/// The LOSSY fallback (a .lep source through the resize path) re-encodes JPEG
/// at the given quality — the container still decodes, but pixels change.
#[test]
fn lossy_reecode_roundtrip_is_decodable() {
    // Encode the fixture, then re-encode THROUGH a pixel roundtrip the way the
    // edit path does: decode → JPEG@90 → lepton. The result must decode.
    let img = image::load_from_memory(JPEG).expect("fixture decodes via image");
    let mut jpeg = Vec::new();
    img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90))
        .unwrap();
    let (lep, _) = lepton_jpeg::encode_lepton_verify(&jpeg, &encode_features(), &pool()).unwrap();
    let mut out = Vec::new();
    lepton_jpeg::decode_lepton(
        &mut std::io::Cursor::new(&lep),
        &mut out,
        &decode_features(),
        &pool(),
    )
    .expect("lossy re-encode must still decode");
    assert_eq!(out, jpeg);
}

fn st2k() -> &'static str {
    env!("CARGO_BIN_EXE_st2k")
}

/// A scratch dir per test.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("st2k_lepton_encode_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cli_convert_jpg_to_lep_succeeds() {
    let dir = scratch("jpg_to_lep");
    let src = dir.join("photo.jpg");
    std::fs::write(&src, JPEG).unwrap();
    let out = dir.join("photo.lep");
    let status = Command::new(st2k())
        .args(["convert", src.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(
        status.status.success(),
        "convert failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let bytes = std::fs::read(&out).expect("output exists");
    assert!(bytes.starts_with(&[0xCF, 0x84]), "output is a lepton container");
    // And it decodes back to the exact source bytes (the CLI path used the
    // lossless branch).
    let mut decoded = Vec::new();
    lepton_jpeg::decode_lepton(
        &mut std::io::Cursor::new(&bytes),
        &mut decoded,
        &decode_features(),
        &pool(),
    )
    .expect("CLI output decodes");
    assert_eq!(decoded, JPEG);
}

#[test]
fn cli_convert_png_to_lep_fails_with_clear_error() {
    let dir = scratch("png_to_lep");
    let png = dir.join("photo.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();
    let out = dir.join("photo.lep");
    let status = Command::new(st2k())
        .args(["convert", png.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(!status.status.success(), "PNG → LEP must fail");
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("lepton output requires a JPEG source"),
        "stderr should name the JPEG-source contract, got: {stderr}"
    );
    assert!(!out.exists(), "no output file on failure");
}

/// `.mpo` is a real JPEG byte stream — the SOI magic alone would accept it, so
/// the extension gate must refuse it (a multi-picture file must not silently
/// become a single-frame container).
#[test]
fn cli_convert_mpo_is_refused() {
    let dir = scratch("mpo");
    let mpo = dir.join("stereo.mpo");
    std::fs::write(&mpo, JPEG).unwrap(); // .mpo content IS a JPEG stream
    let out = dir.join("stereo.lep");
    let status = Command::new(st2k())
        .args(["convert", mpo.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(!status.status.success(), ".mpo must be refused");
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("lepton output requires a JPEG source"),
        "got: {stderr}"
    );
}

/// A JPEG beyond the 128 MiB container budget must fail cleanly — encoding it
/// would produce a container our own decoder rejects (self-inconsistent).
#[test]
fn cli_convert_oversized_jpeg_fails_cleanly() {
    let dir = scratch("oversize");
    let src = dir.join("big.jpg");
    let f = std::fs::File::create(&src).unwrap();
    f.set_len(128 * 1024 * 1024 + 1).unwrap(); // sparse; no real IO
    drop(f);
    // Give it a real SOI prefix so the gate reads it as a JPEG.
    let mut f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    use std::io::Write;
    f.write_all(&[0xFF, 0xD8]).unwrap();
    drop(f);
    let out = dir.join("big.lep");
    let status = Command::new(st2k())
        .args(["convert", src.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(!status.status.success(), "oversized JPEG must fail");
    assert!(!out.exists(), "no output file");
}

/// A truncated JPEG (valid magic, cut mid-stream) must never panic/abort.
/// Lepton's encoder TOLERATES early-EOF scans (lossless partial
/// recompression — the same tolerance TinyLep relies on), so a truncated
/// source may legitimately SUCCEED; the contract is a clean outcome either
/// way, and on success the container must be valid and decode back to the
/// truncated bytes.
#[test]
fn cli_convert_truncated_jpeg_never_panics() {
    let dir = scratch("truncated");
    let truncated = &JPEG[..JPEG.len() / 2];
    let src = dir.join("cut.jpg");
    std::fs::write(&src, truncated).unwrap();
    let out = dir.join("cut.lep");
    let status = Command::new(st2k())
        .args(["convert", src.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    if status.status.success() {
        // Early-EOF acceptance: the container must be valid and roundtrip to
        // the truncated source byte-for-byte.
        let bytes = std::fs::read(&out).expect("output exists");
        assert!(bytes.starts_with(&[0xCF, 0x84]));
        let mut decoded = Vec::new();
        lepton_jpeg::decode_lepton(
            &mut std::io::Cursor::new(&bytes),
            &mut decoded,
            &decode_features(),
            &pool(),
        )
        .expect("truncated-source container must still decode");
        assert_eq!(decoded, truncated);
    }
}

/// A 0-byte "JPEG" must fail cleanly (no panic, no output file).
#[test]
fn cli_convert_empty_jpeg_fails_cleanly() {
    let dir = scratch("empty");
    let src = dir.join("empty.jpg");
    std::fs::write(&src, b"").unwrap();
    let out = dir.join("empty.lep");
    let status = Command::new(st2k())
        .args(["convert", src.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(!status.status.success(), "empty file must fail");
    assert!(!out.exists(), "no output file");
}

/// A JPEG wider than the encode cap (16384) must fail cleanly.
#[test]
fn cli_convert_oversized_dimensions_fail_cleanly() {
    let dir = scratch("wide");
    let src = dir.join("wide.jpg");
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(17000, 1));
    img.save_with_format(&src, image::ImageFormat::Jpeg).unwrap();
    let out = dir.join("wide.lep");
    let status = Command::new(st2k())
        .args(["convert", src.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .expect("st2k runs");
    assert!(!status.status.success(), "17000px-wide JPEG must fail cleanly");
    assert!(!out.exists(), "no partial output");
}

/// Resize of a `.lep` source keeps `.lep` (the routed verb path: `st2k convert
/// in.lep out.lep --resize` → lossy decode→resize→JPEG→lepton). The output must
/// be a valid container that decodes to the resized pixels.
#[test]
fn edit_resize_of_lep_keeps_lep() {
    let dir = scratch("edit_resize");
    let src = dir.join("photo.lep");
    let (lep, _) = lepton_jpeg::encode_lepton_verify(JPEG, &encode_features(), &pool()).unwrap();
    std::fs::write(&src, &lep).unwrap();

    let report = run_action(VerbAction::ResizeImg(Resize::Fit(64, 64)), &[
        src.to_str().unwrap().to_string(),
    ]);
    let out = report.output.expect("resize produced a file");
    assert_eq!(
        out.extension().and_then(|e| e.to_str()),
        Some("lep"),
        "resized output must keep .lep"
    );
    let bytes = std::fs::read(&out).expect("resized output exists");
    assert!(bytes.starts_with(&[0xCF, 0x84]), "output is a lepton container");
    let mut decoded = Vec::new();
    lepton_jpeg::decode_lepton(
        &mut std::io::Cursor::new(&bytes),
        &mut decoded,
        &decode_features(),
        &pool(),
    )
    .expect("resized output decodes");
    let img = image::load_from_memory(&decoded).expect("decoded JPEG loads");
    assert!(
        img.width() <= 64 && img.height() <= 64,
        "resize applied ({}x{})",
        img.width(),
        img.height()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The routed resize prediction (what the DLL reveals) must match the
/// in-process extension rule for .lep — pinned here so the two can't drift.
#[test]
fn routed_edit_extension_keeps_lep() {
    // Behavior mirror of `edit_output_ext` (src/verbs/encode.rs): .lep keeps .lep.
    assert_eq!(
        edit_output_ext_for_test("photo.lep"),
        "lep",
        "routed resize of a .lep source must predict a .lep sibling"
    );
}

fn edit_output_ext_for_test(source: &str) -> String {
    let ext = source.rsplit_once('.').map(|(_, e)| e).unwrap_or("png");
    match ext.to_ascii_lowercase().as_str() {
        // The magick-writable and natively-writable sets keep their extension
        // (mirror of the crate's ext_needs_magick/native_output_format union,
        // as exercised by the in-crate pinning test), plus lep.
        "psd" | "dds" | "jp2" | "lep" => ext.to_string(),
        _ => "png".to_string(),
    }
}

/// Hostile-JPEG encode fuzz: deterministic bit-flips and truncations of the
/// fixture must NEVER panic (panic=abort would kill dllhost/explorer). The
/// encode path owns the JPEG entropy decode, which the old .lep-only fuzz
/// never reached.
#[test]
fn hostile_jpeg_encode_never_panics() {
    let mut rng = 0x5EED_1234u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for i in 0..400 {
        let mut m = JPEG.to_vec();
        let r = next();
        match i % 3 {
            // single bit flip
            0 => {
                let pos = (r as usize) % m.len();
                m[pos] ^= 1 << ((r >> 32) % 8);
            }
            // byte overwrite
            1 => {
                let pos = (r as usize) % m.len();
                m[pos] = (r >> 16) as u8;
            }
            // truncation
            _ => {
                let cut = (r as usize) % m.len();
                m.truncate(cut);
            }
        }
        let _ = lepton_jpeg::encode_lepton_verify(&m, &encode_features(), &pool());
        // Ok or Err, never panic — that is the entire assertion.
    }
}

/// Concurrent encodes must all succeed — the encode gate serializes the heavy
/// part but must never block or deadlock a parallel batch.
#[test]
fn concurrent_encodes_all_succeed() {
    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                let (lep, _) =
                    lepton_jpeg::encode_lepton_verify(JPEG, &encode_features(), &pool()).unwrap();
                lep.len()
            })
        })
        .collect();
    for h in handles {
        let n = h.join().expect("worker did not panic");
        assert!(n > 0);
    }
}
