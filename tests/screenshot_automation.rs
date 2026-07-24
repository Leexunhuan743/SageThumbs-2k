//! Full-screen screenshot-editor automation contract.
//!
//! This test is deliberately ignored by default: it opens an opaque, topmost window over
//! the complete virtual desktop. Run it explicitly when changing screenshot window creation:
//!
//! `cargo test --test screenshot_automation -- --ignored --test-threads=1`
//!
//! The hidden route must remain synthetic and side-effect-free. This test checks only the
//! externally observable window contract; focused unit tests in the screenshot module cover
//! the mode/style decisions without opening UI.
#![cfg(windows)]

use std::ffi::c_void;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, PostMessageW, GWL_EXSTYLE, GW_OWNER,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WM_CLOSE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

const TITLE_PREFIX: &str = "SageThumbs 2K Screenshot Automation";
const INITIAL_PAINTED_TITLE: &str =
    "SageThumbs 2K Screenshot Automation | snap=0 | commit=0 | painted=0 | status=ready";

struct TestChild(Child);

impl TestChild {
    fn close_and_wait(&mut self, hwnd: windows::Win32::Foundation::HWND) {
        unsafe {
            PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))
                .expect("post WM_CLOSE to automation overlay");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.0.try_wait().expect("query automation child").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("automation overlay did not exit after WM_CLOSE");
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        // Scoped to the exact child this test launched. This also cleans up after an
        // assertion panic without touching a user's normal screenshot/daemon process.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

unsafe fn automation_window() -> Option<windows::Win32::Foundation::HWND> {
    FindWindowW(w!("SageThumbs2KShotAutomation"), PCWSTR::null()).ok()
}

unsafe fn normal_capture_window() -> Option<windows::Win32::Foundation::HWND> {
    FindWindowW(w!("SageThumbs2KShot"), PCWSTR::null()).ok()
}

unsafe fn window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

#[test]
#[ignore = "opens the synthetic full-screen screenshot automation overlay"]
fn synthetic_overlay_is_discoverable_by_windows_automation() {
    // Match the PMv2-aware app before comparing virtual-screen metrics/window
    // bounds. Without this, Windows may DPI-virtualize the test caller and make an
    // exact full-screen window look smaller on mixed-DPI desktops.
    unsafe {
        let _ =
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    unsafe {
        assert!(
            automation_window().is_none(),
            "a screenshot automation overlay is already running; close it before this test"
        );
        assert!(
            normal_capture_window().is_none(),
            "a normal screenshot overlay is already running; close it before this test"
        );
    }

    let child = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--screenshot-automation")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch synthetic screenshot automation mode");
    let child_id = child.id();
    let mut child = TestChild(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    let hwnd = loop {
        if let Some(status) = child.0.try_wait().expect("query automation child") {
            panic!("automation child exited before creating its window: {status}");
        }
        let found = unsafe { automation_window() };
        if let Some(hwnd) = found {
            // The bare prefix is used at CreateWindowEx time. Waiting for the full
            // telemetry title proves the first real WM_PAINT completed, rather than
            // accepting a merely-visible but unpainted popup.
            if unsafe {
                IsWindowVisible(hwnd).as_bool()
                    && window_title(hwnd) == INITIAL_PAINTED_TITLE
            } {
                break hwnd;
            }
        }
        assert!(
            Instant::now() < deadline,
            "automation overlay did not become visible within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    unsafe {
        let mut window_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        assert_eq!(
            window_pid, child_id,
            "the discovered automation window must belong to this test's exact child"
        );

        let title = window_title(hwnd);
        assert!(
            title.starts_with(TITLE_PREFIX),
            "unexpected automation window title: {title:?}"
        );
        assert_eq!(
            title, INITIAL_PAINTED_TITLE,
            "automation window must publish its post-paint initial state"
        );

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        assert_eq!(
            ex_style & WS_EX_TOOLWINDOW.0,
            0,
            "WS_EX_TOOLWINDOW makes the editor invisible to Windows UI automation"
        );
        assert_ne!(
            ex_style & WS_EX_TOPMOST.0,
            0,
            "the capture editor must remain topmost"
        );
        assert_ne!(
            ex_style & WS_EX_NOACTIVATE.0,
            0,
            "WS_EX_NOACTIVATE keeps this ownerless popup out of the taskbar by default"
        );
        assert!(
            GetWindow(hwnd, GW_OWNER).is_err(),
            "the automation window must be ownerless so discovery accepts it"
        );

        let mut cloaked = 1u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .expect("query DWM cloak state");
        assert_eq!(cloaked, 0, "automation window must not be DWM-cloaked");

        let mut actual = RECT::default();
        GetWindowRect(hwnd, &mut actual).expect("query automation window bounds");
        let expected = RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        };
        assert_eq!(
            actual, expected,
            "automation canvas must cover the complete virtual desktop"
        );
    }

    child.close_and_wait(hwnd);
}
