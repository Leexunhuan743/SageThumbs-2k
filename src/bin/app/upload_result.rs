//! The upload-result window — shows the uploaded link(s) in a selectable, read-only
//! edit with a **Copy** button (copies every link to the clipboard) and Close. Used by
//! the right-click "Upload" verb (`--upload-keep`, one line per image) and the
//! screenshot Upload button (`--upload`, a single link). The links are already on the
//! clipboard when this opens; Copy re-copies them (handy if the clipboard changed since,
//! or to grab them again after picking one out of the list). Modeled on `image_info.rs`.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, run_dialog, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY};

const ID_EDIT: i32 = 100;

thread_local! {
    /// (heading line, links joined by CRLF) — set before `run_dialog`, read in WM_CREATE.
    /// The edit shows the heading + the links; the Copy button copies ONLY the links.
    static RESULT: RefCell<(String, String)> =
        const { RefCell::new((String::new(), String::new())) };
}

/// Show the uploaded `links` (CRLF-separated — one per image) under `heading`, with a
/// Copy button that (re-)copies just the links to the clipboard.
pub fn show_upload_result(heading: &str, links: &str) {
    RESULT.with(|r| *r.borrow_mut() = (heading.to_string(), links.to_string()));
    unsafe {
        // `run_dialog`'s w/h are the TOTAL window size (no client adjustment), so the
        // client is ~30 design-px shorter than `h`. Size generously and keep the buttons
        // well inside the client — a too-short window clips the Copy/Close row.
        run_dialog(
            w!("SageThumbs2KUploadResult"),
            Some(result_wndproc),
            t("up_caption_file"),
            460,
            300,
            None,
        );
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Shared with the Image-info and OCR result windows — see `win::result_layout` for why
    // this has to come off the real client rect rather than the design size.
    let crate::win::ResultLayout {
        cw,
        m,
        btn_w,
        btn_h,
        gap,
        btn_y,
        close_x,
        copy_x,
        ..
    } = crate::win::result_layout(hwnd);
    let edit_h = (btn_y - gap - m).max(48);

    // Read-only, selectable, vertically scrollable — a multi-image upload can list many links.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        edit_style,
        m,
        m,
        cw - 2 * m,
        edit_h,
        ID_EDIT,
        hinst,
    );
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar. Re-theme
    // the edit to DarkMode_Explorer so its scrollbar renders dark (the edit bg/text stay dark
    // via WM_CTLCOLOREDIT in `dark_ctlcolor`).
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    let text = RESULT.with(|r| {
        let (h, l) = &*r.borrow();
        // Edit controls want CRLF; the links are already CRLF-joined, the heading may not be.
        // `to_crlf` (not a one-way `\n` -> `\r\n` replace) so an already-CRLF heading doesn't
        // become `\r\r\n` and render a stray box.
        use sagethumbs2k_core::clipboard::to_crlf;
        format!("{}\r\n\r\n{}", to_crlf(h), to_crlf(l))
    });
    let wtext = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(wtext.as_ptr()));

    // Buttons bottom-right, inside the client (Close rightmost, Copy to its left).
    ctl(
        hwnd,
        BUTTON,
        t("btn_copy"),
        WS_TABSTOP,
        copy_x,
        btn_y,
        btn_w,
        btn_h,
        ID_RESULT_COPY,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        close_x,
        btn_y,
        btn_w,
        btn_h,
        IDOK,
        hinst,
    );
}

/// What the Copy button puts on the clipboard: the LINKS only, never the heading above them.
unsafe fn copy_source(_hwnd: HWND) -> String {
    RESULT.with(|r| r.borrow().1.clone())
}

extern "system" fn result_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        // Create / Copy / close / quit are identical across the three result dialogs.
        if let Some(r) = crate::win::result_wndproc(hwnd, msg, wparam, build, copy_source) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
