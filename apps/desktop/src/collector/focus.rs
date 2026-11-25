//! Foreground window → owning process name, plus an optional window title.
//!
//! One read per tick costs microseconds: `GetForegroundWindow` and
//! `GetWindowThreadProcessId` are user32 lookups, and
//! `QueryFullProcessImageNameW` with `PROCESS_QUERY_LIMITED_INFORMATION` needs no
//! elevated rights. Nothing here enumerates processes or installs a hook.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, SM_CXSCREEN, SM_CYSCREEN,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    /// Bare executable name, e.g. `Code.exe`. Never a full path.
    pub process: String,
    /// Raw window title, only read when a rule asks for it.
    pub title: Option<String>,
    /// True when the window covers the whole primary monitor, i.e. likely a game
    /// or a fullscreen video. Sampling slows down so we stay out of the way.
    pub fullscreen: bool,
}

/// UWP apps put their visible window on a host process; the real identity is the
/// hosted app, which is not reachable without the AppUserModelId. Reporting the
/// host name would leak nothing useful to the reader, so it is treated as
/// unknown and hidden by the default policy.
const FRAME_HOSTS: [&str; 2] = ["ApplicationFrameHost.exe", "TextInputHost.exe"];

pub fn foreground(include_title: bool) -> Option<Foreground> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }

    let process = process_name(hwnd)?;
    if FRAME_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(&process)) {
        return None;
    }

    let title = if include_title {
        window_title(hwnd)
    } else {
        None
    };
    Some(Foreground {
        process,
        title,
        fullscreen: covers_screen(hwnd),
    })
}

/// A window whose client area spans the whole primary monitor is almost always a
/// game or a fullscreen video, precisely the case where an extra timer tick is
/// unacceptable. Two cheap user32 calls, no hooks.
fn covers_screen(hwnd: HWND) -> bool {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if w <= 0 || h <= 0 {
        return false;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    // Allow a small slack: exclusive-fullscreen windows sometimes overhang by a
    // few pixels, borderless ones occasionally land one short.
    width >= w - 8 && height >= h - 8
}

pub(crate) fn process_name(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return None;
    }

    // A protected or elevated process refuses this handle. Returning `None` keeps
    // the pipeline fail-closed: unknown apps are hidden by default.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || len == 0 {
        return None;
    }
    buf.truncate(len as usize);

    let full = OsString::from_wide(&buf).to_string_lossy().into_owned();
    let name = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

pub(crate) fn window_title(hwnd: HWND) -> Option<String> {
    // `GetWindowTextW(hwnd, null, 0)` looks like it returns the length, but that
    // only holds for windows owned by this process; for every other process it
    // returns 0, so the length has to come from GetWindowTextLengthW.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    let mut cap = if len > 0 { len as usize + 1 } else { 512 };

    // A window can report a length it then refuses to fill (a hung or protected
    // owner), and a title exactly as long as the buffer is indistinguishable from
    // a truncated one, hence the retry with room to spare.
    for _ in 0..3 {
        let mut buf = vec![0u16; cap];
        let written = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
        if written <= 0 {
            if cap >= 512 {
                return None;
            }
            cap = 512;
            continue;
        }
        let written = written as usize;
        if written + 1 >= cap && cap < 4096 {
            cap *= 2;
            continue;
        }
        buf.truncate(written);
        let text = OsString::from_wide(&buf)
            .to_string_lossy()
            .trim()
            .to_string();
        return if text.is_empty() { None } else { Some(text) };
    }
    None
}

/// Only used to decide whether the window belongs to a UWP frame host; kept
/// separate so the hot path does not pay for it.
pub fn window_class(hwnd_is_open: bool) -> Option<String> {
    if !hwnd_is_open {
        return None;
    }
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 256];
    let written = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(OsString::from_wide(&buf).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_current_foreground_without_panicking() {
        // There is always a foreground window in an interactive session; if the
        // test runner has none the call must degrade to `None`, not crash.
        let sample = super::foreground(false);
        if let Some(fg) = sample {
            assert!(!fg.process.contains('\\'), "path leaked: {}", fg.process);
            assert!(fg.title.is_none());
        }
    }
}
