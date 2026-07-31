//! Sibling-file navigation: which files count, and Explorer's sort order.
//!
//! Split out of `window.rs` 2026-07-31 (pure move).

use super::*;

/// True if `ext` (lowercase, no dot) is something the viewer can render — used to filter the
/// folder listing for ←/→ navigation so arrows skip files nothing can preview. Must stay in sync
/// with what `loader::load` actually handles: decoded formats + text/markdown + archives + fonts.
pub(in crate::preview) fn is_previewable_ext(ext: &str) -> bool {
    use sagethumbs2k_core::formats;
    formats::is_known(ext)
        || formats::is_preview_text(ext)
        || formats::is_preview_markdown(ext)
        || formats::is_preview_doc(ext)
        || content::is_archive_ext(ext)
        || crate::preview::font::is_font_ext(ext)
}

/// Explorer-style filename order (`image2` before `image10`). Precompute each UTF-16 key once
/// so a large-folder O(n log n) sort does not allocate inside every comparison.
pub(in crate::preview) fn sort_paths_like_explorer(
    files: Vec<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut keyed: Vec<(Vec<u16>, std::path::PathBuf)> = files
        .into_iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            (crate::win::wide(&name), p)
        })
        .collect();
    keyed.sort_by(|a, b| {
        unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) }
            .cmp(&0)
            .then_with(|| a.1.cmp(&b.1))
    });
    keyed.into_iter().map(|(_, p)| p).collect()
}

/// Flip to the next/prev previewable file in the current file's folder (QuickLook-style folder
/// traversal, wrapping at the ends), without closing the popup. Sorted case-insensitively by
/// Explorer's logical filename order.
pub(in crate::preview) unsafe fn nav_sibling(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let cur = match st.path.borrow().clone() {
        Some(p) => p,
        None => return,
    };
    let cur_path = std::path::Path::new(&cur);
    let Some(dir) = cur_path.parent() else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let files: Vec<std::path::PathBuf> = rd
        .flatten()
        .filter(|e| {
            // Use the DirEntry's cached file_type (no extra per-entry `stat` syscall — a huge
            // folder would otherwise cost thousands of stats on the UI thread) and gate on the
            // extension BEFORE anything else.
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && std::path::Path::new(&e.file_name())
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| is_previewable_ext(&x.to_ascii_lowercase()))
                    .unwrap_or(false)
        })
        .map(|e| e.path())
        .take(20_000) // sanity cap for a pathological folder
        .collect();
    if files.len() < 2 {
        return;
    }
    let files = sort_paths_like_explorer(files);
    let idx = files.iter().position(|p| p == cur_path).unwrap_or(0) as i32;
    let n = files.len() as i32;
    let ni = ((idx + delta) % n + n) % n; // wrap around at both ends
    let next = files[ni as usize].to_string_lossy().into_owned();
    if next != cur {
        request_load(hwnd, &next);
    }
}
