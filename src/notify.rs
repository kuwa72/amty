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

/// One line: a newline breaks the AppleScript string and the Windows toast
/// XML, and an approval detail is a multi-line diff.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// argv of the notifier for `os`.
pub fn argv_for(os: Os, title: &str, body: &str) -> Vec<String> {
    let title = one_line(title);
    let body = one_line(body);
    let (title, body) = (title.as_str(), body.as_str());
    match os {
        Os::Linux => vec![
            "notify-send".into(),
            "-a".into(),
            "amty".into(),
            title.into(),
            body.into(),
        ],
        Os::Mac => {
            // AppleScript strings need their own quoting
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            vec![
                "osascript".into(),
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    esc(body),
                    esc(title)
                ),
            ]
        }
        Os::Windows => vec![
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
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($toast)",
                title = ps_escape(title),
                body = ps_escape(body),
            ),
        ],
    }
}

/// PowerShell single-quoted strings escape `'` by doubling it.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Fire and forget: a missing notifier or a failure never blocks the run.
pub fn send(title: &str, body: &str) {
    let argv = argv_for(current(), title, body);
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // a GUI-subsystem app must not flash a console for the notifier
    crate::catalog::no_window(&mut cmd);
    let child = cmd.spawn();
    // reap it: a GUI that lives for days would otherwise collect zombies
    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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
    fn multi_line_text_is_flattened() {
        let a = argv_for(Os::Mac, "amty", "fs_edit: /x\n--- /x\n+++ /x\n@@ -1,2 +1,2 @@");
        assert!(!a[2].contains('\n'), "{:?}", a);
        assert!(a[2].contains("fs_edit: /x --- /x"), "{:?}", a);
        let w = argv_for(Os::Windows, "amty", "a\nb");
        assert!(!w[3].contains('\n'), "{:?}", w);
    }

    #[test]
    fn linux_uses_notify_send() {
        let a = argv_for(Os::Linux, "amty", "done");
        assert_eq!(a, ["notify-send", "-a", "amty", "amty", "done"]);
    }

    #[test]
    fn mac_uses_osascript_and_escapes_quotes() {
        let a = argv_for(Os::Mac, "amty", "said \"hi\"");
        assert_eq!(a[0], "osascript");
        assert!(a[2].contains("display notification \"said \\\"hi\\\"\""), "{:?}", a);
    }

    #[test]
    fn windows_uses_a_powershell_toast_and_escapes_quotes() {
        let a = argv_for(Os::Windows, "amty", "it's done");
        assert_eq!(a[0], "powershell");
        assert!(a[3].contains("ToastNotificationManager"), "{:?}", a);
        // a toast needs a registered AppUserModelID; ours isn't one
        assert!(a[3].contains("1AC14E77-02E7-4E5D-B744-2EB1AE5198B7"), "{:?}", a);
        assert!(a[3].contains("it''s done"), "{:?}", a);
    }
}
