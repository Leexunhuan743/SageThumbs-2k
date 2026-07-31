//! The off-thread, budgeted menu-preview decode: worker accounting and the decode itself.
//!
//! Split out of `contextmenu.rs` 2026-07-31 (pure move).

use super::*;

/// The `Send` result of the off-thread menu-preview decode: the scaled RGBA thumbnail (the GDI
/// DIB is created on the caller's UI thread) plus the file's true source dimensions.
pub(crate) struct MenuThumb {
    pub(crate) rgba: Vec<u8>,
    pub(crate) w: i32,
    pub(crate) h: i32,
    pub(crate) ow: u32,
    pub(crate) oh: u32,
}

/// Wall-clock budget for the off-thread menu-preview decode. A shell menu callback must feel
/// immediate; if the cheap decoder cannot finish inside this small allowance, show the
/// caption-only tile instead of making Explorer wait.
pub(crate) const MENU_PREVIEW_BUDGET: std::time::Duration = std::time::Duration::from_millis(125);
/// Timed-out workers finish in the background. Bound their count so repeated right-clicks on a
/// pathological image cannot accumulate an unbounded number of decoders inside Explorer.
pub(crate) const MAX_MENU_PREVIEW_WORKERS: usize = 2;
pub(crate) static MENU_PREVIEW_WORKERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct MenuPreviewWorker;

impl Drop for MenuPreviewWorker {
    fn drop(&mut self) {
        MENU_PREVIEW_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Start reading + decoding `path` to a scaled menu thumbnail on a detached worker. Mirrors
/// `propstore::probe_budgeted` / `decode_svg`: the worker holds a `crate::ModuleRef` and inits
/// COM (the WIC HEIC/AVIF/RAW tier needs an apartment). Uses only the cheap in-process tiers
/// (`decode_menu_preview` — container covers, fast image/WIC tiers, and pure-Rust resvg for
/// SVG; no magick/video/pdf), so the worker is fast and bundled-byte-free.
pub(crate) fn start_menu_thumb(path: &str) -> Option<std::sync::mpsc::Receiver<Option<MenuThumb>>> {
    if MENU_PREVIEW_WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_MENU_PREVIEW_WORKERS).then_some(active + 1)
        })
        .is_err()
    {
        return None;
    }

    let path = path.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("st2k-menu-preview".into())
        .spawn(move || {
            let _worker = MenuPreviewWorker;
            #[allow(clippy::default_constructed_unit_structs)]
            let _module = crate::ModuleRef::default();
            let inited = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                )
            }
            .is_ok();
            let out = (|| {
                let bytes = std::fs::read(&path).ok()?;
                let img = crate::decode::decode_menu_preview(&bytes).ok()?;
                let (ow, oh) =
                    crate::container::real_dims(&bytes).unwrap_or((img.width(), img.height()));
                // Width up to PREVIEW_WIDE, height up to PREVIEW_BOX: wide images render wide,
                // normal/tall ones stay capped at the 88px height.
                let thumb = img.thumbnail(PREVIEW_WIDE, PREVIEW_BOX);
                let rgba = thumb.to_rgba8();
                let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                Some(MenuThumb {
                    rgba: rgba.into_raw(),
                    w,
                    h,
                    ow,
                    oh,
                })
            })();
            if inited {
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
            let _ = tx.send(out);
        });
    if worker.is_err() {
        MENU_PREVIEW_WORKERS.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    Some(rx)
}

/// Finish a previously-started decode, or start one on demand for diagnostic
/// callers. The shell path normally supplies a prefetched receiver, hiding most
/// or all of this bounded wait behind Explorer's own menu construction.
pub(crate) fn decode_menu_thumb_budgeted(
    path: &str,
    prefetched: Option<std::sync::mpsc::Receiver<Option<MenuThumb>>>,
) -> Option<MenuThumb> {
    let rx = prefetched.or_else(|| start_menu_thumb(path))?;
    rx.recv_timeout(MENU_PREVIEW_BUDGET).ok().flatten()
}
