//! The macOS Dock menu: right-clicking DBine's Dock icon shows "Nueva ventana"
//! above the list of open windows (which AppKit adds by itself).
//!
//! Neither Tauri nor tao exposes a Dock menu, so this adds methods to tao's
//! application delegate class at runtime:
//! - `applicationDockMenu:`, the `NSApplicationDelegate` hook AppKit asks for
//!   the menu each time the Dock icon is right-clicked;
//! - `dbineNewWindow:`, the menu item's action, which opens a new window;
//! - `applicationShouldTerminate:`, so a quit from the OS (Dock › Salir, the
//!   app switcher, logout) asks about running tasks like ⌘Q does. tao doesn't
//!   implement it, and without it `terminate:` ends the process at once.
//!
//! It depends on tao's private class name (`TaoAppDelegateParent`, tao 0.35).
//! If the class is gone, or a future tao already defines
//! `applicationDockMenu:`, it logs a warning and leaves the Dock menu alone:
//! recheck this on every tao upgrade.
#![cfg(target_os = "macos")]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use objc2::ffi;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::NSString;
use tauri::AppHandle;

const DELEGATE_CLASS: &std::ffi::CStr = c"TaoAppDelegateParent";

/// The handle the menu action opens windows with. Set once by `install`.
static APP: OnceLock<AppHandle> = OnceLock::new();

/// The item's title, in the UI language. `set_label` replaces it; the menu
/// reads it each time AppKit asks for it, so a change shows on the next
/// right-click without touching AppKit from a non-main thread.
static LABEL: Mutex<Option<String>> = Mutex::new(None);

const DEFAULT_LABEL: &str = "Nueva ventana";

/// Hooks the Dock menu into tao's application delegate. Call it once, from
/// `setup` (the main thread, after the event loop has set the delegate).
pub fn install(app: AppHandle) {
    let Some(mtm) = MainThreadMarker::new() else {
        // `class_addMethod` itself is thread-safe, but keep the whole
        // install on the main thread with the rest of the AppKit setup.
        let handle = app.clone();
        if let Err(e) = app.run_on_main_thread(move || install(handle)) {
            tracing::warn!("dock menu not installed: {e}");
        }
        return;
    };
    if APP.set(app).is_err() {
        return; // already installed
    }
    let Some(class) = AnyClass::get(DELEGATE_CLASS) else {
        tracing::warn!("dock menu not installed: TaoAppDelegateParent not found");
        return;
    };
    let class = class as *const AnyClass as *mut AnyClass;

    // SAFETY: each IMP matches the type encoding it is registered with
    // (`@@:@` = object return, self, _cmd, one object argument; `v@:@` = void
    // return, same arguments; `Q@:@` = NSUInteger return), and the functions
    // have the C ABI the runtime
    // calls them with. `class_addMethod` only adds methods: it never replaces
    // an existing implementation, and it returns NO if the class has one.
    unsafe {
        let dock_menu_imp: Imp = std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> *mut NSMenu,
            Imp,
        >(dock_menu);
        if !ffi::class_addMethod(
            class,
            sel!(applicationDockMenu:),
            dock_menu_imp,
            c"@@:@".as_ptr(),
        )
        .as_bool()
        {
            tracing::warn!(
                "dock menu not installed: the app delegate already has applicationDockMenu:"
            );
        }
        let new_window_imp: Imp = std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject),
            Imp,
        >(new_window);
        if !ffi::class_addMethod(
            class,
            sel!(dbineNewWindow:),
            new_window_imp,
            c"v@:@".as_ptr(),
        )
        .as_bool()
        {
            // The menu still shows; the item would be disabled because the
            // existing `dbineNewWindow:` is not ours. Should never happen.
            tracing::warn!("dock menu: the app delegate already has dbineNewWindow:");
        }
        // NSApplicationTerminateReply is an NSUInteger (`Q`).
        let should_terminate_imp: Imp = std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> usize,
            Imp,
        >(should_terminate);
        if !ffi::class_addMethod(
            class,
            sel!(applicationShouldTerminate:),
            should_terminate_imp,
            c"Q@:@".as_ptr(),
        )
        .as_bool()
        {
            tracing::warn!("quit guard: the app delegate already has applicationShouldTerminate:");
        }
    }
    // AppKit may cache which optional delegate methods the delegate answers
    // when it is set: set the same delegate again so it sees the new ones.
    let ns_app = NSApplication::sharedApplication(mtm);
    if let Some(delegate) = ns_app.delegate() {
        ns_app.setDelegate(Some(&delegate));
    }
    tracing::info!("dock menu installed");
}

/// Sets the "Nueva ventana" text in the UI language (from `app_menu_set`).
/// Safe to call from any thread, before or after `install`.
pub fn set_label(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Ok(mut label) = LABEL.lock() {
        *label = Some(text.to_owned());
    }
}

fn current_label() -> String {
    LABEL
        .lock()
        .ok()
        .and_then(|l| l.clone())
        .unwrap_or_else(|| DEFAULT_LABEL.to_owned())
}

/// `-[TaoAppDelegateParent applicationDockMenu:]`. AppKit calls it on the main
/// thread on every right-click of the Dock icon; returns an autoreleased menu
/// (a +0 reference, as the method's naming convention requires), or nil.
extern "C-unwind" fn dock_menu(this: &AnyObject, _cmd: Sel, _sender: *mut AnyObject) -> *mut NSMenu {
    // Never unwind into AppKit: a panic here gives no Dock menu instead.
    catch_unwind(AssertUnwindSafe(|| {
        let Some(mtm) = MainThreadMarker::new() else {
            return ptr::null_mut();
        };
        let menu = build_menu(mtm, this);
        Retained::autorelease_ptr(menu)
    }))
    .unwrap_or(ptr::null_mut())
}

fn build_menu(mtm: MainThreadMarker, target: &AnyObject) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    // SAFETY: `dbineNewWindow:` takes one object argument (the sender) and is
    // implemented on `target`'s class by `install`, so the selector is valid
    // for the target. The delegate outlives the menu (it lives as long as
    // NSApp), and NSMenuItem holds its target weakly anyway.
    let item = unsafe {
        let item = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(&current_label()),
            Some(sel!(dbineNewWindow:)),
            &NSString::new(),
        );
        item.setTarget(Some(target));
        item
    };
    menu.addItem(&item);
    menu
}

/// `-[TaoAppDelegateParent dbineNewWindow:]`, the menu item's action.
extern "C-unwind" fn new_window(_this: &AnyObject, _cmd: Sel, _sender: *mut AnyObject) {
    let _ = catch_unwind(|| {
        let Some(app) = APP.get() else { return };
        let handle = app.clone();
        // Leave AppKit's menu tracking first; the window opens on the next
        // turn of the event loop.
        let result = app.run_on_main_thread(move || {
            if let Err(e) = crate::windows::open_new(&handle) {
                tracing::warn!("dock menu: opening a new window failed: {e}");
            }
        });
        if let Err(e) = result {
            tracing::warn!("dock menu: opening a new window failed: {e}");
        }
    });
}

/// `NSTerminateCancel` / `NSTerminateNow`.
const TERMINATE_CANCEL: usize = 0;
const TERMINATE_NOW: usize = 1;

/// `-[TaoAppDelegateParent applicationShouldTerminate:]`: AppKit asks before
/// an OS quit. With tasks running (or a quit prompt already open) it cancels
/// and lets the UI ask, which quits through `quit_app` (tao's own exit never
/// goes through `terminate:`). Otherwise the quit goes on, so a logout isn't
/// held up; `applicationWillTerminate:` still runs the app's exit cleanup.
extern "C-unwind" fn should_terminate(_this: &AnyObject, _cmd: Sel, _sender: *mut AnyObject) -> usize {
    catch_unwind(|| {
        let Some(app) = APP.get() else { return TERMINATE_NOW };
        if crate::QUIT_CONFIRMED.load(std::sync::atomic::Ordering::SeqCst)
            || !crate::windows::quit_needs_ui(app)
            || !crate::windows::request_quit(app)
        {
            return TERMINATE_NOW;
        }
        TERMINATE_CANCEL
    })
    .unwrap_or(TERMINATE_NOW)
}
