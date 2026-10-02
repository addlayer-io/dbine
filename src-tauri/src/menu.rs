//! The native menu bar (macOS only; Windows and Linux have none, as VS Code).
//!
//! The UI owns the menu: it sends its structure with the labels in the app's
//! language (`app_menu_set`, on start and on every language change) and this
//! builds it. Clicking an item of the UI emits `app-menu` with the item's id
//! to the target window (`windows::target_window`); native items (copy,
//! paste, hide, quit…) do their job themselves. "Nueva ventana" is handled
//! here: it works with no window to forward it to.

use crate::error::CommandResult;
use serde::Deserialize;
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(target_os = "macos")]
use tauri::Runtime;
use tauri::AppHandle;

/// The item that opens a window; the backend handles it.
const NEW_WINDOW: &str = "file.newWindow";

/// The event the UI listens to; its payload is the clicked item's id.
pub const EVENT: &str = "app-menu";
/// Prefix of the UI's item ids, which tells them from the native items'.
const ID_PREFIX: &str = "app-menu:";

#[derive(Debug, Deserialize, PartialEq)]
pub struct AppMenuArgs {
    pub menus: Vec<MenuSpec>,
    /// The Dock menu's "Nueva ventana" in the app's language (macOS).
    #[serde(default, alias = "dockNewWindow")]
    pub dock_new_window: Option<String>,
}

/// A top-level menu. The first one is the application menu (macOS titles it
/// with the app's name).
#[derive(Debug, Deserialize, PartialEq)]
pub struct MenuSpec {
    pub label: String,
    pub items: Vec<Entry>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Entry {
    /// One of the UI's actions.
    Item {
        id: String,
        label: String,
        /// Tauri accelerator syntax (`CmdOrCtrl+N`).
        #[serde(default)]
        accelerator: Option<String>,
    },
    Separator,
    /// A system item; `label` in the app's language (else the system's text).
    Native {
        item: NativeItem,
        #[serde(default)]
        label: Option<String>,
    },
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum NativeItem {
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Fullscreen,
    Minimize,
    Zoom,
}

/// The menu event id of a UI item.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn event_id(id: &str) -> String {
    format!("{ID_PREFIX}{id}")
}

/// The UI's id behind a menu event, `None` for native items.
pub fn item_id(event_id: &str) -> Option<&str> {
    event_id.strip_prefix(ID_PREFIX)
}

/// Every UI item id, in menu order.
#[cfg_attr(not(test), allow(dead_code))]
fn item_ids(menus: &[MenuSpec]) -> Vec<&str> {
    menus
        .iter()
        .flat_map(|m| &m.items)
        .filter_map(|e| match e {
            Entry::Item { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// Install the UI's menu. Returns whether there is a native menu bar (only
/// on macOS): the UI then leaves the menu's shortcuts to it.
#[tauri::command(rename_all = "camelCase")]
pub async fn app_menu_set(app: AppHandle, args: AppMenuArgs) -> CommandResult<bool> {
    #[cfg(target_os = "macos")]
    {
        if let Some(text) = &args.dock_new_window {
            crate::dock_macos::set_label(text);
        }
        let menu = build(&app, &args.menus).map_err(|e| crate::error::CommandError::Internal(e.to_string()))?;
        app.set_menu(menu).map_err(|e| crate::error::CommandError::Internal(e.to_string()))?;
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, args);
        Ok(false)
    }
}

/// Forward clicks on the UI's items to the target window's UI.
pub fn on_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    use tauri::Emitter;
    let Some(id) = item_id(event.id().as_ref()) else { return };
    if id == NEW_WINDOW {
        crate::windows::spawn_new(app);
    } else if let Some(w) = crate::windows::target_window(app) {
        // With no window focused (all minimized), the target may be out of
        // sight: bring it forward so what the item opens is seen.
        crate::windows::bring_forward(&w);
        let _ = app.emit_to(w.label(), EVENT, id);
    }
}

#[cfg(target_os = "macos")]
fn build<R: Runtime>(app: &AppHandle<R>, menus: &[MenuSpec]) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for spec in menus {
        let mut items: Vec<Box<dyn IsMenuItem<R>>> = Vec::with_capacity(spec.items.len());
        let mut window_menu = false;
        for entry in &spec.items {
            items.push(match entry {
                Entry::Item { id, label, accelerator } => {
                    Box::new(MenuItem::with_id(app, event_id(id), label, true, accelerator.as_deref())?)
                }
                Entry::Separator => Box::new(PredefinedMenuItem::separator(app)?),
                Entry::Native { item, label } => {
                    window_menu |= matches!(item, NativeItem::Minimize | NativeItem::Zoom);
                    Box::new(native(app, *item, label.as_deref())?)
                }
            });
        }
        let refs: Vec<&dyn IsMenuItem<R>> = items.iter().map(|i| i.as_ref()).collect();
        let submenu = Submenu::with_items(app, &spec.label, true, &refs)?;
        // The system adds the open windows to it.
        if window_menu {
            submenu.set_as_windows_menu_for_nsapp()?;
        }
        menu.append(&submenu)?;
    }
    Ok(menu)
}

#[cfg(target_os = "macos")]
fn native<R: Runtime>(app: &AppHandle<R>, item: NativeItem, text: Option<&str>) -> tauri::Result<PredefinedMenuItem<R>> {
    use NativeItem::*;
    match item {
        About => PredefinedMenuItem::about(app, text, Some(AboutMetadata::default())),
        Services => PredefinedMenuItem::services(app, text),
        Hide => PredefinedMenuItem::hide(app, text),
        HideOthers => PredefinedMenuItem::hide_others(app, text),
        ShowAll => PredefinedMenuItem::show_all(app, text),
        Quit => PredefinedMenuItem::quit(app, text),
        Undo => PredefinedMenuItem::undo(app, text),
        Redo => PredefinedMenuItem::redo(app, text),
        Cut => PredefinedMenuItem::cut(app, text),
        Copy => PredefinedMenuItem::copy(app, text),
        Paste => PredefinedMenuItem::paste(app, text),
        SelectAll => PredefinedMenuItem::select_all(app, text),
        Fullscreen => PredefinedMenuItem::fullscreen(app, text),
        Minimize => PredefinedMenuItem::minimize(app, text),
        // macOS's "Zoom" is the window's maximize.
        Zoom => PredefinedMenuItem::maximize(app, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `web/src/composables/appMenu.ts` sends.
    #[test]
    fn reads_the_ui_menu() {
        let args: AppMenuArgs = serde_json::from_value(serde_json::json!({
            "menus": [
                { "label": "DBine", "items": [
                    { "kind": "native", "item": "about", "label": "Acerca de DBine" },
                    { "kind": "item", "id": "app.settings", "label": "Configuración…", "accelerator": "CmdOrCtrl+," },
                    { "kind": "separator" },
                    { "kind": "native", "item": "hideOthers" },
                    { "kind": "native", "item": "quit", "label": "Salir de DBine" }
                ]},
                { "label": "Archivo", "items": [
                    { "kind": "item", "id": "file.newConnection", "label": "Nueva conexión" },
                    { "kind": "item", "id": "file.newQuery", "label": "Nueva consulta", "accelerator": "CmdOrCtrl+N" }
                ]},
                { "label": "Ventana", "items": [
                    { "kind": "native", "item": "minimize" },
                    { "kind": "native", "item": "zoom" }
                ]},
                { "label": "Ayuda", "items": [
                    { "kind": "item", "id": "help.checkUpdates", "label": "Buscar actualizaciones…" },
                    { "kind": "separator" },
                    { "kind": "item", "id": "help.support", "label": "Apoyar el proyecto" }
                ]}
            ]
        }))
        .unwrap();

        assert_eq!(args.menus.len(), 4);
        assert_eq!(
            item_ids(&args.menus),
            ["app.settings", "file.newConnection", "file.newQuery", "help.checkUpdates", "help.support"],
        );
        assert_eq!(
            args.menus[3].items[0],
            Entry::Item { id: "help.checkUpdates".into(), label: "Buscar actualizaciones…".into(), accelerator: None },
        );
        let app = &args.menus[0].items;
        assert_eq!(app[0], Entry::Native { item: NativeItem::About, label: Some("Acerca de DBine".into()) });
        assert_eq!(app[2], Entry::Separator);
        assert_eq!(app[3], Entry::Native { item: NativeItem::HideOthers, label: None });
        assert_eq!(
            args.menus[1].items[0],
            Entry::Item { id: "file.newConnection".into(), label: "Nueva conexión".into(), accelerator: None },
        );
    }

    #[test]
    fn reads_the_dock_label() {
        let none: AppMenuArgs = serde_json::from_value(serde_json::json!({ "menus": [] })).unwrap();
        assert_eq!(none.dock_new_window, None);
        for key in ["dock_new_window", "dockNewWindow"] {
            let args: AppMenuArgs = serde_json::from_value(serde_json::json!({ "menus": [], key: "Nueva ventana" })).unwrap();
            assert_eq!(args.dock_new_window.as_deref(), Some("Nueva ventana"));
        }
    }

    #[test]
    fn tells_ui_items_from_native_ones() {
        assert_eq!(item_id(&event_id("view.explorer")), Some("view.explorer"));
        assert_eq!(item_id("about"), None);
    }

    #[test]
    fn rejects_unknown_native_items() {
        let bad = serde_json::json!({ "menus": [{ "label": "X", "items": [{ "kind": "native", "item": "print" }] }] });
        assert!(serde_json::from_value::<AppMenuArgs>(bad).is_err());
    }
}
