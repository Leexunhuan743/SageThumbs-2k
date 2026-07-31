# Decoding

How thumbnails, previews, and first pages are produced for the 326 supported formats, and which engine backs each tier: pure-Rust codecs, OS codecs, or the sandboxed ImageMagick child.

## Decoder Tiers

The ladder, in order: the `image` crate → WIC (HEIC/AVIF/RAW) → ImageMagick subprocess → headerless-Targa fallback.

Video, PDF, SVG, ebooks/comics, and audio cover art take dedicated paths that bypass the ladder; see [[architecture#Decode Pipeline]] for the overview.

## Video Frames

A representative frame (~30% into the file) is grabbed through Windows Media Foundation codecs streamed from disk via an `IMFByteStream` — no codec bytes ship with the app.

`src/video.rs` exposes the decode entry points and `src/vstream.rs` implements the byte stream; the MF imports are delay-loaded and gated behind a `media_foundation_available()` check before use.

## PDF Rendering

First-page thumbnails and previews use the in-box WinRT `Windows.Data.Pdf` rasterizer in `src/pdf.rs`.

The async plumbing (`IAsyncOperation`) comes from `windows-future` — zero bundled PDF engine.

## Quick Preview

The Space-bar viewer (EXE-only) renders Markdown via pulldown-cmark, syntax-highlighted code, archive listings, and OCR text from the WinRT path.

Archive listing comes from [[src/container/mod.rs#list_archive]] and OCR from the WinRT path behind [[src/lib.rs#ocr_probe]]. Local HTML preview is optional behind the `html-preview` feature (WebView2, scripts off) and default-off Settings toggles.
