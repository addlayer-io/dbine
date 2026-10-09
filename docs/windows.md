# Multiple windows

DBine can have several windows open at once. They are all the same instance
of the app: they share the connections, saved queries, settings, library and
the MCP server. What is changed in one window shows up in the others.

## Where "New window" is

| System | How to open it |
|---|---|
| macOS | Right-click (or press and hold) the DBine icon in the Dock › **New window**. Also in the **File › New window** menu or with ⌘⇧N. |
| Windows | Right-click the DBine icon in the taskbar › **New window**. Also with Ctrl+Shift+N, or by opening DBine again from the Start menu or a shortcut. |
| Linux | Right-click the launcher icon › **New window** (on desktops that show launcher actions, with the `.deb` and `.rpm` packages). Also with Ctrl+Shift+N, or by opening DBine again. |

On macOS, the Dock menu also lists the open windows, and a click on the icon
with all windows minimized brings back the last one used.

## What each window keeps

- **The first window keeps your tabs.** It is the one that restores the tabs
  and the AI assistant's conversation when DBine opens, and the one that saves
  them.
- **New windows start empty,** with no tabs or conversation. What is opened in
  them is not restored the next time.
- If the first window is closed and others remain, the last one used becomes
  the main one: from then on its tabs are the ones that get saved.
- Each window remembers its position and size.

## Closing a window or quitting

- **Closing a window** (the close button) closes only that window. If it has
  tasks running in the background (an export, a sync…), it asks only about
  those before cancelling them. The other windows' tasks continue.
- **Closing the last window** closes DBine, also on macOS.
- **Quit** (⌘Q, **Quit** in the menu or the Dock, or the system shutdown)
  closes all windows. If tasks are running, it asks once with those of all
  windows.

## Updates

- **The check when DBine opens** is done by a single window: the first one to
  start.
- **The new-version notice appears in a single window:** the one that found
  it, or the window where **Help › Check for updates…** was chosen. If you
  check from another window while a download is in progress, it says which
  window the download is in.
- **Restart to finish** asks once about the tasks of all windows, like Quit.
  Details: [`updates.md`](updates.md).
