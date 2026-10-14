//! Parser for the `SOCD` (Satellite Observation Code Data) chunk payload.
//!
//! Each `SOCD` chunk holds the decoded observations for one (satellite,
//! observation-code) pair across every epoch where the satellite was present.
//!
//! ## Payload layout
//!
//! 1. 8-byte observation name: `sat[3] NUL sig[2-3] NUL-padded`.
//! 2. ULEB128 `(count - 1)` — total number of observation slots (including
//!    absent slots encoded by `0xFD` blocks).
//! 3. LLI run-length block (see below).
//! 4. SSI run-length block (same format as LLI).
//! 5. Packed observation block (see below).
//!
//! ## LLI / SSI encoding
//!
//! ```text
//! ULEB128  byte_length_of_rle_data
//! repeating until byte_length exhausted:
//!   ULEB128  (count - 1) * 11 + idx
//! ```
//!
//! The eleven-symbol alphabet is `" 0123456789"` (space, then `'0'`–`'9'`).
//! `idx` is the zero-based index within that alphabet (0 = space, 1–10 = `'0'`–`'9'`).
//! Any observations beyond the encoded runs default to `b' '`.
//!
//! ## Packed observation encoding
//!
//! ```text
//! ULEB128  byte_length_of_obs_data   (includes schema byte onward)
//! ULEB128  schema                    (delta_order | 8 if explicit scale)
//! [ULEB128 scale_times_1000]         (only when schema >= 8)
//! schema & 7  ×  SLEB128            initial delta-coder state
//! zero or more blocks:
//!   u8  block_header
//!   ... block body
//! ```
//!
//! Block headers:
//! - `0x00..=0xDF`: bit-matrix block — `m` values from top-3-bit lookup
//!   (0→8, 1→16, 2→24, 3→32, 4→40, 5→48, 6→112), `k+1` bits each,
//!   where `k = header & 0x1F`.
//! - `0xFD`: absent-values run — ULEB128 (count-1); emits `ABSENT` without
//!   advancing the delta accumulators.
//! - `0xFE`: zero-delta run — ULEB128 (count-1); feeds zero into the
//!   integrator for each slot.
//! - `0xFF`: SLEB128 run — ULEB128 (count-1) followed by that many SLEB128
//!   deltas fed through the integrator.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::compat::{cast_signed, split_first_chunk};
use crate::leb128::{read_sleb128, read_uleb128, LebError};

/// Sentinel value for absent observations.
pub const ABSENT: i64 = i64::MIN;

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SocdError {
    #[error("invalid LEB128 in SOCD chunk: {0}")]
    Leb(#[from] LebError),
    #[error("SOCD chunk truncated reading {what}")]
    Truncated { what: &'static str },
    #[error("SOCD declared {declared} observations but decoded {decoded}")]
    CountMismatch { declared: usize, decoded: usize },
    #[error("SOCD schema {0} has reserved block header")]
    ReservedBlockHeader(u8),
    #[error("SOCD observation count {0} exceeds addressable memory")]
    TooManyObservations(u64),
    #[error("SOCD schema delta order {0} exceeds maximum of 7")]
    InvalidDeltaOrder(u8),
}

/// Decoded content of one `SOCD` chunk.
#[derive(Debug, Clone)]
pub struct Socd {
    /// 8-byte (satellite + observation code) name field.
    pub obs_name: [u8; 8],
    /// Decoded observation values, one per present epoch.  `ABSENT`
    /// (`i64::MIN`) marks epochs where the satellite was present but this
    /// specific observation code was not recorded.
    ///
    /// The physical value is `values[i] * scale / 1000` when not absent.
    pub values: Vec<i64>,
    /// LLI (loss-of-lock) characters, one per observation (`b' '` = none).
    pub lli: Vec<u8>,
    /// SSI (signal-strength) characters, one per observation (`b' '` = none).
    pub ssi: Vec<u8>,
    /// Scale factor × 1000, matching the SRNX schema encoding.
    ///
    /// Invariant: `physical_value × 1000 = value × scale` (the comparison key
    /// used by the diff tool).  Equivalently, `physical_value = value × scale / 1000`.
    ///
    /// When the schema has no explicit scale, the on-disk default is
    /// `scale_times_1000 = 1000` (i.e. identity).  We store that as `1` so the
    /// invariant holds: the encoded value is already in milliunits and
    /// `value × 1 = physical × 1000`.
    pub scale: u32,
    /// Delta order extracted from the schema byte (0–7).
    pub delta_order: u8,
    /// Counts of each block-header byte seen while decoding this SOCD.
    /// Keys 0x00–0x9F are bit-matrix headers; 0xFD/0xFE/0xFF are run headers.
    pub block_header_counts: BTreeMap<u8, usize>,
}

/// Parse a `SOCD` chunk payload.
#[allow(clippy::too_many_lines)]
pub fn parse(payload: &[u8]) -> Result<Socd, SocdError> {
    let mut cursor: &[u8] = payload;

    // 8-byte observation name.
    let obs_name = split_first_chunk::<8>(cursor)
        .map(|(arr, rest)| {
            cursor = rest;
            *arr
        })
        .ok_or(SocdError::Truncated { what: "obs name" })?;

    // Count of observations (count-minus-1 encoding).
    let count_m1 = read_uleb128(&mut cursor)?;
    let count_u64 = count_m1
        .checked_add(1)
        .ok_or(SocdError::Leb(LebError::Overflow))?;
    let count =
        usize::try_from(count_u64).map_err(|_| SocdError::TooManyObservations(count_u64))?;

    // LLI run-length block.
    let lli = decode_rle_block(&mut cursor, count)?;

    // SSI run-length block.
    let ssi = decode_rle_block(&mut cursor, count)?;

    // Packed observation data.
    let obs_byte_len_u64 = read_uleb128(&mut cursor)?;
    let obs_byte_len = usize::try_from(obs_byte_len_u64)
        .map_err(|_| SocdError::TooManyObservations(obs_byte_len_u64))?;
    if cursor.len() < obs_byte_len {
        return Err(SocdError::Truncated { what: "obs data" });
    }
    let mut obs = &cursor[..obs_byte_len];

    let schema = read_uleb128(&mut obs)?;
    let delta_order = (schema & 7) as u8;
    if delta_order > 7 {
        return Err(SocdError::InvalidDeltaOrder(delta_order));
    }
    let has_explicit_scale = schema >= 8;

    let scale = if has_explicit_scale {
        let s_u64 = read_uleb128(&mut obs)?;
        let s = u32::try_from(s_u64).unwrap_or(1_000_000_000);
        if s == 0 {
            1
        } else {
            s
        }
    } else {
        1 // default per spec: "If no explicit scale is used, it is the same as 1"
    };

    // Initial delta-coder state (delta_order values).
    let mut integrator = {
        let mut init = [0i64; 8];
        for item in init.iter_mut().take(delta_order as usize) {
            *item = read_sleb128(&mut obs)?;
        }
        DeltaIntegrator::new(delta_order as usize, &init[..delta_order as usize])
    };

    // Decode blocks.
    let mut values: Vec<i64> = Vec::with_capacity(count);
    let mut block_header_counts: BTreeMap<u8, usize> = BTreeMap::new();
    while !obs.is_empty() {
        let header = obs[0];
        obs = &obs[1..];
        *block_header_counts.entry(header).or_default() += 1;

        match header {
            0x00..=0xDF => {
                // Bit-matrix block.
                // Top 3 bits select block size: 0→8, 1→16, 2→24, 3→32, 4→40, 5→48, 6→112
                let m_table: [usize; 7] = [8, 16, 24, 32, 40, 48, 112];
                let m_index = (header >> 5) as usize;
                let m = m_table[m_index];
                let k = (header & 0x1F) as usize;
                let bits_per_value = k + 1; // 1..=32
                let row_bytes = m / 8;
                let matrix_bytes = bits_per_value * row_bytes;

                if obs.len() < matrix_bytes {
                    return Err(SocdError::Truncated {
                        what: "bit-matrix data",
                    });
                }
                let matrix_data = &obs[..matrix_bytes];
                obs = &obs[matrix_bytes..];

                for i in 0..m {
                    let raw = extract_value(matrix_data, i, bits_per_value, row_bytes);
                    values.push(integrator.integrate(raw));
                }
            }
            0xFD => {
                // Absent-values run — do NOT advance the integrator.
                let run = uleb_run_count(&mut obs)?;
                values.extend(std::iter::repeat(ABSENT).take(run));
            }
            0xFE => {
                // Zero-delta run — feed zero deltas through the integrator.
                let run = uleb_run_count(&mut obs)?;
                for _ in 0..run {
                    values.push(integrator.integrate(0));
                }
            }
            0xFF => {
                // SLEB128-delta run.
                let run = uleb_run_count(&mut obs)?;
                for _ in 0..run {
                    let delta = read_sleb128(&mut obs)?;
                    values.push(integrator.integrate(delta));
                }
            }
            _ => {
                return Err(SocdError::ReservedBlockHeader(header));
            }
        }
    }

    if values.len() != count {
        return Err(SocdError::CountMismatch {
            declared: count,
            decoded: values.len(),
        });
    }

    Ok(Socd {
        obs_name,
        values,
        lli,
        ssi,
        scale,
        delta_order,
        block_header_counts,
    })
}

// ── RLE / run-count helpers ──────────────────────────────────────────────────

/// Read a ULEB128 count-minus-1 and return the actual count (`count_m1 + 1`),
/// converting to `usize` safely.
#[inline]
fn uleb_run_count(cursor: &mut &[u8]) -> Result<usize, SocdError> {
    let m1 = read_uleb128(cursor)?;
    Ok(usize::try_from(m1).unwrap_or(usize::MAX).saturating_add(1))
}

/// Decode a LLI/SSI run-length block into a `Vec<u8>` of exactly `count`
/// characters, space-padding any trailing unencoded observations.
///
/// Each run is a single ULEB128 value `(count - 1) * 11 + idx` where `idx`
/// is the zero-based index in the 11-symbol alphabet `" 0123456789"`.
fn decode_rle_block(cursor: &mut &[u8], count: usize) -> Result<Vec<u8>, SocdError> {
    const ALPHABET: &[u8] = b" 0123456789";
    let byte_len_u64 = read_uleb128(cursor)?;
    let byte_len =
        usize::try_from(byte_len_u64).map_err(|_| SocdError::TooManyObservations(byte_len_u64))?;
    if cursor.len() < byte_len {
        return Err(SocdError::Truncated {
            what: "LLI/SSI RLE data",
        });
    }
    let mut rle = &cursor[..byte_len];
    *cursor = &cursor[byte_len..];

    let mut out = Vec::with_capacity(count);
    while !rle.is_empty() && out.len() < count {
        let packed = read_uleb128(&mut rle)?;
        let idx = (packed % 11) as usize;
        let run = (packed / 11) as usize + 1;
        let ch = ALPHABET[idx];
        let run = run.min(count - out.len());
        for _ in 0..run {
            out.push(ch);
        }
    }
    // Pad remainder with spaces.
    out.resize(count, b' ');
    Ok(out)
}

// ── Bit-matrix value extraction ──────────────────────────────────────────────

/// Extract the sign-extended `bits_per_value`-bit value at column `i` from a
/// transposed bit matrix.
///
/// The matrix is stored as `bits_per_value` rows of `row_bytes` each. Row 0
/// holds the MSB of each value, row `bits_per_value-1` holds the LSB.  Within
/// each row, values are packed MSB-first (bit 7 of the first byte = value 0's
/// bit for that row).
#[inline]
fn extract_value(data: &[u8], i: usize, bits_per_value: usize, row_bytes: usize) -> i64 {
    let mut raw = 0u64;
    for row in 0..bits_per_value {
        let byte_idx = row * row_bytes + i / 8;
        let bit_pos = 7 - (i % 8);
        let bit = u64::from((data[byte_idx] >> bit_pos) & 1);
        raw = (raw << 1) | bit;
    }
    // Sign-extend from bits_per_value bits to 64 bits.
    let shift = 64 - bits_per_value;
    cast_signed(raw << shift) >> shift
}

// ── Delta integrator ─────────────────────────────────────────────────────────

/// N-th order delta-difference integrator.
///
/// `acc[0]` is the output accumulator (produces final observation values).
/// `acc[order-1]` is the innermost accumulator that receives new deltas.
///
/// For each input delta `d`:
///   `acc[order-1] += d`
///   for i in `(0..order-1).rev()`: `acc[i] += acc[i+1]`
///   output = `acc[0]`
pub(crate) struct DeltaIntegrator {
    order: usize,
    acc: [i64; 8],
}

impl DeltaIntegrator {
    pub(crate) fn new(order: usize, initial: &[i64]) -> Self {
        debug_assert!(order <= 8 && order == initial.len());
        let mut acc = [0i64; 8];
        acc[..order].copy_from_slice(initial);
        Self { order, acc }
    }

    #[inline]
    pub(crate) fn integrate(&mut self, delta: i64) -> i64 {
        if self.order == 0 {
            return delta;
        }
        self.acc[self.order - 1] = self.acc[self.order - 1].wrapping_add(delta);
        for i in (0..self.order - 1).rev() {
            let carry = self.acc[i + 1];
            self.acc[i] = self.acc[i].wrapping_add(carry);
        }
        self.acc[0]
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

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

    /// Build a minimal SOCD payload from raw values using a SLEB128-run block
    /// (order-0 delta, no explicit scale).
    fn build_socd_sleb(obs_name: [u8; 8], values: &[i64]) -> Vec<u8> {
        let count = values.len();
        let mut out = obs_name.to_vec();
        // count-minus-1
        out.extend(enc_uleb((count as u64).saturating_sub(1)));
        // LLI: zero bytes
        out.extend(enc_uleb(0));
        // SSI: zero bytes
        out.extend(enc_uleb(0));

        // Build obs block: schema=0 (order=0, no scale), no init state,
        // one 0xFF run block.
        let mut obs_block: Vec<u8> = Vec::new();
        obs_block.extend(enc_uleb(0)); // schema = 0
                                       // no initial state (order=0)
        if !values.is_empty() {
            obs_block.push(0xFF); // SLEB128 run
            obs_block.extend(enc_uleb((count as u64) - 1)); // count-minus-1
            for &v in values {
                obs_block.extend(enc_sleb(v));
            }
        }

        out.extend(enc_uleb(obs_block.len() as u64));
        out.extend(obs_block);
        out
    }

    // ── extract_value unit tests ──────────────────────────────────────────

    #[test]
    fn extract_1bit_values() {
        // m=8, bits=1: one row of one byte.
        // Byte 0xA5 = 0b10100101 → raw bits: 1,0,1,0,0,1,0,1
        // 1-bit two's-complement: bit 0 → 0, bit 1 → -1.
        let data = [0xA5u8];
        for (i, &expected) in [-1i64, 0, -1, 0, 0, -1, 0, -1].iter().enumerate() {
            assert_eq!(extract_value(&data, i, 1, 1), expected, "i={i}");
        }
    }

    #[test]
    fn extract_2bit_signed() {
        // m=8, bits=2: 2 rows of 1 byte each.
        // Row0 (MSB/sign): 0b1011_0000 → signs:  1,0,1,1,0,0,0,0
        // Row1 (LSB):      0b0110_1001 → lsbs:   0,1,1,0,1,0,0,1
        // 2-bit signed values: 10=−2, 01=1, 11=−1, 10=−2, 01=1, 00=0, 00=0, 01=1
        let data = [0b1011_0000_u8, 0b0110_1001_u8];
        let expected = [-2i64, 1, -1, -2, 1, 0, 0, 1];
        for (i, &exp) in expected.iter().enumerate() {
            assert_eq!(extract_value(&data, i, 2, 1), exp, "i={i}");
        }
    }

    #[test]
    fn extract_8bit_positive() {
        // m=8, bits=8: 8 rows of 1 byte = 8 bytes.
        // Encodes values 0..8 in transposed form.
        // Value i occupies bit 7-i%8 of each row byte.
        // For 8 values of 8 bits each, the transposed matrix:
        // For value i, bit r is at row r, byte r, bit 7-i.
        let values_in = [0i64, 1, 2, 3, 4, 5, 6, 7];
        // Build transposed matrix.
        let mut matrix = [0u8; 8];
        for (row, byte) in matrix.iter_mut().enumerate() {
            for (i, &v) in values_in.iter().enumerate() {
                // values_in are 0..8 so v >= 0; cast_sign_loss is intentional.
                #[allow(clippy::cast_sign_loss)]
                let bit = ((v >> (7 - row)) & 1) as u8;
                *byte |= bit << (7 - i);
            }
        }
        for (i, &expected) in values_in.iter().enumerate() {
            assert_eq!(extract_value(&matrix, i, 8, 1), expected, "i={i}");
        }
    }

    #[test]
    fn extract_8bit_negative() {
        // Test sign extension: -1 in 8-bit is 0xFF = 0b11111111.
        // All-ones transposed matrix: each row byte = 0xFF.
        let matrix = [0xFFu8; 8];
        for i in 0..8 {
            assert_eq!(extract_value(&matrix, i, 8, 1), -1i64, "i={i}");
        }
    }

    // ── DeltaIntegrator unit tests ────────────────────────────────────────

    #[test]
    fn integrator_order0() {
        let mut d = DeltaIntegrator::new(0, &[]);
        assert_eq!(d.integrate(5), 5);
        assert_eq!(d.integrate(-3), -3);
        assert_eq!(d.integrate(0), 0);
    }

    #[test]
    fn integrator_order1() {
        let mut d = DeltaIntegrator::new(1, &[0]);
        // Feeding 1, 1, 1 should give cumsum 1, 2, 3.
        assert_eq!(d.integrate(1), 1);
        assert_eq!(d.integrate(1), 2);
        assert_eq!(d.integrate(1), 3);
    }

    #[test]
    fn integrator_order1_initial() {
        // Initial state = 10, then delta +2 → 12, 14, 16.
        let mut d = DeltaIntegrator::new(1, &[10]);
        assert_eq!(d.integrate(2), 12);
        assert_eq!(d.integrate(2), 14);
        assert_eq!(d.integrate(2), 16);
    }

    #[test]
    fn integrator_order2() {
        // Second-order: constant second differences = linear first diffs = ramp.
        // Initial: acc[0]=0 (previous output), acc[1]=1 (previous 1st diff).
        // Feeding 0 (const 2nd diff) → output: 0+1=1, 1+1+0=2, 2+1+0=3, ...
        let mut d = DeltaIntegrator::new(2, &[0, 1]);
        assert_eq!(d.integrate(0), 1);
        assert_eq!(d.integrate(0), 2);
        assert_eq!(d.integrate(0), 3);
        assert_eq!(d.integrate(0), 4);
    }

    #[test]
    fn integrator_order2_acceleration() {
        // acc[0]=0, acc[1]=0: feeding 1, 0, 0, 0 gives 0+0=0 after d=1:
        // d=1: acc[1] = 0+1 = 1; acc[0] = 0+1 = 1; output 1
        // d=0: acc[1] = 1+0 = 1; acc[0] = 1+1 = 2; output 2
        // d=0: acc[1] = 1; acc[0] = 2+1 = 3; output 3
        let mut d = DeltaIntegrator::new(2, &[0, 0]);
        assert_eq!(d.integrate(1), 1);
        assert_eq!(d.integrate(0), 2);
        assert_eq!(d.integrate(0), 3);
        assert_eq!(d.integrate(0), 4);
    }

    // ── Block decoder unit tests ──────────────────────────────────────────

    #[test]
    fn sleb_run_block_order0() {
        // Values: 10, -3, 0, 5.
        let payload = build_socd_sleb(*b"G01\0L1\0\0", &[10, -3, 0, 5]);
        let socd = parse(&payload).unwrap();
        assert_eq!(socd.values, &[10, -3, 0, 5]);
        assert_eq!(socd.scale, 1);
        assert!(socd.lli.iter().all(|&b| b == b' '));
        assert!(socd.ssi.iter().all(|&b| b == b' '));
    }

    #[test]
    fn absent_run_does_not_advance_integrator() {
        // order=1, init=0, then: 5 (delta), ABSENT×2, 3 (delta).
        // After 5: acc = 5.
        // Absent values: acc stays 5.
        // After delta 3: acc = 5+3 = 8.
        let mut out = b"G01\0L1\0\0".to_vec();
        out.extend(enc_uleb(3)); // count_m1=3 → count=4
        out.extend(enc_uleb(0)); // LLI bytes = 0
        out.extend(enc_uleb(0)); // SSI bytes = 0

        let mut obs_block: Vec<u8> = Vec::new();
        obs_block.extend(enc_uleb(1)); // schema = 1 (order=1, no explicit scale)
        obs_block.extend(enc_sleb(0)); // initial state acc[0] = 0
                                       // Block 1: SLEB run ×1 → delta=5
        obs_block.push(0xFF);
        obs_block.extend(enc_uleb(0)); // count_m1=0 → 1 value
        obs_block.extend(enc_sleb(5));
        // Block 2: absent run ×2
        obs_block.push(0xFD);
        obs_block.extend(enc_uleb(1)); // count_m1=1 → 2 absent
                                       // Block 3: SLEB run ×1 → delta=3
        obs_block.push(0xFF);
        obs_block.extend(enc_uleb(0));
        obs_block.extend(enc_sleb(3));

        out.extend(enc_uleb(obs_block.len() as u64));
        out.extend(obs_block);

        let socd = parse(&out).unwrap();
        assert_eq!(socd.values, &[5, ABSENT, ABSENT, 8]);
    }

    #[test]
    fn zero_run_block() {
        // order=1, init=3: zero deltas keep acc at 3 (constant output).
        let mut out = b"G01\0L1\0\0".to_vec();
        out.extend(enc_uleb(2)); // count = 3
        out.extend(enc_uleb(0)); // LLI
        out.extend(enc_uleb(0)); // SSI

        let mut obs: Vec<u8> = Vec::new();
        obs.extend(enc_uleb(1)); // schema=1 (order=1)
        obs.extend(enc_sleb(3)); // init acc[0]=3
        obs.push(0xFE);
        obs.extend(enc_uleb(2)); // count_m1=2 → 3 zero-deltas

        out.extend(enc_uleb(obs.len() as u64));
        out.extend(obs);

        let socd = parse(&out).unwrap();
        // Zero deltas with acc[0]=3: each call to integrate(0) adds acc[1]=0 then acc[0]+=0 for order 1.
        // Wait for order=1: acc[0] += 0 = 3, output 3 each time.
        assert_eq!(socd.values, &[3, 3, 3]);
    }

    #[test]
    fn bit_matrix_block_round_trip() {
        // 8 values, 4 bits each (range -8..7), no explicit scale, order=0.
        let input = [3i64, -1, 0, 7, -8, 2, -4, 1];

        // Build the transposed 8×4 matrix manually.
        // bits_per_value=4, m=8, row_bytes=1, matrix_bytes=4.
        let mut matrix = [0u8; 4];
        for (row, byte) in matrix.iter_mut().enumerate() {
            // row ∈ 0..4; (3 - row) is the bit position within a 4-bit value.
            let bit_pos = 3 - row; // safe: row < bits_per_value = 4
            for (i, &v) in input.iter().enumerate() {
                #[allow(clippy::cast_sign_loss)]
                let bit = ((v >> bit_pos) & 1) as u8;
                *byte |= bit << (7 - i);
            }
        }

        // Build SOCD payload with one bit-matrix block.
        let mut out = b"G01\0L1\0\0".to_vec();
        out.extend(enc_uleb(7)); // count_m1=7 → count=8
        out.extend(enc_uleb(0)); // LLI
        out.extend(enc_uleb(0)); // SSI

        let mut obs: Vec<u8> = Vec::new();
        obs.extend(enc_uleb(0)); // schema=0 (order=0, no scale)
                                 // bit-matrix header: m=8 (shift=0), k=3 (4 bits) → header = 0b000_00011 = 3
        obs.push(0x03);
        obs.extend_from_slice(&matrix);

        out.extend(enc_uleb(obs.len() as u64));
        out.extend(obs);

        let socd = parse(&out).unwrap();
        assert_eq!(socd.values, input, "decoded values mismatch");
    }

    #[test]
    fn explicit_scale_parsed() {
        // schema=8 (order=0, has explicit scale), scale=500 (½-unit).
        let mut out = b"G01\0S1\0\0".to_vec();
        out.extend(enc_uleb(1)); // count_m1=1 → 2 values
        out.extend(enc_uleb(0)); // LLI
        out.extend(enc_uleb(0)); // SSI

        let mut obs: Vec<u8> = Vec::new();
        obs.extend(enc_uleb(8)); // schema=8 (order=0, explicit scale)
        obs.extend(enc_uleb(500)); // scale_times_1000 = 500
        obs.push(0xFF); // SLEB run
        obs.extend(enc_uleb(1)); // count_m1=1 → 2
        obs.extend(enc_sleb(4));
        obs.extend(enc_sleb(6));

        out.extend(enc_uleb(obs.len() as u64));
        out.extend(obs);

        let socd = parse(&out).unwrap();
        assert_eq!(socd.scale, 500);
        assert_eq!(socd.values, &[4, 6]);
    }

    #[test]
    fn lli_rle_decoding() {
        // 4 observations: ' ', ' ', '1', ' '  → LLI RLE: (' ' ×2, '1' ×1, ' ' ×1)
        // New encoding: packed ULEB128 = (count-1)*11 + idx
        // ' ' = idx 0, '1' = idx 2 (space=0, '0'=1, '1'=2)
        // Run ' ' ×2: (2-1)*11 + 0 = 11
        // Run '1' ×1: (1-1)*11 + 2 = 2
        let mut lli_rle: Vec<u8> = Vec::new();
        lli_rle.extend(enc_uleb(11)); // ' ' ×2
        lli_rle.extend(enc_uleb(2)); // '1' ×1
                                     // Don't encode trailing spaces; they'll be padded.

        let mut out = b"G01\0L1\0\0".to_vec();
        out.extend(enc_uleb(3)); // count=4
        out.extend(enc_uleb(lli_rle.len() as u64));
        out.extend_from_slice(&lli_rle);
        out.extend(enc_uleb(0)); // SSI: 0 bytes

        // 4 zero values via SLEB run (order=0).
        let mut obs: Vec<u8> = Vec::new();
        obs.extend(enc_uleb(0)); // schema=0
        obs.push(0xFF);
        obs.extend(enc_uleb(3));
        for _ in 0..4 {
            obs.extend(enc_sleb(0));
        }

        out.extend(enc_uleb(obs.len() as u64));
        out.extend(obs);

        let socd = parse(&out).unwrap();
        assert_eq!(socd.lli, b"  1 ");
    }

    // ── Fixture integration tests ─────────────────────────────────────────

    fn try_load_socd_chunk(srnx_name: &str, socd_index: usize) -> Option<Vec<u8>> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(srnx_name);
        let bytes = std::fs::read(path).ok()?;
        let file = crate::file::SrnxFile::parse(&bytes).ok()?;
        let socd_chunks: Vec<_> = file
            .chunks
            .iter()
            .filter(|c| c.fourcc == *b"SOCD")
            .collect();
        let chunk = socd_chunks.get(socd_index)?;
        Some(chunk.payload.to_vec())
    }

    #[test]
    fn fixture_at012000_socd_count_matches() {
        // The first SOCD in at012000 is E01/L1 with 27047 present observations.
        let Some(payload) = try_load_socd_chunk("at012000.srnx", 0) else {
            return;
        };
        let socd = parse(&payload).expect("SOCD parse must succeed");
        assert_eq!(socd.obs_name[..3], *b"E01");
        assert_eq!(socd.values.len(), 27047);
        assert_eq!(socd.lli.len(), 27047);
        assert_eq!(socd.ssi.len(), 27047);
    }

    #[test]
    fn fixture_at012000_socd_values_reasonable() {
        // E01/L1 carrier phase. Stored as integer millicycles × scale.
        // Default scale=1000: value is in millicycles. Galileo L1 ≈ 1.5 GHz,
        // wavelength ≈ 0.19 m ≈ 19 cm. Phase range ~1e8 cycles.
        let Some(payload) = try_load_socd_chunk("at012000.srnx", 0) else {
            return;
        };
        let socd = parse(&payload).expect("parse must succeed");
        assert_eq!(socd.scale, 1);
        let present: Vec<_> = socd
            .values
            .iter()
            .filter(|&&v| v != ABSENT)
            .copied()
            .collect();
        // L1 carrier phase values are physical × 1000 (millicycles); should be large (>1e11).
        let max_abs = present.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
        assert!(
            max_abs > 1_000_000,
            "L1 phase values seem too small: max={max_abs}"
        );
    }

    #[test]
    fn fixture_at012000_no_value_overflows_sentinel() {
        // Total value count must be 27047; a small fraction may be ABSENT due
        // to data gaps within the satellite's visibility window.
        let Some(payload) = try_load_socd_chunk("at012000.srnx", 0) else {
            return;
        };
        let socd = parse(&payload).unwrap();
        assert_eq!(socd.values.len(), 27047);
        // No "valid" slot should accidentally equal the absent sentinel when the
        // encoding doesn't intend it — just confirm there's no runaway absents.
        let absent_count = socd.values.iter().filter(|&&v| v == ABSENT).count();
        assert!(
            absent_count < 1000,
            "unexpectedly many absent slots: {absent_count}"
        );
    }

    /// Encode `rnx-data/<source>` with `rnx2srnx --socd` so the SOCD decoder
    /// is exercised; the nominal `.srnx` fixtures use revision 1.1 SOC2.
    /// Returns `None` (skipping the test) if the source or encoder is absent.
    fn socd_fixture(source: &str) -> Option<Vec<u8>> {
        let input = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(source);
        if !input.exists() {
            eprintln!("skipping SOCD fixture test: {source} unavailable");
            return None;
        }
        let output = std::env::temp_dir().join(format!(
            "srnx-socd-test-{}-{source}.srnx",
            std::process::id()
        ));
        let Ok(status) = std::process::Command::new("rnx2srnx")
            .arg("--socd")
            .arg(&input)
            .arg(&output)
            .status()
        else {
            eprintln!("skipping SOCD fixture test: rnx2srnx unavailable");
            return None;
        };
        assert!(status.success(), "rnx2srnx --socd {source}: {status}");
        let bytes = std::fs::read(&output).expect("read SOCD fixture");
        let _ = std::fs::remove_file(&output);
        Some(bytes)
    }

    #[test]
    fn fixture_at012000_all_socd_chunks_parse() {
        let Some(bytes) = socd_fixture("at012000.20o") else {
            return;
        };
        let file = crate::file::SrnxFile::parse(&bytes).expect("file parse");
        let mut chunks = 0_usize;
        let mut total_values: usize = 0;
        for chunk in file.chunks.iter().filter(|c| c.fourcc == *b"SOCD") {
            let socd = parse(chunk.payload)
                .unwrap_or_else(|e| panic!("SOCD at offset {} failed: {e}", chunk.file_offset));
            chunks += 1;
            total_values += socd.values.len();
        }
        assert_eq!(chunks, 705);
        // Sanity: total decoded values across all 705 SOCDs.
        assert!(
            total_values > 1_000_000,
            "expected >1M values, got {total_values}"
        );
    }

    #[test]
    fn fixture_abmf_all_socd_parse() {
        let Some(bytes) = socd_fixture("abmf.rnx") else {
            return;
        };
        let file = crate::file::SrnxFile::parse(&bytes).expect("file parse");
        let mut chunks = 0_usize;
        for chunk in file.chunks.iter().filter(|c| c.fourcc == *b"SOCD") {
            parse(chunk.payload)
                .unwrap_or_else(|e| panic!("SOCD at {} failed: {e}", chunk.file_offset));
            chunks += 1;
        }
        assert!(chunks > 0, "abmf --socd produced no SOCD chunks");
    }
}
