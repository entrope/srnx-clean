//! Parser for the `EVTF` (Event Flag / Special Record) chunk's payload.
//!
//! Per `docs/Succinct_RINEX.md` §EVTF, the payload is:
//!
//! 1. ULEB128 epoch index (0 = before first epoch, 1 = between epoch 0 and 1, …).
//! 2. Verbatim RINEX event record text, LF-terminated.
//!
//! The SDIR's `evtf_offset` field names the first `EVTF` chunk.  Chunks
//! SHOULD be stored consecutively, but need not be, so
//! [`SrnxFile::evtf_chunks`](crate::file::SrnxFile::evtf_chunks) reads every
//! `EVTF` chunk in file order.

use thiserror::Error;

use crate::leb128::{read_uleb128, LebError};

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvtfError {
    #[error("invalid LEB128 in EVTF chunk: {0}")]
    Leb(#[from] LebError),
    #[error("EVTF chunk truncated reading epoch index")]
    Truncated,
    #[error("EVTF text is not valid UTF-8")]
    NotUtf8,
}

/// A parsed EVTF chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evtf {
    /// Epoch index: 0 = before first epoch, 1 = between epoch 0 and 1, etc.
    pub epoch_index: u64,
    /// Verbatim event text (RINEX format, LF-terminated lines).
    /// Each element includes the trailing newline.
    pub text: Vec<String>,
}

/// Parse an `EVTF` chunk payload.
pub fn parse(payload: &[u8]) -> Result<Evtf, EvtfError> {
    let mut cursor: &[u8] = payload;
    let epoch_index = read_uleb128(&mut cursor)?;

    // The remainder is verbatim RINEX event text.  Split into lines on LF.
    let text_bytes = std::str::from_utf8(cursor).map_err(|_| EvtfError::NotUtf8)?;

    let text: Vec<String> = if text_bytes.is_empty() {
        Vec::new()
    } else {
        text_bytes.lines().map(|l| format!("{l}\n")).collect()
    };

    Ok(Evtf { epoch_index, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_event() {
        // epoch_index = 5, then two lines of event text.
        let payload = b"\x05line one\nline two\n";
        let evtf = parse(payload).unwrap();
        assert_eq!(evtf.epoch_index, 5);
        assert_eq!(evtf.text.len(), 2);
        assert_eq!(evtf.text[0], "line one\n");
        assert_eq!(evtf.text[1], "line two\n");
    }

    #[test]
    fn parse_empty_text() {
        // epoch_index = 0, no text.
        let payload = b"\x00";
        let evtf = parse(payload).unwrap();
        assert_eq!(evtf.epoch_index, 0);
        assert_eq!(evtf.text, [] as [String; 0]);
    }

    #[test]
    fn parse_epoch_index_zero() {
        // epoch_index = 0 means "before the first epoch".
        let payload = b"\x00event before first epoch\n";
        let evtf = parse(payload).unwrap();
        assert_eq!(evtf.epoch_index, 0);
        assert_eq!(evtf.text.len(), 1);
    }

    #[test]
    fn parse_large_epoch_index() {
        // epoch_index = 1000 (ULEB128: 0xE8 0x07).
        let payload = b"\xE8\x07event at epoch 1000\n";
        let evtf = parse(payload).unwrap();
        assert_eq!(evtf.epoch_index, 1000);
    }

    #[test]
    fn parse_not_utf8() {
        assert_eq!(parse(b"\x00caf\xe9\n"), Err(EvtfError::NotUtf8));
    }

    #[test]
    fn parse_truncated() {
        let payload = b"";
        assert!(parse(payload).is_err());
    }
}
