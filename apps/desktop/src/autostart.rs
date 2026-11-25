//! Per-user autostart via the `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! key.
//!
//! Deliberately not a Windows Service and not a Scheduled Task: a service needs
//! admin rights to install and cannot show a tray icon in the user's session; a
//! scheduled task with "run at logon" works but adds a second mechanism to debug.
//! The Run key needs no elevation and is the same thing every tray app does.

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "now-doing";

/// Path of the running executable, quoted so paths with spaces survive.
fn exe_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Ok(format!("\"{}\"", exe.display()))
}

/// Is the Run value present and pointing at *this* executable?
///
/// A stale value pointing at an old install path is treated as "not enabled" so
/// the UI never claims autostart works when it would launch a missing file.
pub fn is_enabled() -> bool {
    let Ok(hkcu) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)
    else {
        return false;
    };
    let stored: Result<String, _> = hkcu.get_value(VALUE_NAME);
    match (stored, exe_command()) {
        (Ok(current), Ok(expected)) => path_eq(&current, &expected),
        _ => false,
    }
}

/// Add or remove the Run entry. Returns the resulting state on success.
pub fn set_enabled(enabled: bool) -> Result<bool, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .map_err(|e| format!("open Run key: {e}"))?;

    if enabled {
        let cmd = exe_command()?;
        hkcu.set_value(VALUE_NAME, &cmd)
            .map_err(|e| format!("set Run value: {e}"))?;
        log::info!("autostart enabled -> {cmd}");
    } else {
        // Missing value is not an error: the desired end state is "absent".
        match hkcu.delete_value(VALUE_NAME) {
            Ok(()) => log::info!("autostart disabled"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("delete Run value: {e}")),
        }
    }

    Ok(enabled)
}

/// Compare two Run values ignoring case and insignificant whitespace differences,
/// which Windows itself does when resolving the command.
fn path_eq(a: &str, b: &str) -> bool {
    a.trim()
        .trim_matches('"')
        .eq_ignore_ascii_case(b.trim().trim_matches('"'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_is_ignored_in_comparison() {
        assert!(path_eq(r#""C:\a b\x.exe""#, r#"C:\a b\x.exe"#));
    }

    #[test]
    fn case_is_ignored() {
        assert!(path_eq(r"C:\Apps\Now-Doing.exe", r"c:\apps\now-doing.exe"));
    }

    #[test]
    fn different_paths_differ() {
        assert!(!path_eq(r"C:\one\x.exe", r"C:\two\x.exe"));
    }
}
