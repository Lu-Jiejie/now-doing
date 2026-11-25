//! A stable, non-identifying device id plus a readable machine name.
//!
//! The id is a random hex string generated once and kept in the data directory.
//! The machine name is only used for display and is capped in length.

use std::fs;
use std::path::PathBuf;

fn id_path() -> PathBuf {
    crate::config::data_dir().join("device-id")
}

pub fn load_or_create_id() -> String {
    if let Ok(existing) = fs::read_to_string(id_path()) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() && trimmed.len() <= 64 {
            return trimmed.to_string();
        }
    }
    let id = random_hex(16);
    let _ = fs::create_dir_all(crate::config::data_dir());
    let _ = fs::write(id_path(), &id);
    id
}

/// Randomness without a crypto dependency: the id is an identifier, not a
/// secret, and uniqueness only has to hold across one person's devices.
fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    let filled = fill_random(&mut bytes);
    if !filled {
        for b in bytes.iter_mut() {
            *b = (crate::util::unix_now() as u32).wrapping_mul(2654435761) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> bool {
    use windows_sys::Win32::Security::Cryptography::BCryptGenRandom;
    use windows_sys::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG;
    let ok = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    ok == 0
}

#[cfg(not(windows))]
fn fill_random(_buf: &mut [u8]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn random_hex_has_the_requested_length() {
        let id = super::random_hex(8);
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
