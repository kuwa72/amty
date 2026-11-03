//! Desktop notification when a run ends or an approval waits, sent only
//! while the window is not focused (the GUI shows both in place otherwise).

/// The notifier platform decides the command; kept as a value so the
/// command construction is testable on any host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

pub fn current() -> Os {
    if cfg!(target_os = "macos") {
        Os::Mac
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

/// Send only when notifications are on and the window is in the background.
pub fn should_notify(enabled: bool, focused: bool) -> bool {
    enabled && !focused
}

/// argv of the notifier, or `None` when the platform has no known one.
pub fn argv_for(os: Os, title: &str, body: &str) -> Option<Vec<String>> {
    match os {
        Os::Linux => Some(vec![
            "notify-send".into(),
            "-a".into(),
            "amty".into(),
            title.into(),
            body.into(),
        ]),
        Os::Mac => {
            // AppleScript strings need their own quoting
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(vec![
                "osascript".into(),
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    esc(body),
                    esc(title)
                ),
            ])
        }
        Os::Windows => Some(vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] > $null; \
                 $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
                 $x = $t.GetElementsByTagName('text'); \
                 $x.Item(0).AppendChild($t.CreateTextNode('{title}')) > $null; \
                 $x.Item(1).AppendChild($t.CreateTextNode('{body}')) > $null; \
                 $toast = [Windows.UI.Notifications.ToastNotification]::new($t); \
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('amty').Show($toast)",
                title = ps_escape(title),
                body = ps_escape(body),
            ),
        ]),
    }
}

/// PowerShell single-quoted strings escape `'` by doubling it.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Fire and forget: a missing notifier or a failure never blocks the run.
pub fn send(title: &str, body: &str) {
    let Some(argv) = argv_for(current(), title, body) else { return };
    let _ = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_only_when_enabled_and_unfocused() {
        assert!(should_notify(true, false));
        assert!(!should_notify(true, true));
        assert!(!should_notify(false, false));
    }

    #[test]
    fn linux_uses_notify_send() {
        let a = argv_for(Os::Linux, "amty", "done").unwrap();
        assert_eq!(a, ["notify-send", "-a", "amty", "amty", "done"]);
    }

    #[test]
    fn mac_uses_osascript_and_escapes_quotes() {
        let a = argv_for(Os::Mac, "amty", "said \"hi\"").unwrap();
        assert_eq!(a[0], "osascript");
        assert!(a[2].contains("display notification \"said \\\"hi\\\"\""), "{:?}", a);
    }

    #[test]
    fn windows_uses_a_powershell_toast_and_escapes_quotes() {
        let a = argv_for(Os::Windows, "amty", "it's done").unwrap();
        assert_eq!(a[0], "powershell");
        assert!(a[3].contains("ToastNotificationManager"), "{:?}", a);
        assert!(a[3].contains("it''s done"), "{:?}", a);
    }
}
