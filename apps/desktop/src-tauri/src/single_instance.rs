//! Single-instance guard.
//!
//! A named kernel mutex in the `Local\` namespace is the cheapest primitive that
//! works on Windows and costs nothing at runtime (no polling, no file locks, no
//! sockets). The handle is intentionally leaked for the lifetime of the process:
//! closing it would let a second instance slip through.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// `Local\` scopes the mutex to the current logon session, so two different
/// Windows users can each run their own copy.
const MUTEX_NAME: &str = r"Local\now-doing-single-instance";

/// Returns `Some(handle)` when this process is the first one, `None` when another
/// instance already holds the mutex.
pub fn acquire() -> Option<isize> {
    acquire_named(MUTEX_NAME)
}

/// Same guard under an arbitrary name. Split out so tests do not collide with a
/// copy of the app the developer happens to have running in the tray.
fn acquire_named(mutex_name: &str) -> Option<isize> {
    let name: Vec<u16> = OsStr::new(mutex_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the call.
    let handle: HANDLE = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };

    if handle.is_null() {
        // Could not create the mutex at all (extremely unlikely). Refuse to start
        // rather than risk two instances writing the same config file.
        log::error!("CreateMutexW failed; refusing to start");
        return None;
    }

    // SAFETY: reading the thread-local last-error is always sound.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        // The handle is still ours, but the name pre-existed. Another instance is
        // live; leave the mutex alone and let the caller exit.
        return None;
    }

    Some(handle as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        // A dedicated name keeps this test green even when the developer has the
        // app itself running, which would otherwise hold the production mutex.
        let name = format!(r"Local\now-doing-test-{}", std::process::id());
        let first = acquire_named(&name);
        assert!(first.is_some(), "first acquire should succeed");
        let second = acquire_named(&name);
        assert!(second.is_none(), "second acquire must fail");
        // Keep `first` alive until the end of the test.
        drop(first);
    }
}
