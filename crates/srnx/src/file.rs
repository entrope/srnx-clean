//! Top-level SRNX file reader.
//!
//! Owns the chicken-and-egg bootstrap that the chunk-framing layer cannot do
//! by itself: parse the `SRNX` header chunk to learn the per-chunk and
//! file-level digest IDs, verify both digests, and walk the rest of the file
//! recording every body chunk along with its absolute file offset. Per-chunk
//! payload parsing for `RHDR`, `EPOC`, `SATE`, `SOCD`, `EVTF` is the job of
//! later phases — this module only frames them.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::chunk::{read_chunk, read_framed, ChunkError};
use crate::digest::DigestError;
use crate::epoc::{self, Epoc, EpocError};
use crate::sdir::{self, Sdir, SdirError};
use crate::srnx_header::{self, SrnxHeader, SrnxHeaderError};

/// FOURCC tag for the SRNX header chunk.
pub const SRNX_TAG: [u8; 4] = *b"SRNX";
/// FOURCC tag for the RINEX header chunk.
pub const RHDR_TAG: [u8; 4] = *b"RHDR";
/// FOURCC tag for the satellite directory chunk.
pub const SDIR_TAG: [u8; 4] = *b"SDIR";
/// FOURCC tag for the epoch records chunk.
pub const EPOC_TAG: [u8; 4] = *b"EPOC";

#[derive(Debug, Error)]
pub enum FileError {
    #[error("chunk framing error: {0}")]
    Framing(#[from] ChunkError),
    #[error("SRNX header error: {0}")]
    Header(#[from] SrnxHeaderError),
    #[error("SDIR chunk error: {0}")]
    Sdir(#[from] SdirError),
    #[error("EPOC chunk error: {0}")]
    Epoc(#[from] EpocError),
    #[error("first chunk is {got:?}, expected SRNX")]
    NotSrnxFile { got: [u8; 4] },
    #[error("file truncated reading {what}")]
    Truncated { what: &'static str },
}

/// One body chunk (i.e. anything after the `SRNX` header chunk and before
/// the trailing file-level digest), plus its absolute byte offset within
/// the input.
#[derive(Debug, Clone, Copy)]
pub struct BodyChunk<'a> {
    pub fourcc: [u8; 4],
    pub payload: &'a [u8],
    /// Result of per-chunk digest verification (always `true` under
    /// `DigestId::Null`).
    pub digest_ok: bool,
    /// Offset of the chunk's FOURCC tag from the start of the file.
    pub file_offset: u64,
}

#[derive(Debug)]
pub struct SrnxFile<'a> {
    pub header: SrnxHeader,
    /// Per-chunk digest verification of the `SRNX` chunk itself.
    pub srnx_digest_ok: bool,
    /// File-level digest verification result.
    pub file_digest_ok: bool,
    /// All body chunks in file order.
    pub chunks: Vec<BodyChunk<'a>>,
}

impl<'a> SrnxFile<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, FileError> {
        // 1. Frame the SRNX chunk (digest length unknown at this point).
        let srnx_framed = read_framed(bytes)?;
        if srnx_framed.fourcc != SRNX_TAG {
            return Err(FileError::NotSrnxFile {
                got: srnx_framed.fourcc,
            });
        }
        let header = srnx_header::parse(srnx_framed.payload)?;

        // 2. Verify the SRNX chunk's per-chunk digest using the now-known ID.
        let chunk_dlen = header.chunk_digest.byte_len();
        if srnx_framed.after_payload.len() < chunk_dlen {
            return Err(FileError::Truncated {
                what: "SRNX chunk digest",
            });
        }
        let srnx_covered_len = bytes.len() - srnx_framed.after_payload.len();
        let srnx_covered = &bytes[..srnx_covered_len];
        let srnx_digest_bytes = &srnx_framed.after_payload[..chunk_dlen];
        let srnx_digest_ok = match header.chunk_digest.verify(srnx_covered, srnx_digest_bytes) {
            Ok(()) => true,
            Err(DigestError::Mismatch) => false,
            Err(DigestError::Truncated { .. } | DigestError::Unknown(_)) => unreachable!(),
        };

        // 3. Determine body bounds.
        let body_start = srnx_covered_len + chunk_dlen;
        let file_dlen = header.file_digest.byte_len();
        if bytes.len() < body_start + file_dlen {
            return Err(FileError::Truncated {
                what: "file-level digest",
            });
        }
        let body_end = bytes.len() - file_dlen;

        // 4. Verify file-level digest (covers everything between the SRNX
        //    chunk's end and the trailer).
        let file_digest_bytes = &bytes[body_end..];
        let file_covered = &bytes[body_start..body_end];
        let file_digest_ok = match header.file_digest.verify(file_covered, file_digest_bytes) {
            Ok(()) => true,
            Err(DigestError::Mismatch) => false,
            Err(DigestError::Truncated { .. } | DigestError::Unknown(_)) => unreachable!(),
        };

        // 5. Walk body chunks, recording absolute file offsets.
        let mut chunks = Vec::new();
        let mut cursor = file_covered;
        while !cursor.is_empty() {
            let consumed = file_covered.len() - cursor.len();
            let file_offset = u64::try_from(body_start + consumed)
                .expect("usize fits in u64 on every supported target");
            let (chunk, rest) = read_chunk(cursor, header.chunk_digest)?;
            chunks.push(BodyChunk {
                fourcc: chunk.fourcc,
                payload: chunk.payload,
                digest_ok: chunk.digest_ok,
                file_offset,
            });
            cursor = rest;
        }

        Ok(Self {
            header,
            srnx_digest_ok,
            file_digest_ok,
            chunks,
        })
    }

    /// Locate the body chunk whose FOURCC tag begins at `offset`. Linear
    /// scan; sufficient for the small number of named offsets that exist in
    /// any one file (currently SDIR + EPOC + first EVTF).
    #[must_use]
    pub fn chunk_at_offset(&self, offset: u64) -> Option<&BodyChunk<'a>> {
        self.chunks.iter().find(|c| c.file_offset == offset)
    }

    /// Convenience: the payload of the first `RHDR` chunk, if any. The spec
    /// requires `RHDR` to be the second chunk of the file, but we don't
    /// enforce that here — callers can check chunk ordering themselves if
    /// they care.
    #[must_use]
    pub fn rhdr_payload(&self) -> Option<&'a [u8]> {
        self.chunks
            .iter()
            .find(|c| c.fourcc == RHDR_TAG)
            .map(|c| c.payload)
    }

    /// Parse the `SDIR` chunk if present. Looks it up by the offset stored
    /// in the SRNX header (rather than scanning by FOURCC) so that
    /// `header.sdir_offset == 0` cleanly maps to `Ok(None)`.
    pub fn sdir(&self) -> Result<Option<Sdir>, FileError> {
        if self.header.sdir_offset == 0 {
            return Ok(None);
        }
        let chunk = self
            .chunk_at_offset(self.header.sdir_offset)
            .ok_or(FileError::Truncated {
                what: "SDIR chunk at advertised offset",
            })?;
        if chunk.fourcc != SDIR_TAG {
            return Err(FileError::Truncated {
                what: "SDIR chunk at advertised offset (FOURCC mismatch)",
            });
        }
        Ok(Some(sdir::parse(chunk.payload)?))
    }

    /// Parse the `EPOC` chunk if present. Located via the `SDIR` chunk's
    /// `epoc_offset`, mirroring the SRNX-spec discovery mechanism. Returns
    /// `Ok(None)` when there is no SDIR or no epochs.
    pub fn epoc(&self) -> Result<Option<Epoc>, FileError> {
        let Some(sdir) = self.sdir()? else {
            return Ok(None);
        };
        if sdir.epoc_offset == 0 {
            return Ok(None);
        }
        let chunk = self
            .chunk_at_offset(sdir.epoc_offset)
            .ok_or(FileError::Truncated {
                what: "EPOC chunk at advertised offset",
            })?;
        if chunk.fourcc != EPOC_TAG {
            return Err(FileError::Truncated {
                what: "EPOC chunk at advertised offset (FOURCC mismatch)",
            });
        }
        Ok(Some(epoc::parse(chunk.payload)?))
    }

    /// Histogram of body-chunk FOURCCs (does not include the SRNX chunk).
    #[must_use]
    pub fn count_by_fourcc(&self) -> BTreeMap<[u8; 4], usize> {
        let mut counts = BTreeMap::new();
        for c in &self.chunks {
            *counts.entry(c.fourcc).or_insert(0) += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path resolver for fixtures in `rnx-data/`.
    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(name)
    }

    fn try_load(name: &str) -> Option<Vec<u8>> {
        match std::fs::read(fixture(name)) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("skipping fixture test: {name} unavailable: {e}");
                None
            }
        }
    }

    /// Expected per-FOURCC body-chunk counts from `srnx_scan` reference output.
    struct Expected {
        rhdr: usize,
        epoc: usize,
        sdir: usize,
        evtf: usize,
        sate: usize,
        socd: usize,
    }

    fn check_counts(file: &SrnxFile<'_>, want: &Expected, name: &str) {
        let counts = file.count_by_fourcc();
        let g = |tag: &[u8; 4]| counts.get(tag).copied().unwrap_or(0);
        assert_eq!(g(b"RHDR"), want.rhdr, "{name}: RHDR count");
        assert_eq!(g(b"EPOC"), want.epoc, "{name}: EPOC count");
        assert_eq!(g(b"SDIR"), want.sdir, "{name}: SDIR count");
        assert_eq!(g(b"EVTF"), want.evtf, "{name}: EVTF count");
        assert_eq!(g(b"SATE"), want.sate, "{name}: SATE count");
        assert_eq!(g(b"SOCD"), want.socd, "{name}: SOCD count");
    }

    fn check_full(name: &str, want: &Expected) {
        let Some(bytes) = try_load(name) else { return };
        let file = SrnxFile::parse(&bytes).expect("parse should succeed");
        assert!(file.srnx_digest_ok, "{name}: SRNX-chunk digest mismatch");
        assert!(file.file_digest_ok, "{name}: file-level digest mismatch");
        for chunk in &file.chunks {
            assert!(
                chunk.digest_ok,
                "{name}: digest mismatch on body chunk at file_offset={}",
                chunk.file_offset
            );
        }
        check_counts(&file, want, name);

        // SDIR lookup via offset must succeed and contain the expected number
        // of satellites (= count of distinct SATE chunks).
        let sdir = file
            .sdir()
            .expect("sdir() should not error")
            .expect("SDIR present");
        assert_eq!(
            sdir.satellites.len(),
            want.sate,
            "{name}: SDIR satellite count should match SATE chunk count"
        );

        // EPOC offset from SDIR must point at an actual EPOC chunk.
        let epoc_chunk = file
            .chunk_at_offset(sdir.epoc_offset)
            .expect("SDIR.epoc_offset should resolve");
        assert_eq!(&epoc_chunk.fourcc, b"EPOC");

        // Every SATE offset in SDIR must resolve to a SATE chunk.
        for entry in &sdir.satellites {
            let c = file.chunk_at_offset(entry.sate_offset).unwrap_or_else(|| {
                panic!(
                    "{name}: SATE offset {} for sat {:?} did not resolve",
                    entry.sate_offset, entry.sat_id
                )
            });
            assert_eq!(&c.fourcc, b"SATE");
        }

        // RHDR payload should be reachable.
        assert!(file.rhdr_payload().is_some(), "{name}: RHDR missing");
    }

    #[test]
    fn at012000_srnx() {
        check_full(
            "at012000.srnx",
            &Expected {
                rhdr: 1,
                epoc: 1,
                sdir: 1,
                evtf: 0,
                sate: 73,
                socd: 705,
            },
        );
    }

    #[test]
    fn abmf_srnx() {
        // RINEX v3 fixture; chunk counts taken from `srnx_scan abmf.srnx`.
        let Some(bytes) = try_load("abmf.srnx") else {
            return;
        };
        let file = SrnxFile::parse(&bytes).expect("parse should succeed");
        assert!(file.srnx_digest_ok);
        assert!(file.file_digest_ok);
        // Sanity: parses, has RHDR, has SDIR.
        assert!(file.rhdr_payload().is_some());
        assert!(file.sdir().unwrap().is_some());
    }

    #[test]
    fn okcb1200_srnx() {
        let Some(bytes) = try_load("okcb1200.srnx") else {
            return;
        };
        let file = SrnxFile::parse(&bytes).expect("parse should succeed");
        assert!(file.srnx_digest_ok);
        assert!(file.file_digest_ok);
        assert!(file.rhdr_payload().is_some());
        assert!(file.sdir().unwrap().is_some());
    }

    #[test]
    fn sbst1200_srnx() {
        let Some(bytes) = try_load("sbst1200.srnx") else {
            return;
        };
        let file = SrnxFile::parse(&bytes).expect("parse should succeed");
        assert!(file.srnx_digest_ok);
        assert!(file.file_digest_ok);
        assert!(file.rhdr_payload().is_some());
        assert!(file.sdir().unwrap().is_some());
    }

    #[test]
    fn rejects_non_srnx_file() {
        // 4-byte FOURCC + 1-byte length + 0-byte payload + 0 padding.
        let bytes = [b'F', b'A', b'K', b'E', 0x00];
        let err = SrnxFile::parse(&bytes).unwrap_err();
        assert!(matches!(err, FileError::NotSrnxFile { got } if &got == b"FAKE"));
    }

    #[test]
    fn detects_tampered_file_digest() {
        let Some(mut bytes) = try_load("at012000.srnx") else {
            return;
        };
        // Flip a bit in the last byte (file-level digest).
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let file = SrnxFile::parse(&bytes).expect("parsing still succeeds");
        assert!(!file.file_digest_ok, "tampered file digest should fail");
    }
}
