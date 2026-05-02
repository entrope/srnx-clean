//! Parser for the `SRNX` chunk's payload — the file's master header.
//!
//! Per `docs/Succinct_RINEX.md` §SRNX, the payload is five ULEB128 values
//! followed by optional padding (which a reader MUST ignore):
//!
//! 1. major version (currently 1)
//! 2. minor version (currently 0)
//! 3. per-chunk digest identifier
//! 4. file-level digest identifier
//! 5. file offset of the `SDIR` chunk (0 if absent)

use thiserror::Error;

use crate::digest::{DigestError, DigestId};
use crate::leb128::{read_uleb128, LebError};

/// Highest major version this implementation understands.
pub const SUPPORTED_MAJOR: u64 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SrnxHeaderError {
    #[error("invalid LEB128 in SRNX header: {0}")]
    Leb(#[from] LebError),
    #[error("invalid digest identifier in SRNX header: {0}")]
    Digest(#[from] DigestError),
    #[error("unsupported SRNX major version {major} (this reader supports {SUPPORTED_MAJOR})")]
    UnsupportedMajor { major: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrnxHeader {
    pub major: u64,
    pub minor: u64,
    pub chunk_digest: DigestId,
    pub file_digest: DigestId,
    /// File offset of the `SDIR` chunk, or 0 if no SDIR is present.
    pub sdir_offset: u64,
}

/// Parse the payload of an `SRNX` chunk. Trailing padding is ignored.
pub fn parse(payload: &[u8]) -> Result<SrnxHeader, SrnxHeaderError> {
    let mut cursor: &[u8] = payload;
    let major = read_uleb128(&mut cursor)?;
    let minor = read_uleb128(&mut cursor)?;
    if major != SUPPORTED_MAJOR {
        return Err(SrnxHeaderError::UnsupportedMajor { major });
    }
    let chunk_digest = DigestId::from_raw(read_uleb128(&mut cursor)?)?;
    let file_digest = DigestId::from_raw(read_uleb128(&mut cursor)?)?;
    let sdir_offset = read_uleb128(&mut cursor)?;
    // Anything left in `cursor` is padding; the spec says "MUST be ignored".
    Ok(SrnxHeader {
        major,
        minor,
        chunk_digest,
        file_digest,
        sdir_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_header_no_padding() {
        // major=1, minor=0, chunk-digest=2 (CRC32C), file-digest=5 (BLAKE3-32),
        // sdir-offset=0.
        let bytes = [0x01, 0x00, 0x02, 0x05, 0x00];
        let h = parse(&bytes).unwrap();
        assert_eq!(h.major, 1);
        assert_eq!(h.minor, 0);
        assert_eq!(h.chunk_digest, DigestId::Crc32c);
        assert_eq!(h.file_digest, DigestId::Blake3_32);
        assert_eq!(h.sdir_offset, 0);
    }

    #[test]
    fn parse_ignores_trailing_padding() {
        // Same as above plus 8 bytes of zero padding (as in real fixtures).
        let bytes = [
            0x01, 0x00, 0x02, 0x05, 0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22,
        ];
        let h = parse(&bytes).unwrap();
        assert_eq!(h.sdir_offset, 0);
    }

    #[test]
    fn parse_multibyte_sdir_offset() {
        // sdir_offset = 25_668_721 (the value in at012000.srnx) =
        // ULEB128 [0xF1, 0xD8, 0x9E, 0x0C].
        let bytes = [0x01, 0x00, 0x02, 0x05, 0xF1, 0xD8, 0x9E, 0x0C];
        let h = parse(&bytes).unwrap();
        assert_eq!(h.sdir_offset, 25_668_721);
    }

    #[test]
    fn parse_rejects_unknown_major() {
        let bytes = [0x02, 0x00, 0x02, 0x05, 0x00];
        assert_eq!(
            parse(&bytes),
            Err(SrnxHeaderError::UnsupportedMajor { major: 2 })
        );
    }

    #[test]
    fn parse_rejects_unknown_digest() {
        // digest id 1 is not defined.
        let bytes = [0x01, 0x00, 0x01, 0x05, 0x00];
        assert!(matches!(
            parse(&bytes),
            Err(SrnxHeaderError::Digest(DigestError::Unknown(1)))
        ));
    }

    #[test]
    fn parse_truncated() {
        assert!(matches!(
            parse(&[0x01, 0x00]),
            Err(SrnxHeaderError::Leb(LebError::Truncated))
        ));
    }
}
