//! Parser for the `SATE` (Satellite Observations) chunk payload.
//!
//! Each `SATE` chunk describes one satellite: which epochs it was present, and
//! the relative file offsets of each observation code's `SOCD` chunk.
//!
//! ## Payload layout
//!
//! 1. Satellite name as a null-terminated ASCII string (e.g. `b"G01\0"`).
//! 2. `num_obs_codes` × SLEB128 — offset of each `SOCD` chunk relative to the
//!    start of this `SATE` chunk's FOURCC tag. Zero means never observed.
//! 3. Epoch presence RLE (see below).
//!
//! ## Epoch presence encoding
//!
//! The presence data encodes which epochs (indices into the `EPOC` list) the
//! satellite was observed.  Layout:
//!
//! ```text
//! ULEB128  (num_runs - 1)
//! for each run:
//!   ULEB128  absent_count       — raw value, NOT count-minus-1; may be 0
//!   ULEB128  (present_count-1)
//! ```
//!
//! Each run advances the epoch cursor by `absent_count` (absent epochs) then
//! `present_count` (present epochs).  The cursor starts at 0.

use thiserror::Error;

use crate::leb128::{read_sleb128, read_uleb128, LebError};

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SateError {
    #[error("invalid LEB128 in SATE chunk: {0}")]
    Leb(#[from] LebError),
    #[error("SATE satellite name has no null terminator")]
    NoNullTerminator,
    #[error("SATE epoch presence overflow (more epochs than declared total)")]
    EpochOverflow,
}

/// Decoded content of one `SATE` chunk.
#[derive(Debug, Clone)]
pub struct Sate {
    /// Three-byte satellite identifier (e.g. `b"G01"`).
    pub sat_name: [u8; 3],
    /// SLEB128 offsets of the corresponding `SOCD` or `SOC2` chunk, relative to the
    /// start of this `SATE` chunk's FOURCC tag.  One entry per observation
    /// code (in the same order as the `RHDR` obs-code list).  Zero means this
    /// observation code was never recorded for this satellite.
    pub socd_offsets: Vec<i64>,
    /// Sorted epoch indices (0-based into the `EPOC` list) at which this
    /// satellite was observed. Length equals the observation chunk count.
    pub present_epoch_indices: Vec<usize>,
}

/// Parse only the observation chunk offsets of a `SATE` chunk payload,
/// skipping its epoch presence data.
pub fn parse_offsets(payload: &[u8], num_obs_codes: usize) -> Result<Vec<i64>, SateError> {
    let mut cursor = payload;
    read_name(&mut cursor)?;
    read_offsets(&mut cursor, num_obs_codes)
}

/// Null-terminated satellite name.
fn read_name(cursor: &mut &[u8]) -> Result<[u8; 3], SateError> {
    let null_pos = cursor
        .iter()
        .position(|&b| b == 0)
        .ok_or(SateError::NoNullTerminator)?;
    let mut name = [b' '; 3];
    let copy_len = null_pos.min(3);
    name[..copy_len].copy_from_slice(&cursor[..copy_len]);
    *cursor = &cursor[null_pos + 1..];
    Ok(name)
}

/// SLEB128 SOCD offsets, one per observation code.
fn read_offsets(cursor: &mut &[u8], num_obs_codes: usize) -> Result<Vec<i64>, SateError> {
    let mut socd_offsets = Vec::with_capacity(num_obs_codes);
    for _ in 0..num_obs_codes {
        socd_offsets.push(read_sleb128(cursor)?);
    }
    Ok(socd_offsets)
}

/// Parse a `SATE` chunk payload.
///
/// * `payload` — raw bytes of the chunk (after the FOURCC + length framing).
/// * `num_obs_codes` — number of observation codes declared in the `RHDR`.
/// * `total_epochs` — total epoch count from the `EPOC` chunk; presence runs
///   that extend past this bound are silently clamped.
pub fn parse(payload: &[u8], num_obs_codes: usize, total_epochs: usize) -> Result<Sate, SateError> {
    let mut cursor: &[u8] = payload;
    let sat_name = read_name(&mut cursor)?;
    let socd_offsets = read_offsets(&mut cursor, num_obs_codes)?;

    // Epoch presence RLE.
    let num_runs_m1 = read_uleb128(&mut cursor)?;
    let num_runs = usize::try_from(num_runs_m1.saturating_add(1)).unwrap_or(usize::MAX);

    let mut present_epoch_indices = Vec::new();
    let mut epoch_cursor: usize = 0;

    for _ in 0..num_runs {
        // absent_count is a raw ULEB128, NOT count-minus-1.
        let absent = usize::try_from(read_uleb128(&mut cursor)?).unwrap_or(usize::MAX);
        let present_m1 = read_uleb128(&mut cursor)?;
        let present = usize::try_from(present_m1)
            .unwrap_or(usize::MAX)
            .saturating_add(1);

        epoch_cursor = epoch_cursor
            .checked_add(absent)
            .ok_or(SateError::EpochOverflow)?;

        let span_end = epoch_cursor
            .checked_add(present)
            .ok_or(SateError::EpochOverflow)?;

        // Clamp to total_epochs — presence may extend just past the end.
        let actual_end = span_end.min(total_epochs);
        if epoch_cursor < actual_end {
            present_epoch_indices.extend(epoch_cursor..actual_end);
        }
        epoch_cursor = span_end;
    }

    Ok(Sate {
        sat_name,
        socd_offsets,
        present_epoch_indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_uleb(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn enc_sleb(v: i64) -> Vec<u8> {
        #[allow(clippy::cast_sign_loss)]
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        enc_uleb(zz)
    }

    /// Build a minimal SATE payload for testing.
    fn build(
        sat: [u8; 3],
        offsets: &[i64],
        runs: &[(u64, u64)], // (absent_raw, present_m1)
    ) -> Vec<u8> {
        let mut out = sat.to_vec();
        out.push(0); // null terminator
        for &off in offsets {
            out.extend(enc_sleb(off));
        }
        // num_runs_m1
        out.extend(enc_uleb(runs.len().saturating_sub(1) as u64));
        for &(absent, present_m1) in runs {
            out.extend(enc_uleb(absent));
            out.extend(enc_uleb(present_m1));
        }
        out
    }

    #[test]
    fn parse_satellite_present_from_start() {
        // Satellite present from epoch 0 for 3 epochs, then absent.
        // 1 run: absent=0, present_m1=2 → present=3
        let payload = build(*b"G01", &[-100i64, 0, -200], &[(0, 2)]);
        let sate = parse(&payload, 3, 10).unwrap();
        assert_eq!(&sate.sat_name, b"G01");
        assert_eq!(sate.socd_offsets, &[-100, 0, -200]);
        assert_eq!(sate.present_epoch_indices, &[0, 1, 2]);
    }

    #[test]
    fn parse_satellite_with_initial_gap() {
        // 2 absent, then 6 present (matching spec example but with fewer values).
        let payload = build(*b"G02", &[0i64; 0], &[(2, 5)]);
        let sate = parse(&payload, 0, 20).unwrap();
        assert_eq!(sate.present_epoch_indices, &[2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn parse_two_runs() {
        // 2 absent, 3 present, 4 absent, 2 present.
        let payload = build(*b"R10", &[], &[(2, 2), (4, 1)]);
        let sate = parse(&payload, 0, 20).unwrap();
        // Epoch cursor: 0 → skip 2 → present 3,4,5 → skip 4 → 10,11
        assert_eq!(sate.present_epoch_indices, &[2, 3, 4, 9, 10]);
    }

    #[test]
    fn parse_clamped_to_total_epochs() {
        // Presence span extends past total_epochs; should be clamped.
        let payload = build(*b"G01", &[], &[(0, 4)]);
        let sate = parse(&payload, 0, 3).unwrap();
        assert_eq!(sate.present_epoch_indices, &[0, 1, 2]);
    }

    #[test]
    fn parse_no_null_terminator_is_error() {
        // Only the satellite name bytes, no null.
        let payload = b"G01".to_vec();
        assert!(matches!(
            parse(&payload, 0, 0),
            Err(SateError::NoNullTerminator)
        ));
    }

    // ---- Fixture tests -------------------------------------------------------

    /// The first satellite's SATE payload, the number of observation codes,
    /// and the number of epochs in an SRNX fixture.
    fn first_sate(srnx_name: &str) -> (Vec<u8>, usize, usize) {
        let bytes = crate::test_data(srnx_name);
        let file = crate::file::SrnxFile::parse(&bytes).unwrap();
        let sdir = file.sdir().unwrap().unwrap();
        let sate_chunk = file
            .chunk_at_offset(sdir.satellites[0].sate_offset)
            .unwrap();
        let total_epochs = file.epoc().unwrap().unwrap().epochs.len();
        let header = crate::rinex_header::parse(file.rhdr_payload().unwrap()).unwrap();
        // For v2 mixed-system files there is one shared code list.
        let num_obs_codes = header.obs_codes.total_codes();
        (sate_chunk.payload.to_vec(), num_obs_codes, total_epochs)
    }

    #[test]
    fn fixture_at01_sate_e01() {
        let (payload, num_obs, total_epochs) = first_sate("at01_v211_1hz.srnx");
        // at01: 20 obs codes, 120 epochs, first sat is E01.
        assert_eq!(num_obs, 20);
        assert_eq!(total_epochs, 120);

        let sate = parse(&payload, num_obs, total_epochs).unwrap();
        assert_eq!(&sate.sat_name, b"E01");
        assert_eq!(sate.socd_offsets.len(), 20);
        // E01 is Galileo: L2/P2/P1/S2/C2 (GPS-only codes) should be absent.
        assert_ne!(sate.socd_offsets[0], 0, "L1 should be observed");
        assert_eq!(sate.socd_offsets[1], 0, "L2 not expected for Galileo E01");
        // E01 is in every epoch of the excerpt.
        assert_eq!(sate.present_epoch_indices, (0..120).collect::<Vec<_>>());
    }
}
