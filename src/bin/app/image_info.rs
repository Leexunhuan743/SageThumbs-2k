//! The "Image info" window — a verbose, copyable metadata dump for the right-click
//! Tools verb. Launched standalone via `SageThumbs2K.exe --image-info <path>`: a
//! scrollable read-only edit with every file/image/EXIF field, plus a Copy button.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, run_dialog, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY};

const ID_EDIT: i32 = 100;

thread_local! {
    /// The metadata text to show — set just before `run_dialog`, read in WM_CREATE.
    static INFO: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Gather verbose metadata for `path` and show it in a scrollable, copyable window.
pub fn run_image_info(path: &str) {
    let text = sagethumbs2k_core::read_info_verbose(path);
    INFO.with(|i| *i.borrow_mut() = text);
    unsafe {
        // Title reuses the context-menu verb's key — same phrase, already translated
        // in every shipped locale.
        run_dialog(
            w!("SageThumbs2KImageInfo"),
            Some(info_wndproc),
            t("menu_image_info"),
            480,
            470,
            None,
        );
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Shared with the Upload-links and OCR result windows — see `win::result_layout` for why
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

    // Read-only, word-wrapped, vertically scrollable — the verbose dump can be long.
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
    // Edit controls want CRLF line breaks (a lone LF renders as a box). `to_crlf` rather than a
    // one-way `\n` -> `\r\n` replace: a line that is ALREADY CRLF (any EXIF/XMP value carrying
    // its own line breaks) would come out as `\r\r\n` and show a stray box anyway.
    let text = INFO.with(|i| sagethumbs2k_core::clipboard::to_crlf(&i.borrow()).into_owned());
    let w = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));

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

/// What the Copy button puts on the clipboard: the whole stored dump.
unsafe fn copy_source(_hwnd: HWND) -> String {
    INFO.with(|i| i.borrow().clone())
}

extern "system" fn info_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
