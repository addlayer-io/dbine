//! Windows taskbar Jump List: a "new window" user task on the taskbar
//! right-click menu.
//!
//! The task starts `dbine.exe --new-window`; the single-instance plugin hands
//! that argv to the running process, which opens a window instead of starting
//! a second process.
//!
//! Jump Lists are keyed by AppUserModelID. Tauri's NSIS and MSI installers
//! stamp their shortcuts with `System.AppUserModel.ID = identifier`, so the
//! process sets the same ID explicitly. Without it Windows derives one from
//! the exe path, which matches neither the pinned shortcut nor a launch that
//! did not come from a shortcut (an updater relaunch, `dbine.exe` from a shell).

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
    SetCurrentProcessExplicitAppUserModelID, ShellLink, SHStrDupW,
};

/// Must match `identifier` in `tauri.conf.json`: the bundler writes it as the
/// AppUserModelID of the installed shortcuts (NSIS `SetLnkAppUserModelId`,
/// MSI `ShortcutProperty System.AppUserModel.ID`).
const APP_USER_MODEL_ID: &str = "com.addlayer.dbine";

/// Argument the task passes; the single-instance callback looks for it.
const NEW_WINDOW_ARG: &str = "--new-window";

/// Sets the process AppUserModelID and builds the Jump List with one task
/// titled `title`. Call it once from `setup`, before any window is shown: the
/// AppUserModelID has to be set before the process presents UI. The list is
/// built on its own COM single-threaded apartment thread, so the caller's
/// thread is never initialised for COM.
pub fn install(title: &str) {
    let id = HSTRING::from(APP_USER_MODEL_ID);
    if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&id) } {
        tracing::warn!("jump list: could not set the AppUserModelID: {e}");
    }
    let title = title.to_string();
    let spawned = std::thread::Builder::new()
        .name("jumplist".into())
        .spawn(move || {
            let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if init.is_err() {
                tracing::warn!("jump list: COM init failed: {init:?}");
                return;
            }
            if let Err(e) = build(&title) {
                tracing::warn!("jump list: could not build it: {e}");
            }
            unsafe { CoUninitialize() };
        });
    if let Err(e) = spawned {
        tracing::warn!("jump list: could not start its thread: {e}");
    }
}

fn build(title: &str) -> windows::core::Result<()> {
    // SAFETY: plain COM calls on interfaces this thread created after
    // CoInitializeEx; every pointer passed outlives its call.
    unsafe {
        let exe = std::env::current_exe()
            .map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
        let exe = HSTRING::from(exe.as_os_str());
        let title_w = HSTRING::from(title);

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&exe)?;
        link.SetArguments(&HSTRING::from(NEW_WINDOW_ARG))?;
        link.SetIconLocation(&exe, 0)?;
        link.SetDescription(&title_w)?;

        // A task's visible text is its PKEY_Title, as VT_LPWSTR like
        // InitPropVariantFromString makes it. SHStrDupW allocates with
        // CoTaskMemAlloc, which PROPVARIANT's Drop (PropVariantClear) frees.
        let store: IPropertyStore = link.cast()?;
        let mut value = PROPVARIANT::default();
        {
            let inner = &mut value.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = SHStrDupW(&title_w)?;
        }
        store.SetValue(&PKEY_Title, &value)?;
        store.Commit()?;

        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        list.SetAppID(&HSTRING::from(APP_USER_MODEL_ID))?;
        let mut min_slots = 0u32;
        let _removed: IObjectArray = list.BeginList(&mut min_slots)?;
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        tasks.AddObject(&link)?;
        let tasks: IObjectArray = tasks.cast()?;
        if let Err(e) = list.AddUserTasks(&tasks) {
            let _ = list.AbortList();
            return Err(e);
        }
        list.CommitList()
    }
}
