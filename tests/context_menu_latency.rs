//! Latency guard for the legacy Explorer context-menu surface.
//!
//! This is deliberately an integration test rather than a micro-benchmark: it drives the
//! exact COM path Explorer uses (shell item -> IDataObject -> IShellExtInit ->
//! QueryContextMenu).  The budget is intentionally generous; it catches the old synchronous
//! preview decode (which could wait two seconds) without failing due to ordinary CI noise.
#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoInitializeEx, IClassFactory, IDataObject, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Shell::{
    BHID_DataObject, IContextMenu, IContextMenu3, IShellExtInit, IShellItem,
    SHCreateItemFromParsingName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, GetSubMenu, HMENU,
    MENUITEMINFOW, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE, WM_INITMENUPOPUP,
};

const CLSID_CONTEXT_MENU: GUID = GUID::from_u128(0x9F3A2B1C_5E8D_4A7F_9C2E_1B6D4F8A0E53);
const TEST_SETTINGS_ROOT: &str = r"Software\SageThumbs2K\__test_context_menu_latency_absent";
const FIRST_CALL_BUDGET: Duration = Duration::from_millis(750);

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn dll_path() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sagethumbs2k.dll")
}

unsafe fn context_menu_for(file: &std::path::Path) -> Result<IContextMenu> {
    // Keep this independent of a developer's real Explorer preferences.  The absent key
    // selects the product defaults, including the standard submenu preview placement.
    std::env::set_var("ST2K_SETTINGS_ROOT", TEST_SETTINGS_ROOT);
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let path = dll_path();
    assert!(
        path.exists(),
        "cdylib not built at {path:?} — run cargo build first"
    );
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let get_class: DllGetClassObjectFn = std::mem::transmute(proc);
    let mut factory_ptr = std::ptr::null_mut();
    get_class(&CLSID_CONTEXT_MENU, &IClassFactory::IID, &mut factory_ptr).ok()?;
    let factory = IClassFactory::from_raw(factory_ptr);

    let init: IShellExtInit = factory.CreateInstance(None)?;
    let file_wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(file_wide.as_ptr()), None)?;
    let object: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
    init.Initialize(None, Some(&object), Some(HKEY::default()))?;
    init.cast()
}

#[test]
fn first_query_context_menu_is_fast_with_default_preview_enabled() {
    let dir =
        std::env::temp_dir().join(format!("st2k_context_menu_latency_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("large-enough-to-decode.png");
    // A real but modest image: old eager-preview code performs a decode here, whereas the
    // expected path only performs extension/metadata/menu construction.
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(4096, 4096))
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();

    unsafe {
        let menu = context_menu_for(&png).expect("create and initialize classic context menu");
        let popup: HMENU = CreatePopupMenu().expect("CreatePopupMenu");
        let started = Instant::now();
        let result = menu.QueryContextMenu(popup, 0, 1, 0x7fff, 0);
        let elapsed = started.elapsed();
        result.ok().expect("QueryContextMenu");
        assert!(
            elapsed < FIRST_CALL_BUDGET,
            "first QueryContextMenu took {} ms (budget {} ms): preview work must stay lazy/off the shell thread",
            elapsed.as_millis(), FIRST_CALL_BUDGET.as_millis()
        );

        // Product defaults place the preview inside the SageThumbs submenu. QueryContextMenu
        // must only reserve that slot; Explorer sends WM_INITMENUPOPUP immediately before it
        // paints the flyout. Drive that exact lifecycle through IContextMenu3 and prove the
        // bitmap preview + separating divider appear once, even if Explorer repeats the
        // notification.
        let submenu = GetSubMenu(popup, 0);
        assert!(
            !submenu.is_invalid(),
            "first root item must be the SageThumbs submenu under default settings"
        );
        let before = GetMenuItemCount(Some(submenu));
        assert!(before > 0, "SageThumbs submenu must contain command items");
        let menu3: IContextMenu3 = menu.cast().expect("IContextMenu3");
        menu3
            .HandleMenuMsg2(
                WM_INITMENUPOPUP,
                WPARAM(submenu.0 as usize),
                LPARAM(0),
                None,
            )
            .expect("first WM_INITMENUPOPUP");
        let after_first = GetMenuItemCount(Some(submenu));
        assert_eq!(
            after_first,
            before + 2,
            "opening the SageThumbs flyout must insert exactly preview + separator"
        );

        let mut preview = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            ..Default::default()
        };
        GetMenuItemInfoW(submenu, 0, true, &mut preview).expect("preview menu item info");
        assert!(
            !preview.hbmpItem.is_invalid(),
            "first inserted flyout item must carry the preview bitmap"
        );
        let mut divider = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            ..Default::default()
        };
        GetMenuItemInfoW(submenu, 1, true, &mut divider).expect("preview divider item info");
        assert_eq!(
            divider.fType, MFT_SEPARATOR,
            "preview must be followed by a separator"
        );

        menu3
            .HandleMenuMsg2(
                WM_INITMENUPOPUP,
                WPARAM(submenu.0 as usize),
                LPARAM(0),
                None,
            )
            .expect("repeated WM_INITMENUPOPUP");
        assert_eq!(
            GetMenuItemCount(Some(submenu)),
            after_first,
            "repeated submenu initialization must not duplicate the preview"
        );
        let _ = DestroyMenu(popup);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
