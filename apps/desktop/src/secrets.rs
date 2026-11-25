//! Credential storage backed by Windows DPAPI.
//!
//! Secrets are encrypted with `CryptProtectData` under the current user account
//! and stored in the portable data directory next to the config. That keeps the
//! whole install movable (nothing lands in a roaming profile) while still making
//! the file useless if copied to another machine or account.

use std::fs;
use std::path::PathBuf;

use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secrets {
    #[serde(default)]
    pub access_key_id: String,
    /// Held encrypted on disk; plaintext only in memory.
    #[serde(default)]
    pub secret_access_key: String,
    /// Optional Cloudflare API token for the write-then-purge optimisation.
    #[serde(default)]
    pub purge_token: String,
    #[serde(default)]
    pub zone_id: String,
}

fn path() -> PathBuf {
    crate::config::data_dir().join("secrets.bin")
}

impl Secrets {
    pub fn is_configured(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }

    pub fn load() -> Self {
        let raw = match fs::read(path()) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        let plain = match unprotect(&raw) {
            Some(p) => p,
            None => return Self::default(),
        };
        serde_json::from_slice(&plain).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec(self).map_err(|e| format!("secrets encode: {e}"))?;
        let blob = protect(&json).ok_or_else(|| "DPAPI encrypt failed".to_string())?;
        let target = path();
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("create {dir:?}: {e}"))?;
        }
        let tmp = target.with_extension("bin.tmp");
        fs::write(&tmp, &blob).map_err(|e| format!("write {tmp:?}: {e}"))?;
        fs::rename(&tmp, &target).map_err(|e| format!("rename {target:?}: {e}"))
    }
}

fn protect(plain: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let out = bytes.to_vec();
    unsafe { CoTaskMemFree(output.pbData as *mut core::ffi::c_void) };
    Some(out)
}

fn unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.is_empty() {
        return None;
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let out = bytes.to_vec();
    unsafe { CoTaskMemFree(output.pbData as *mut core::ffi::c_void) };
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip() {
        let plain = b"hello secret world";
        let sealed = protect(plain).expect("protect");
        assert_ne!(&sealed[..], &plain[..]);
        let back = unprotect(&sealed).expect("unprotect");
        assert_eq!(&back[..], &plain[..]);
    }
}
