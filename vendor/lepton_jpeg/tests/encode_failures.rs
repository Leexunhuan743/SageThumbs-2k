//! SAGETHUMBS regression tests for the encode-side hardening patches.
//!
//! Patch 11 (`structs/multiplexer.rs`): `MultiplexWriter::flush` used to
//! `.unwrap()` the channel send. When the multiplex writer thread exits early on
//! an I/O error (disk full, quota, broken pipe), `multiplex_write` drops the
//! packet receivers while encode workers are still flushing — the send then
//! fails and the unwrap panicked inside a pool worker, aborting
//! explorer/dllhost under panic=abort. The patch propagates a `BrokenPipe`
//! `io::Error` through the existing `?`/`context()` chains.
//!
//! These tests drive a fail-after-N-bytes writer to reach that path
//! deterministically and assert the failure surfaces as an `Err` of the
//! expected KIND — not as a caught unwrap-panic (which is what the unpatched
//! crate produced under the dev-profile `catch_unwind`, and what would abort
//! the host under the release profile).
//!
//! Self-contained: the only external input is the checked-in JPEG fixture
//! (`include_bytes`), so the CI vendored-lepton job runs this without the
//! `{WORKSPACE_ROOT}/images` corpus.

use std::io::{self, Cursor, Seek, Write};

use lepton_jpeg::{EnabledFeatures, LeptonThreadPriority, SimpleThreadPool, encode_lepton};

/// The same feature set the SageThumbs encode path uses: strict decode gates
/// (the container we produce must be decodable by our own decode tier).
fn features() -> EnabledFeatures {
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
        max_jpeg_file_size: 128 * 1024 * 1024,
        stop_reading_at_eoi: false,
    }
}

const JPEG: &[u8] = include_bytes!("../../../tests/fixtures/jpegtran/restart_420.jpg");

/// A writer that accepts up to `fail_at` bytes, then reports an I/O error —
/// the deterministic stand-in for a full disk. `Seek` satisfies the
/// `StreamPosition` blanket impl the encoder's `position()` calls need.
struct FailWriter {
    written: usize,
    fail_at: usize,
}

impl Write for FailWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() > self.fail_at {
            return Err(io::Error::new(io::ErrorKind::Other, "disk full (test)"));
        }
        self.written += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for FailWriter {
    fn seek(&mut self, _pos: io::SeekFrom) -> io::Result<u64> {
        Ok(self.written as u64)
    }
}

/// An output I/O failure mid-encode must surface as a propagated error whose
/// message identifies the I/O failure — NOT as an `AssertionFailure` carrying
/// an unwrap-panic payload ("called `Result::unwrap()` …").
///
/// NOTE on discrimination: this end-to-end smoke does NOT discriminate the
/// patch by itself — `multiplex_write`'s main loop hits the failing writer's
/// `?` first and returns "disk full (test)" identically before the workers'
/// final flush can fail, so the test also passed pre-patch. The discriminating
/// regression is the in-module `flush_after_receiver_drop_returns_error_not_panic`
/// unit test, which drives the exact failing send directly (pre-patch it
/// panics and fails). This smoke still guards the whole path: encode + failing
/// output writer must end in an `Err`, never a hang or a lost result.
#[test]
fn output_io_error_propagates_not_panics() {
    let pool = SimpleThreadPool::new(LeptonThreadPriority::Normal);
    // Mid-stream: past the container header (~200 B) but inside the block
    // write loop, so the failure hits the writer rather than the first bytes.
    // (The fixture's lepton output is well under 4 KiB.)
    let mut writer = FailWriter {
        written: 0,
        fail_at: 512,
    };
    let err = encode_lepton(&mut Cursor::new(JPEG), &mut writer, &features(), &pool)
        .expect_err("encode must fail on a failing writer");
    let msg = err.message();
    assert!(
        !msg.contains("unwrap"),
        "caught an unwrap-panic payload instead of a propagated I/O error: {msg}"
    );
    assert!(
        msg.contains("disk full") || msg.contains("multiplex writer"),
        "unexpected error kind: {msg}"
    );
}

/// Sanity: the same encode against an unlimited writer succeeds, so the test
/// above is failing for the right reason (the injected I/O error).
#[test]
fn encode_succeeds_with_unlimited_writer() {
    let pool = SimpleThreadPool::new(LeptonThreadPriority::Normal);
    let mut output = Vec::new();
    let mut cursor = Cursor::new(&mut output);
    encode_lepton(&mut Cursor::new(JPEG), &mut cursor, &features(), &pool)
        .expect("encode must succeed with an unlimited writer");
    assert!(!output.is_empty());
    // The container ends with the 4-byte size trailer.
    assert!(output.len() > 4);
}
