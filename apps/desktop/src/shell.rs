//! Opening files with the user's default handler.
//!
//! `ShellExecuteW` is the only way to respect whatever editor is registered for
//! `.json`; spawning `notepad.exe` would ignore that choice.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

pub fn open(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("{path:?} does not exist yet"));
    }

    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file = wide(path.as_os_str());

    // ShellExecuteW returns a value > 32 on success.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if rc > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW returned {rc}"))
    }
}
