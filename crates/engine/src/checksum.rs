//! File checksums (MD5 / SHA-256) for verifying finished downloads.

use std::io::Read;
use std::path::Path;

use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algo {
    Md5,
    Sha256,
}

impl Algo {
    /// Guess the algorithm from a published hex digest's length.
    pub fn for_digest(hex: &str) -> Option<Self> {
        let hex = hex.trim();
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        match hex.len() {
            32 => Some(Algo::Md5),
            64 => Some(Algo::Sha256),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "MD5",
            Algo::Sha256 => "SHA-256",
        }
    }
}

/// Lower-case hex digest of the file. Blocking; run it off the async runtime.
pub fn hash_file(path: &Path, algo: Algo) -> std::io::Result<String> {
    fn run<D: Digest>(path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = D::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
    match algo {
        Algo::Md5 => run::<Md5>(path),
        Algo::Sha256 => run::<Sha256>(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_algorithm_by_length() {
        assert_eq!(Algo::for_digest("d41d8cd98f00b204e9800998ecf8427e"), Some(Algo::Md5));
        assert_eq!(Algo::for_digest(&"A".repeat(64)), Some(Algo::Sha256));
        assert_eq!(Algo::for_digest("xyz"), None);
        assert_eq!(Algo::for_digest(&"g".repeat(32)), None);
    }

    #[test]
    fn hashes_known_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(hash_file(&path, Algo::Md5).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hash_file(&path, Algo::Sha256).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
