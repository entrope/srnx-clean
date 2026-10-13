//! Chunk-level framing for SRNX files.
//!
//! On-disk layout of each chunk (per `docs/Succinct_RINEX.md`):
//!
//! ```text
//! FOURCC (4)  ||  length (ULEB128)  ||  payload (length bytes)  ||  digest (N bytes)
//! ```
//!
//! The per-chunk digest length `N` is determined by the digest identifier
//! stored in the file's first chunk (the `SRNX` chunk's payload). That makes
//! the very first chunk a chicken-and-egg case: callers must parse the SRNX
//! payload before they can know how to frame any other chunk. See
//! [`read_framed`] (which ignores digests entirely) for the bootstrap path,
//! and [`ChunkIter`] for the steady-state iteration path used after the
//! digest ID is known.
//!
//! This module does not interpret FOURCCs — unknown chunk types are yielded
//! verbatim, and dispatching is left to higher layers.

use thiserror::Error;

use crate::compat::split_first_chunk;
use crate::digest::{DigestError, DigestId};
use crate::leb128::{read_uleb128, LebError};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChunkError {
    #[error("input truncated while reading {what}")]
    Truncated { what: &'static str },
    #[error("invalid chunk length encoding: {0}")]
    LengthEncoding(#[from] LebError),
    #[error("chunk payload length {length} exceeds remaining input")]
    PayloadOverflow { length: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct Chunk<'a> {
    pub fourcc: [u8; 4],
    pub payload: &'a [u8],
    /// Per-chunk digest verification result. Always `true` when the file
    /// uses `DigestId::Null`. `false` indicates a digest mismatch — the
    /// chunk's payload is still returned so callers can decide how strict
    /// to be.
    pub digest_ok: bool,
}

/// Result of [`read_framed`]: a chunk's FOURCC and payload, plus the bytes
/// that follow the payload (i.e. starting at the per-chunk digest field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framed<'a> {
    pub fourcc: [u8; 4],
    pub payload: &'a [u8],
    pub after_payload: &'a [u8],
}

/// Parse a chunk's framing (FOURCC + length + payload) without consuming any
/// per-chunk digest. Used to bootstrap the first chunk of a file before its
/// digest identifier is known.
pub fn read_framed(input: &[u8]) -> Result<Framed<'_>, ChunkError> {
    let (fourcc_arr, rest) =
        split_first_chunk::<4>(input).ok_or(ChunkError::Truncated { what: "FOURCC" })?;
    let mut cursor: &[u8] = rest;
    let length = read_uleb128(&mut cursor)?;
    let length_usize =
        usize::try_from(length).map_err(|_| ChunkError::PayloadOverflow { length })?;
    if cursor.len() < length_usize {
        return Err(ChunkError::PayloadOverflow { length });
    }
    let (payload, after_payload) = cursor.split_at(length_usize);
    Ok(Framed {
        fourcc: *fourcc_arr,
        payload,
        after_payload,
    })
}

/// Parse one chunk including its per-chunk digest. Returns the parsed chunk
/// and the bytes following the digest field.
pub fn read_chunk(input: &[u8], digest_id: DigestId) -> Result<(Chunk<'_>, &[u8]), ChunkError> {
    let framed = read_framed(input)?;
    let dlen = digest_id.byte_len();
    if framed.after_payload.len() < dlen {
        return Err(ChunkError::Truncated {
            what: "per-chunk digest",
        });
    }
    let (digest_bytes, rest) = framed.after_payload.split_at(dlen);

    let consumed_before_digest = input.len() - framed.after_payload.len();
    let covered = &input[..consumed_before_digest];

    let digest_ok = match digest_id.verify(covered, digest_bytes) {
        Ok(()) => true,
        Err(DigestError::Mismatch) => false,
        // We already verified the slice length; other errors are impossible.
        Err(DigestError::Truncated { .. } | DigestError::Unknown(_)) => unreachable!(),
    };

    Ok((
        Chunk {
            fourcc: framed.fourcc,
            payload: framed.payload,
            digest_ok,
        },
        rest,
    ))
}

/// Sequential iterator over chunks in a byte slice. The slice must NOT
/// include any trailing file-level digest; the caller is responsible for
/// trimming that off before constructing the iterator.
///
/// On the first error, the iterator yields `Some(Err(...))` and then `None`
/// on every subsequent call.
pub struct ChunkIter<'a> {
    remaining: &'a [u8],
    digest_id: DigestId,
    halted: bool,
}

impl<'a> ChunkIter<'a> {
    #[must_use]
    pub fn new(data: &'a [u8], digest_id: DigestId) -> Self {
        Self {
            remaining: data,
            digest_id,
            halted: false,
        }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> &'a [u8] {
        self.remaining
    }
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = Result<Chunk<'a>, ChunkError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.halted || self.remaining.is_empty() {
            return None;
        }
        match read_chunk(self.remaining, self.digest_id) {
            Ok((chunk, rest)) => {
                self.remaining = rest;
                Some(Ok(chunk))
            }
            Err(e) => {
                self.halted = true;
                self.remaining = &[];
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic chunk: FOURCC + ULEB128 length + payload.
    /// (Caller appends digest bytes separately.)
    fn frame(fourcc: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&fourcc);
        // Encode payload length as ULEB128 by hand for tests.
        let mut len = u64::try_from(payload.len()).unwrap();
        loop {
            let byte = (len & 0x7F) as u8;
            len >>= 7;
            if len == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn read_framed_basic() {
        let mut bytes = frame(*b"ABCD", b"hello");
        bytes.extend_from_slice(b"trailing");
        let f = read_framed(&bytes).unwrap();
        assert_eq!(&f.fourcc, b"ABCD");
        assert_eq!(f.payload, b"hello");
        assert_eq!(f.after_payload, b"trailing");
    }

    #[test]
    fn read_framed_truncated_fourcc() {
        assert_eq!(
            read_framed(b"ABC"),
            Err(ChunkError::Truncated { what: "FOURCC" })
        );
    }

    #[test]
    fn read_framed_truncated_payload() {
        // Length says 10 but only 3 payload bytes follow.
        let bytes = b"ABCD\x0Axyz";
        assert_eq!(
            read_framed(bytes),
            Err(ChunkError::PayloadOverflow { length: 10 })
        );
    }

    #[test]
    fn iter_with_null_digest() {
        let mut bytes = frame(*b"AAAA", b"one");
        bytes.extend(frame(*b"BBBB", b"two"));
        bytes.extend(frame(*b"CCCC", b""));
        let chunks: Vec<_> = ChunkIter::new(&bytes, DigestId::Null)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(&chunks[0].fourcc, b"AAAA");
        assert_eq!(chunks[0].payload, b"one");
        assert!(chunks[0].digest_ok);
        assert_eq!(&chunks[2].fourcc, b"CCCC");
        assert!(chunks[2].payload.is_empty());
    }

    #[test]
    fn iter_with_crc32c_verifies_and_detects_tampering() {
        let mut bytes = frame(*b"WXYZ", b"payload bytes");
        // Append the correct CRC32C of FOURCC+length+payload.
        let crc = crc32c::crc32c(&bytes);
        let crc_bytes = crc.to_le_bytes();
        bytes.extend_from_slice(&crc_bytes);

        // Add a second chunk with a *wrong* digest.
        let chunk2_start = bytes.len();
        bytes.extend(frame(*b"OOPS", b"corrupt me"));
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let chunks: Vec<_> = ChunkIter::new(&bytes, DigestId::Crc32c)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].digest_ok);
        assert!(!chunks[1].digest_ok);
        // Sanity: second chunk really did start where we expected it to.
        assert_eq!(&bytes[chunk2_start..chunk2_start + 4], b"OOPS");
    }

    #[test]
    fn iter_halts_after_error() {
        // First chunk is fine; second has a length that overruns the slice.
        let mut bytes = frame(*b"AAAA", b"ok");
        bytes.extend_from_slice(b"BBBB\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\x01"); // length = u64::MAX
        let mut iter = ChunkIter::new(&bytes, DigestId::Null);
        assert!(iter.next().unwrap().is_ok());
        assert!(matches!(
            iter.next(),
            Some(Err(ChunkError::PayloadOverflow { .. }))
        ));
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[test]
    fn iter_truncated_digest() {
        // Frame says payload is 1 byte, but no room for the 4-byte CRC.
        let bytes = frame(*b"AAAA", b"x");
        let mut iter = ChunkIter::new(&bytes, DigestId::Crc32c);
        assert!(matches!(
            iter.next(),
            Some(Err(ChunkError::Truncated {
                what: "per-chunk digest"
            }))
        ));
    }

    /// End-to-end check on a real SRNX file. We do the SRNX-chunk bootstrap
    /// by hand (Phase 3 will turn this into a proper API), then iterate the
    /// remainder and confirm chunk-type counts against the `srnx_scan`
    /// reference output.
    #[test]
    fn iter_real_at012000_srnx() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../rnx-data/at012000.srnx");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping fixture test: {path} unavailable");
            return;
        };

        // Bootstrap: read the SRNX chunk's framing, then parse its payload
        // by hand to learn the per-chunk and file-level digest IDs.
        let srnx = read_framed(&bytes).unwrap();
        assert_eq!(&srnx.fourcc, b"SRNX");

        let mut cursor: &[u8] = srnx.payload;
        let _major = read_uleb128(&mut cursor).unwrap();
        let _minor = read_uleb128(&mut cursor).unwrap();
        let chunk_digest = DigestId::from_raw(read_uleb128(&mut cursor).unwrap()).unwrap();
        let file_digest = DigestId::from_raw(read_uleb128(&mut cursor).unwrap()).unwrap();
        assert_eq!(chunk_digest, DigestId::Crc32c);
        assert_eq!(file_digest, DigestId::Blake3_32);

        // Skip the SRNX chunk's per-chunk digest, and trim the file-level
        // digest off the end before iterating.
        let dlen = chunk_digest.byte_len();
        let body_start = bytes.len() - srnx.after_payload.len() + dlen;
        let body_end = bytes.len() - file_digest.byte_len();
        let body = &bytes[body_start..body_end];

        let mut counts: std::collections::BTreeMap<[u8; 4], usize> =
            std::collections::BTreeMap::new();
        let mut tampered = 0_usize;
        for chunk in ChunkIter::new(body, chunk_digest) {
            let chunk = chunk.expect("chunk parse error in real fixture");
            *counts.entry(chunk.fourcc).or_default() += 1;
            if !chunk.digest_ok {
                tampered += 1;
            }
        }
        assert_eq!(tampered, 0, "real file should have all valid digests");

        // Expected counts come from the `srnx_scan` reference output for
        // this fixture (SRNX chunk itself isn't in `body`).
        assert_eq!(counts.get(b"RHDR").copied(), Some(1));
        assert_eq!(counts.get(b"EPOC").copied(), Some(1));
        assert_eq!(counts.get(b"SDIR").copied(), Some(1));
        assert_eq!(counts.get(b"SATE").copied(), Some(73));
        // Revision 1.1 fixture: observation data is in SOC2, not SOCD.
        assert_eq!(counts.get(b"SOC2").copied(), Some(705));
        assert!(!counts.contains_key(b"SOCD"));
        // No event chunks in this fixture.
        assert!(!counts.contains_key(b"EVTF"));
    }
}
