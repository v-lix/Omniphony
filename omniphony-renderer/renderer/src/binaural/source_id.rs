//! What a prepared file was made from: the identity of a SOFA file.
//!
//! A prepared room ([`super::brir::prepare_room`]) and a finished HRIR grid
//! ([`super::grid_cache`]) each carry the identity of the SOFA file they were
//! made from, so that a host can tell from them alone whether they are what
//! a file it is given would make, and the engine whether a grid serves the
//! set it is asked for. The identity is the file's length and the 64-bit
//! FNV-1a hash of its bytes: one pass over bytes that are read anyway, the
//! same on every build and platform, and with the length as good as unique
//! among a listener's files. It is not a cryptographic digest, and does not
//! guard against a file forged to collide.

use std::fmt;

/// The identity of a SOFA file's bytes (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId {
    /// The file's length, bytes.
    pub len: u64,
    /// The 64-bit FNV-1a hash of its bytes.
    pub hash: u64,
}

impl SourceId {
    /// Bytes [`Self::to_bytes`] writes.
    pub const BYTES: usize = 16;

    /// The identity of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h: u64, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        Self {
            len: bytes.len() as u64,
            hash,
        }
    }

    /// Little-endian: the length, then the hash.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..].copy_from_slice(&self.hash.to_le_bytes());
        out
    }

    /// What [`Self::to_bytes`] wrote.
    pub fn from_bytes(bytes: [u8; Self::BYTES]) -> Self {
        let (len, hash) = bytes.split_at(8);
        Self {
            len: u64::from_le_bytes(len.try_into().expect("8 bytes")),
            hash: u64::from_le_bytes(hash.try_into().expect("8 bytes")),
        }
    }
}

/// `<length>-<hash, 16 hex digits>`: how a host compares two identities.
impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:016x}", self.len, self.hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published FNV-1a 64 test vectors, and a round trip.
    #[test]
    fn is_the_length_and_the_fnv1a_hash() {
        assert_eq!(SourceId::of(b"").hash, 0xcbf2_9ce4_8422_2325);
        assert_eq!(SourceId::of(b"a").hash, 0xaf63_dc4c_8601_ec8c);
        assert_eq!(SourceId::of(b"foobar").hash, 0x8594_4171_f739_67e8);
        let id = SourceId::of(b"foobar");
        assert_eq!(id.len, 6);
        assert_eq!(id.to_string(), "6-85944171f73967e8");
        assert_eq!(SourceId::from_bytes(id.to_bytes()), id);
        assert_ne!(SourceId::of(b"foobaR"), id);
    }
}
