# SageThumbs 2K patch to `lepton_jpeg` 0.5.8

This directory is the crates.io `lepton_jpeg` 0.5.8 package
(`microsoft/lepton_jpeg_rust` at tag `0.5.8`) plus the upstream `LICENSE.txt`
and `NOTICE.txt` (Apache-2.0), with 13 SageThumbs-2k hardening patches
(decode-side arithmetic/gate fixes 1-10, encode-side fixes 11-13 — see the
numbered list below).

## The bug

`src/structs/lepton_header.rs`, `LeptonHeader::read_compressed_lepton_header`
(the `early_eof_encountered` branch):

```rust
let mut max_last_segment_size = self.jpeg_file_size
    - u32::try_from(self.rinfo.garbage_data.len())?
    - u32::try_from(self.raw_jpeg_header_read_index)?
    - u32::try_from(SOI.len())?;
...
max_last_segment_size -= self.thread_handoff[i].segment_size;
```

For files encoded by the **legacy C++ lepton** whose declared
`jpeg_file_size` is smaller than `garbage_data + raw_jpeg_header + 2`
(observed with `dropbox/lepton`'s own `images/gold-legacy.lep`), the first
expression underflows and **panics** in any build with overflow checks
(debug/dev; tests). Under `panic = "abort"` — SageThumbs' release profile, and
the mode the shell extension runs in inside explorer/dllhost — that panic
becomes a process abort of the thumbnail host.

The same applies to the per-segment `-=` loop: a file whose declared size
doesn't cover the sum of its own segment sizes underflows there instead.

## The fix

All fixes are in `src/`; each is marked with a `SAGETHUMBS PATCH (0.5.8)`
comment. Together they enforce the invariant that a corrupt or malicious
`.lep` can produce a `LeptonError` — never a panic/abort inside the shell
host (see the repository-root `src/safety.rs` and the `panic = "abort"`
rationale in the repository-root `src/lib.rs`).

1. **`structs/lepton_header.rs` — early_eof size arithmetic** (`saturating_sub`
   on `jpeg_file_size - garbage - header - SOI` and the per-segment loop).
   Debug/overflow-checked builds panicked on legacy C++ files
   (`dropbox/lepton` `gold-legacy.lep`); release silently wrapped. Clamping to
   0 preserves the "shorten the last segment" intent and fails cleanly.
2. **`structs/lepton_header.rs` — zero thread-handoff guard.** `num_threads - 1`
   underflowed (usize) on legacy C++ files with an empty HH list; now a clean
   `BadLeptonFile`.
3. **`structs/lepton_header.rs` — first `parse()` must reach a scan.**
   `JpegHeader::parse` returns `Ok(false)` when the raw JPEG header ends at
   EOI before any SOS; the boolean was ignored. A ~30-byte hostile file
   (bare `FF D9` header + an HH marker) then indexed an empty `trunc_info`
   (`get_block_height(0)`) — a Vec OOB panic in ALL builds, release
   included. Requiring a real scan header also blocks the sibling
   huffman-index panic (huff tables stay at their 0xff defaults without an
   SOS, indexing `h_codes[0][255]` during recode).
4. **`structs/lepton_header.rs` — thread-range validation.** After the
   existing clamps, ranges must be non-inverted, contiguous
   (`start[i] == end[i-1]`), and start at row 0; otherwise
   `BlockBasedImage::merge`'s contiguity assert (a panic in ALL builds on
   the main thread for progressive files) is reachable with hostile HH
   values. Legit encoders always write such ranges (the format requires it
   for merge), so this rejects only corrupt/hostile headers.
5. **`structs/lepton_header.rs` — capped header length fields.** The HDR,
   FRS, and GRB length fields are hostile-controlled u32s; upstream
   `Vec::resize`d with no bound, so a ~40-byte file declaring ~4 GiB
   lengths OOM-aborted the process. All are now capped at
   `max_jpeg_file_size` (`BadLeptonFile` beyond). The same cap covers the
   CRS marker's `rst_count` loop bound (fix 10) — a deflate bomb (a few MiB
   of compressed input, itself bounded only by `compressed_header_size`)
   could otherwise decompress to GiB of restart-count entries and OOM.
6. **`jpeg/jpeg_write.rs` — bounded `rst_cnt` index.** The CRS marker's
   restart-count list can hold fewer entries than the file has scans;
   indexing it with the unbounded scan counter panicked in ALL builds. A
   missing entry now means "no injected restarts for this scan".
7. **`jpeg/block_based_image.rs` — saturating `luma_y_end - luma_y_start`.**
   Inverted ranges underflowed u32 (debug panic; release wrapped into a
   graceful OOM). Defense in depth behind fix 4.
8. **`structs/lepton_file_reader.rs` — saturating `jpeg_file_size_left`.**
   Declared file size smaller than the garbage blob underflowed u64 (debug
   panic on gold-legacy-style files).
9. **`structs/simple_threadpool.rs` — per-call pools must not leak threads.**
   Three coordinated changes:
   - Workers hold a **Weak** reference to the idle list instead of a strong
     Arc. Upstream's strong clone kept `Mutex<Vec<Sender>>` alive as long as
     a worker lived, so a per-call pool's channels never closed on drop and
     every decode leaked 1–2 permanently parked threads in dllhost.
   - Parked workers wait with `recv_timeout(250 ms)` and re-check pool
     liveness on timeout (their own captured Sender keeps the channel open,
     so plain `recv` never observes Disconnected once the pool is gone);
     they exit ≤250 ms after the pool drop. The liveness probe never
     re-pushes the Sender (each wake-up would duplicate the idle-list entry,
     and a worker later evicted by the NUM_CPUS cap would leave a stale
     Sender behind → `send` → `SendError` panic on the next submit).
   - `send` to a stale Sender (evicted worker) no longer unwraps; the task
     is re-queued onto a fresh worker.
   The process-global `DEFAULT_THREAD_POOL` is unaffected (never dropped;
   its Weak never expires, workers are reused). Regression tests in the
   vendored crate: `pool_drop_releases_idle_list` (drop releases the idle
   list) and `per_call_pool_workers_exit_on_drop` (workers actually
   terminate ≤ 250 ms after the drop — asserted via a cfg(test)-only
   liveness counter), plus the SageThumbs `decode::lepton` suite.
10. **`structs/lepton_header.rs` — capped CRS `rst_count` loop.** (See the
    note under fix 5.) `rst_count` is a hostile u32 loop bound; the loop
    reads 4 bytes per entry from the zlib stream, so an unbounded count is
    an OOM entry point even though `compressed_header_size` itself is
    capped. Legit files carry one small entry per scan.
11. **`structs/multiplexer.rs` — `MultiplexWriter::flush` propagates a dead
    writer channel.** Upstream `.unwrap()`-ed the channel `send` of every
    64 KiB block. When the multiplex writer thread exits early on an I/O
    error (disk full, quota, broken pipe), `multiplex_write` drops the
    packet receivers while encode workers are still flushing; the send then
    fails and the unwrap panicked inside a pool worker — aborting
    explorer/dllhost under panic=abort (`catch_unwind` is a no-op there).
    The patch maps the failure to an `io::ErrorKind::BrokenPipe` error that
    flows through the existing `?`/`context()` chains. Discriminating
    regression: the in-module `flush_after_receiver_drop_returns_error_not_panic`
    unit test (pre-patch it panics and fails).
12. **`jpeg/jpeg_header.rs` — Huffman length-end shift wraps explicitly.
    DEFENSIVE-ONLY:** Rust `<<` never panics on shifted-out value bits (only
    a shift amount ≥ bit width does; this amount is the literal 1), so
    upstream's `code = code << 1` was already byte-identical to
    `wrapping_shl(1)` in every profile — no debug abort existed. The patch
    makes the intended wrapping explicit: empty later lengths keep doubling
    `code`, so even a normal JPEG (e.g. counts[1]=2, counts[2]=3,
    counts[3]=1) reaches 61440 by the last round, and an early wrap trips
    the `code >= (1u32 << len)` layout check with a clean error. (An
    earlier audit draft proposed rejecting `code > 0x7fff` — that would
    have REJECTED every real JPEG with short codes, since empty rounds
    legitimately push the running code past 0x7fff; the wrap is the correct
    form.) No regression test discriminates this patch (there is no
    pre-patch failure to reproduce).
13. **`jpeg/jpeg_read.rs` — progressive successive-approximation shifts
    wrap explicitly. DEFENSIVE-ONLY:** the four `<< jf.cs_sal` coefficient
    shifts (DC first stage, DC refine, AC first stage, SA-later) — Rust
    `<<` never panics on shifted-out value bits, only on a shift amount ≥
    bit width, and `cs_sal` is bounded to < 12 by the SOS validation before
    any scan decode, so upstream was already byte-identical to
    `wrapping_shl` in every profile (no debug abort existed). The explicit
    form documents that coefficients intentionally wrap and keeps the
    semantics self-evident if the bound ever changes. No regression test
    discriminates this patch.

Encode-side audits (Phase 1 of the Lepton-encode feature) covered the
previously-unfuzzed JPEG entropy surface (`jpeg_read.rs` first/progressive
scan decode, `bit_reader.rs`, `jpeg_position_state.rs`, `model.rs`,
`lepton_encoder.rs`, `row_spec.rs`, `truncate_components.rs`,
`block_context.rs`). No release-mode panics beyond patches 11-13; all
structural asserts/indexes there are guarded (coefficient magnitudes are
capped at 2047 by the `CoefficientOutOfRange` gate before they can feed
model priors).

## Verification

- `cargo test` (debug): `decode::lepton::tests::*` plus the C++-interop
  samples (dropbox `gold-legacy.lep`, `iphone16.lep`, `narrowrst.lep`) must
  not panic; legacy-format files decode or return `Err`, never abort.
  Observed: `iphone16.lep` decodes to the full 3264×2448 JPEG;
  `gold-legacy.lep` returns `BadLeptonFile` (empty thread handoffs);
  `narrowrst.lep` returns `VersionUnsupported` (lepton version 4 — out of
  this crate's supported version-1 range).
- `cargo test --release`: same suite, release overflow-checking off — the
  regression this patch prevents would previously have been silent in
  release; the debug-mode panic is the reproducible symptom.
- `cargo test --manifest-path vendor/lepton_jpeg/Cargo.toml --lib
  simple_threadpool` (with `WORKSPACE_ROOT` set to a real directory for the
  crate's compile-time `env!`): the pool regression tests.
- `cargo test --manifest-path vendor/lepton_jpeg/Cargo.toml --lib
  flush_after_receiver_drop` (CI vendored-lepton job): the DISCRIMINATING
  regression for patch 11 — drops the writer receiver, calls
  `MultiplexWriter::flush` directly, asserts `Err(BrokenPipe)`
  (pre-patch: panic).
- `cargo test --manifest-path vendor/lepton_jpeg/Cargo.toml --test
  encode_failures` (CI vendored-lepton job): encode-side smoke —
  `output_io_error_propagates_not_panics` (fail-after-N writer; does NOT
  discriminate, the main loop `?` returns first) and
  `encode_succeeds_with_unlimited_writer` (fail_at = 512).
- `decode::lepton::tests::lepton_mutations_never_panic`: 400 deterministic
  bit-flip/truncation mutations + an oversized-declared-size file must never
  panic, only Ok/Err.

Remove this override after upstream ships the fixes (upstream `main` still
carries the unchecked arithmetic, the ignored parse boolean, the unbounded
header resizes, the uncapped CRS loop, the thread-pool Arc leak, and the
send-unwrap — and the LepViewer/LepThumb project vendors the same unfixed
0.5.8).
