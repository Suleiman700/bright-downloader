//! Saved site passwords. On Windows they are encrypted with DPAPI for the
//! current user, so `settings.json` never holds them in clear text and
//! another account on the machine cannot read them.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const PREFIX: &str = "dpapi:";

#[cfg(windows)]
fn protect(plain: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: plain.len() as u32, pbData: plain.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: `input` points at `plain`, which outlives the call; DPAPI
    // allocates `out` with LocalAlloc and we free it after copying.
    unsafe {
        if CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(bytes)
    }
}

#[cfg(windows)]
fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: sealed.len() as u32, pbData: sealed.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: as in `protect`.
    unsafe {
        if CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(bytes)
    }
}

#[cfg(not(windows))]
fn protect(plain: &[u8]) -> Option<Vec<u8>> {
    Some(plain.to_vec())
}

#[cfg(not(windows))]
fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
    Some(sealed.to_vec())
}

pub fn seal(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    protect(plain.as_bytes()).map_or_else(String::new, |b| format!("{PREFIX}{}", STANDARD.encode(b)))
}

/// Empty if the value can't be decrypted (e.g. settings copied from another account).
pub fn open(sealed: &str) -> String {
    sealed
        .strip_prefix(PREFIX)
        .and_then(|b| STANDARD.decode(b).ok())
        .and_then(|b| unprotect(&b))
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default()
}

/// `#[serde(with = "crate::secret")]`: clear text in memory, sealed on disk
/// and in what the window receives.
pub fn serialize<S: Serializer>(value: &str, s: S) -> Result<S::Ok, S::Error> {
    seal(value).serialize(s)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(open(&String::deserialize(d)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_hides_the_text() {
        let sealed = seal("p@ss wörd");
        assert!(sealed.starts_with(PREFIX));
        assert!(!sealed.contains("p@ss"));
        assert_eq!(open(&sealed), "p@ss wörd");
        assert_eq!(seal(""), "");
        assert_eq!(open("garbage"), "");
    }
}
