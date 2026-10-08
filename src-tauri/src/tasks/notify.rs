//! The OS notification at the end of a run: with the app closed there's no
//! window to show it in, so it goes through each OS's own tool. A failure
//! to notify is logged and nothing more.

use std::process::{Command, Stdio};

pub fn send(title: &str, body: &str) {
    let body: String = body.chars().take(240).collect();
    if let Err(e) = os_send(title, &body) {
        tracing::warn!("could not show the notification: {e}");
    }
}

fn quiet(cmd: &mut Command) -> std::io::Result<()> {
    let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("exit {status}"))) }
}

#[cfg(target_os = "macos")]
fn os_send(title: &str, body: &str) -> std::io::Result<()> {
    // AppleScript string literals: backslash and quote escaped.
    let lit = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let script = format!("display notification {} with title {}", lit(body), lit(title));
    quiet(Command::new("/usr/bin/osascript").arg("-e").arg(script))
}

#[cfg(windows)]
fn os_send(title: &str, body: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // A toast through WinRT, as PowerShell's own app id (always registered).
    let xml_escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
    let ps_literal = |s: &str| s.replace('\'', "''");
    let xml = format!(
        "<toast><visual><binding template='ToastGeneric'><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null; \
         $x = New-Object Windows.Data.Xml.Dom.XmlDocument; $x.LoadXml('{}'); \
         $t = [Windows.UI.Notifications.ToastNotification]::new($x); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($t)",
        ps_literal(&xml)
    );
    quiet(Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).creation_flags(CREATE_NO_WINDOW))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_send(title: &str, body: &str) -> std::io::Result<()> {
    quiet(Command::new("notify-send").args(["--app-name=DBine", title, body]))
}
