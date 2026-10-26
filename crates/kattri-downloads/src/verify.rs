//! Checksum verification of a committed download.
//!
//! Rules:
//! - Only a checksum that the server provided is used. No checksum is ever invented.
//! - The hash algorithm is the one the server named. A stronger one is never substituted.
//! - No server checksum means the download commits as [`Verification::Unverified`].
//! - A checksum that does not match is [`VerifyError::Mismatch`]; the caller fails the job.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha1::Sha1;
use sha2::{Digest, Sha256};

const READ_CHUNK: usize = 64 * 1024;

/// Algorithms this module can compute. MD5 is intentionally absent: no MD5 implementation
/// is in the workspace, and a server MD5 value is treated as "not provided" rather than
/// being compared with a different algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Algorithm {
    Sha1,
    Sha256,
}

/// A checksum the server reported for the original file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerChecksum {
    algorithm: Algorithm,
    hex: String,
}

impl ServerChecksum {
    /// Parses a server-provided algorithm name and lowercase or uppercase hex digest.
    /// Returns `None` for unknown algorithms (including MD5) or a malformed digest.
    pub fn parse(algorithm: &str, hex: &str) -> Option<Self> {
        let algorithm = match algorithm.to_ascii_lowercase().as_str() {
            "sha1" => Algorithm::Sha1,
            "sha256" => Algorithm::Sha256,
            _ => return None,
        };
        let expected_len = match algorithm {
            Algorithm::Sha1 => 40,
            Algorithm::Sha256 => 64,
        };
        let hex = hex.trim().to_ascii_lowercase();
        if hex.len() != expected_len || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self { algorithm, hex })
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }
}

/// Status of a committed file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verification {
    /// The server's checksum matched.
    Verified,
    /// The server gave no checksum; the file is committed without verification.
    Unverified,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("file checksum does not match the server's value")]
    Mismatch,
    #[error("could not read the file for checksum: {0}")]
    Io(#[from] io::Error),
}

/// Verifies `path` against the server checksum, streaming the file in 64 KiB chunks.
pub fn verify_file(
    path: &Path,
    expected: Option<&ServerChecksum>,
) -> Result<Verification, VerifyError> {
    let Some(expected) = expected else {
        return Ok(Verification::Unverified);
    };
    let actual = match expected.algorithm {
        Algorithm::Sha1 => hash_file::<Sha1>(path)?,
        Algorithm::Sha256 => hash_file::<Sha256>(path)?,
    };
    if actual == expected.hex {
        Ok(Verification::Verified)
    } else {
        Err(VerifyError::Mismatch)
    }
}

fn hash_file<D: Digest>(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = D::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.bin");
        std::fs::write(&path, bytes).expect("write");
        (dir, path)
    }

    // Known digests of b"abc".
    const SHA1_ABC: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn matching_server_checksum_verifies() {
        let (_dir, path) = file_with(b"abc");
        let sum = ServerChecksum::parse("SHA1", SHA1_ABC).expect("parse");
        assert_eq!(
            verify_file(&path, Some(&sum)).ok(),
            Some(Verification::Verified)
        );
        let sum = ServerChecksum::parse("sha256", SHA256_ABC).expect("parse");
        assert_eq!(
            verify_file(&path, Some(&sum)).ok(),
            Some(Verification::Verified)
        );
    }

    #[test]
    fn mismatching_server_checksum_fails() {
        let (_dir, path) = file_with(b"abd");
        let sum = ServerChecksum::parse("sha1", SHA1_ABC).expect("parse");
        assert!(matches!(
            verify_file(&path, Some(&sum)),
            Err(VerifyError::Mismatch)
        ));
    }

    #[test]
    fn absent_checksum_commits_as_unverified() {
        let (_dir, path) = file_with(b"anything");
        assert_eq!(
            verify_file(&path, None).ok(),
            Some(Verification::Unverified)
        );
    }

    #[test]
    fn md5_and_malformed_digests_are_not_accepted() {
        assert!(ServerChecksum::parse("md5", "900150983cd24fb0d6963f7d28e17f72").is_none());
        assert!(ServerChecksum::parse("sha1", "not-hex").is_none());
        assert!(ServerChecksum::parse("sha1", SHA256_ABC).is_none());
    }
}
