//! Last-input time, used to decide "active" vs "idle".
//!
//! `GetLastInputInfo` is a single user32 call over a cached struct: no hook, no
//! polling of the input queue, no measurable CPU.

use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// Seconds since the last keyboard or mouse event.
pub fn seconds_since_input() -> u64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if ok == 0 {
        return 0;
    }
    // Both counters are 32-bit milliseconds and wrap together, so wrapping
    // subtraction gives the correct delta across a wrap.
    let now = unsafe { GetTickCount() };
    let delta = now.wrapping_sub(info.dwTime) / 1000;
    delta as u64
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_a_plausible_delta() {
        // A fresh CI session may have never seen input; either way the value must
        // be a small, sane number rather than garbage from a failed call.
        let secs = super::seconds_since_input();
        assert!(secs < 60 * 60 * 24 * 30, "implausible idle time: {secs}");
    }
}
