//! Parser for RINEX v2.x observation data files.
//!
//! Reads the fixed-column text format defined in `docs/rinex211.txt`,
//! Table A2.  Populates the shared [`ObsFile`] semantic model.
//!
//! ## Format summary
//!
//! ### Epoch / satellite header line (80 chars)
//!
//! ```text
//!  YY MM DD HH MM SS.SSSSSSS  F NNN S01S02...S12  CCCCCCCCCCCC
//!  ^col1                                            ^clock offset
//! ```
//!
//! Fortran: `1X, I2.2, 4(1X,I2), F11.7, 2X, I1, I3, 12(A1,I2), F12.9`
//!
//! Continuation line for >12 satellites:
//! `32X, 12(A1,I2)`
//!
//! ### Observation records (one set per satellite in epoch order)
//!
//! `m(F14.3, I1, I1)` per line, up to 5 observations (80 chars total).
//! If a satellite has more than 5 observation codes the record wraps to a
//! second (or more) 80-char line.  Lines shorter than the full 80 chars
//! have been right-truncated; treat missing characters as spaces.
//!
//! ### Event records (epoch flag 2–6)
//!
//! The "number of satellites" field instead gives the number of verbatim
//! lines that follow the epoch header.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::model::{EpochRecord, EventRecord, Obs, ObsFile, SatId};
use crate::rinex_header::{self, HeaderError, HeaderText, ObsCode, SatSystem};

#[derive(Debug, Error)]
pub enum Rinex2Error {
    #[error("header parse error: {0}")]
    Header(#[from] HeaderError),
    #[error("input is not valid UTF-8")]
    NotUtf8,
    #[error("no END OF HEADER line found")]
    NoEndOfHeader,
    #[error("epoch line is too short to contain a timestamp (line {0})")]
    EpochLineTooShort(usize),
    #[error("invalid epoch flag or satellite count on line {0}")]
    BadEpochHeader(usize),
    #[error("file ended mid-epoch (expected {expected} observation lines, got {got})")]
    UnexpectedEof { expected: usize, got: usize },
    #[error("observation file has no obs codes in header")]
    NoObsCodes,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a complete RINEX v2 observation file.
///
/// Both LF and CRLF line endings are accepted.
#[allow(clippy::too_many_lines)]
pub fn parse(input: &[u8]) -> Result<ObsFile, Rinex2Error> {
    let text = std::str::from_utf8(input).map_err(|_| Rinex2Error::NotUtf8)?;

    // Split into lines (strips the newline character(s)).
    let lines: Vec<&str> = text.lines().collect();

    // Locate the END OF HEADER line.
    let eoh_idx = lines
        .iter()
        .position(|l| {
            let trimmed = if l.len() >= 60 {
                l[60..].trim()
            } else {
                l.trim()
            };
            trimmed.starts_with("END OF HEADER")
        })
        .ok_or(Rinex2Error::NoEndOfHeader)?;

    // Capture raw header text (all lines through END OF HEADER), normalized to LF.
    // `lines()` strips newlines, so re-add the trailing LF to match RHDR semantics.
    let mut raw_header_text = lines[..=eoh_idx].join("\n");
    raw_header_text.push('\n');

    // Parse the header section (pass original bytes — the header parser
    // tolerates trailing data because it stops at END OF HEADER).
    let header = rinex_header::parse(input).map_err(Rinex2Error::Header)?;

    // Get the observation code list.  For v2, all systems share a single list
    // stored under SatSystem::MIXED.  Fall back to the first list if MIXED is
    // absent (should not happen for conforming v2 files).
    let obs_codes: Vec<ObsCode> = if let Some(codes) = header.obs_codes.for_system(SatSystem::MIXED)
    {
        codes.to_vec()
    } else {
        header
            .obs_codes
            .by_system
            .first()
            .map(|(_, v)| v.clone())
            .ok_or(Rinex2Error::NoObsCodes)?
    };
    let num_obs = obs_codes.len();
    let obs_lines_per_sat = obs_lines_count(num_obs);

    // Data starts on the line after END OF HEADER.
    let data_lines = &lines[eoh_idx + 1..];
    let base_line = eoh_idx + 1; // for error reporting

    // Sparse per-(sat, code) → [(epoch_index, Obs)] list, appended in order.
    let mut sparse: BTreeMap<(SatId, ObsCode), Vec<(usize, Obs)>> = BTreeMap::new();
    let mut epochs: Vec<EpochRecord> = Vec::new();
    let mut events: Vec<(usize, EventRecord)> = Vec::new();

    let mut pos = 0; // index into data_lines
    while pos < data_lines.len() {
        let line = data_lines[pos];
        let line_no = base_line + pos + 1; // 1-based line number for errors

        // Parse the epoch / satellite header line.
        let Ok((epoch_rec, flag, num_entries)) = parse_epoch_header(line, line_no) else {
            // Not a valid epoch header — skip to the next plausible one.
            pos += 1;
            continue;
        };
        pos += 1;

        // Epoch flags 2–5: event record.  The num_entries field is the count of
        // verbatim special-record lines that follow; there is NO satellite PRN
        // list on the header line.  Must check flag BEFORE calling collect_sats.
        if (2u8..=5).contains(&flag) {
            let epoch_idx = epochs.len();
            let end = (pos + num_entries).min(data_lines.len());
            let event_lines: Vec<String> = data_lines[pos..end]
                .iter()
                .map(|l| (*l).to_string())
                .collect();
            pos = end;
            events.push((
                epoch_idx,
                EventRecord {
                    flag,
                    lines: event_lines,
                },
            ));
            continue;
        }

        // Flags 0, 1, 6: epoch header carries a satellite PRN list.
        let mut sats: Vec<SatId> = Vec::with_capacity(num_entries);
        collect_sats(line, &mut sats); // 'line' still refers to the header line

        // Continuation lines for >12 satellites.
        let mut cont_malformed = false;
        while sats.len() < num_entries && pos < data_lines.len() {
            // Each continuation line must actually look like one.
            if !is_continuation_line(data_lines[pos]) {
                cont_malformed = true;
                break;
            }
            let before = sats.len();
            collect_sats(data_lines[pos], &mut sats);
            // Stop if this continuation line added no satellites.
            if sats.len() == before {
                break;
            }
            pos += 1;
        }
        sats.truncate(num_entries);

        // --- Validate epoch structure before committing ---
        //
        // A well-formed epoch has:
        //   1. No more satellite IDs than the claimed num_entries (fewer is OK
        //      for non-conforming files; claimed count still governs obs lines).
        //   2. No epoch-header line embedded in the observation block.
        //   3. The line after the observation block is an epoch header (or EOF).
        //
        // If any check fails, discard this epoch and scan forward to the
        // next epoch header.

        let obs_lines_total = num_entries * obs_lines_per_sat;

        // Check 1: continuation lines were malformed or we collected MORE
        // satellites than claimed (indicates we misparsed an obs line as a
        // continuation).  Fewer satellites than claimed is non-conforming but
        // recoverable — the claimed count still tells us how many obs lines
        // to consume.
        let sat_count_ok = sats.len() <= num_entries;

        // Check 2: is there an epoch or event header line within the expected
        // observation block?  If so, the epoch structure is broken — skip to
        // that header.
        let mut malformed = cont_malformed || !sat_count_ok;
        let mut skip_to: Option<usize> = None;
        if !malformed {
            for i in pos..pos + obs_lines_total {
                if i >= data_lines.len() {
                    malformed = true;
                    break;
                }
                if is_any_header_line(data_lines[i]) {
                    skip_to = Some(i);
                    malformed = true;
                    break;
                }
            }
        }

        // Check 3: the line after the observation block should be an epoch
        // header, event record header, or EOF.  Non-conforming files
        // (e.g., teqc-spliced) may have extra lines (comments, blanks) between
        // epochs.  Scan forward to find the next epoch or event header; if
        // found nearby, skip the extra lines after parsing observations.
        // Only mark malformed if no header appears within a reasonable distance.
        let mut skip_extra_to: Option<usize> = None;
        if !malformed {
            let after_obs = pos + obs_lines_total;
            if after_obs < data_lines.len() && !is_any_header_line(data_lines[after_obs]) {
                // Scan forward for the next epoch or event header.
                let mut scan_pos = after_obs;
                let mut found_next = None;
                while scan_pos < data_lines.len() {
                    if is_any_header_line(data_lines[scan_pos]) {
                        found_next = Some(scan_pos);
                        break;
                    }
                    scan_pos += 1;
                    // Give up after scanning too far — something is genuinely wrong.
                    if scan_pos - after_obs > 100 {
                        break;
                    }
                }
                if let Some(next_header) = found_next {
                    // Extra lines between epochs — remember to skip them.
                    skip_extra_to = Some(next_header);
                } else {
                    // No header found — genuinely malformed.
                    malformed = true;
                }
            }
            // At EOF with exactly the right number of lines is fine.
        }

        if malformed {
            // Already found an epoch header mid-block? Jump there.
            if let Some(target) = skip_to {
                pos = target;
                continue;
            }
            // Otherwise scan forward from after the expected block.
            let scan_from = pos + obs_lines_total.min(data_lines.len().saturating_sub(pos));
            pos = scan_from;
            while pos < data_lines.len() {
                if is_any_header_line(data_lines[pos]) {
                    break;
                }
                pos += 1;
            }
            continue;
        }

        // --- Valid epoch — parse observations ---

        // Flag 6 (cycle-slip record): same obs format as flags 0/1, but we
        // discard the observations and do not add an epoch entry.
        if flag == 6 {
            pos += obs_lines_total;
            // Skip any extra lines between epochs.
            if let Some(target) = skip_extra_to {
                pos = target;
            }
            continue;
        }

        // Normal epoch (flag 0 or 1): parse observation records.
        let epoch_idx = epochs.len();
        epochs.push(epoch_rec);

        for &sat in &sats {
            for (obs_line_idx, obs_chunk) in
                data_lines[pos..pos + obs_lines_per_sat].iter().enumerate()
            {
                let base_obs = obs_line_idx * 5;
                let end_obs = (base_obs + 5).min(num_obs);
                for (slot, &code) in obs_codes[base_obs..end_obs].iter().enumerate() {
                    if let Some(obs) = parse_obs_field(obs_chunk, slot) {
                        sparse
                            .entry((sat, code))
                            .or_default()
                            .push((epoch_idx, obs));
                    }
                }
            }
            pos += obs_lines_per_sat;
        }

        // Skip any extra lines between epochs (e.g., teqc splice comments).
        if let Some(target) = skip_extra_to {
            pos = target;
        }
    }

    Ok(ObsFile {
        header: HeaderText::from_text(&raw_header_text),
        epochs,
        events,
        observations: sparse,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Number of 80-char observation lines required for `num_obs` obs codes.
#[inline]
fn obs_lines_count(num_obs: usize) -> usize {
    num_obs.div_ceil(5)
}

/// Check if a line could be an epoch header or event record header.
/// Uses a dummy line number since we only care about Ok/Err.
///
/// Fast-path: check the separator spaces at positions 26–27 and the epoch
/// flag digit at position 28 before calling the full parser.  Observation
/// lines have the hundredths/tenths of a seconds value at 26–27 (digits or
/// `.`), so a header line must have spaces there and a digit at 28.
fn is_any_header_line(line: &str) -> bool {
    let b = line.as_bytes();
    if b.len() < 32
        || b[26] != b' '
        || b[27] != b' '
        || !b[28].is_ascii_digit()
    {
        return false;
    }
    parse_epoch_header(line, 0).is_ok()
}

/// Heuristic: does this line look like an epoch-header continuation?
///
/// Continuation lines start with 32 spaces then a letter (system id).
fn is_continuation_line(line: &str) -> bool {
    line.len() > 32
        && line[..32].chars().all(|c| c == ' ')
        && line
            .as_bytes()
            .get(32)
            .is_some_and(|&b| b.is_ascii_alphabetic())
}

/// Return a character at byte offset `i` in `line`, or `b' '` if the line is
/// shorter.  RINEX v2 lines may have trailing whitespace stripped by writers.
#[inline]
fn byte_at(line: &str, i: usize) -> u8 {
    line.as_bytes().get(i).copied().unwrap_or(b' ')
}

/// Parse a 2-digit year to a 4-digit year using the RINEX v2 convention:
/// 80–99 → 1980–1999, 00–79 → 2000–2079.
#[inline]
fn expand_year(y2: u8) -> u16 {
    if y2 >= 80 {
        1900 + u16::from(y2)
    } else {
        2000 + u16::from(y2)
    }
}

/// Parse a fixed-width decimal integer from `line[start..start+width]`,
/// interpreting blank/missing as zero.
fn parse_int_field(line: &str, start: usize, width: usize) -> i64 {
    let mut acc: i64 = 0;
    let mut any_digit = false;
    let mut negative = false;
    for i in start..start + width {
        let b = byte_at(line, i);
        match b {
            b'-' => negative = true,
            b'0'..=b'9' => {
                acc = acc * 10 + i64::from(b - b'0');
                any_digit = true;
            }
            _ => {}
        }
    }
    if !any_digit {
        return 0;
    }
    if negative {
        -acc
    } else {
        acc
    }
}

/// Parse the epoch / satellite header line.
///
/// Returns `(EpochRecord, flag, num_entries)` where `num_entries` is the
/// number of satellite IDs to read (or special-record lines for events).
fn parse_epoch_header(line: &str, line_no: usize) -> Result<(EpochRecord, u8, usize), Rinex2Error> {
    if line.len() < 32 {
        return Err(Rinex2Error::EpochLineTooShort(line_no));
    }

    // Byte layout (0-indexed):
    //  0     : space (1X)
    //  1-2   : year (I2.2)
    //  3     : space
    //  4-5   : month (I2)
    //  6     : space
    //  7-8   : day (I2)
    //  9     : space
    //  10-11 : hour (I2)
    //  12    : space
    //  13-14 : minute (I2)
    //  15-25 : seconds (F11.7)
    //  26-27 : spaces (2X)
    //  28    : epoch flag (I1)
    //  29-31 : num satellites (I3)
    //  32-67 : up to 12 satellite IDs  (12 × A1,I2)
    //  68-79 : receiver clock offset (F12.9, optional)

    // Verify separator positions are spaces.  This rejects observation data
    // lines that happen to have digits at timestamp positions.
    for &sep_pos in &[3, 6, 9, 12, 26, 27] {
        if byte_at(line, sep_pos) != b' ' {
            return Err(Rinex2Error::BadEpochHeader(line_no));
        }
    }

    let y2 = u8::try_from(parse_int_field(line, 1, 2))
        .map_err(|_| Rinex2Error::BadEpochHeader(line_no))?;
    let month = u8::try_from(parse_int_field(line, 4, 2))
        .map_err(|_| Rinex2Error::BadEpochHeader(line_no))?;
    let day = u8::try_from(parse_int_field(line, 7, 2))
        .map_err(|_| Rinex2Error::BadEpochHeader(line_no))?;
    let hour = u8::try_from(parse_int_field(line, 10, 2))
        .map_err(|_| Rinex2Error::BadEpochHeader(line_no))?;
    let minute = u8::try_from(parse_int_field(line, 13, 2))
        .map_err(|_| Rinex2Error::BadEpochHeader(line_no))?;

    // Seconds: F11.7 at positions 15-25.  Parse as f64 then convert to e7.
    let sec_str = &line[15..line.len().min(26)];
    let sec_f64: f64 = sec_str.trim().parse().unwrap_or(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds_e7 = (sec_f64 * 1e7).round() as u64;

    let flag_raw = parse_int_field(line, 28, 1);
    if !(0..=6).contains(&flag_raw) {
        return Err(Rinex2Error::BadEpochHeader(line_no));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let flag = flag_raw as u8;

    // Validate timestamp ranges to reject observation data lines that
    // happen to have digits at epoch-header positions.  Events (flags 2-5)
    // may have missing timestamps in non-conforming files.
    let timestamp_present = month != 0 || day != 0 || hour != 0 || minute != 0 || sec_f64 != 0.0;
    if timestamp_present {
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
            return Err(Rinex2Error::BadEpochHeader(line_no));
        }
    } else if !(2..=5).contains(&flag) {
        // Normal epochs must have a valid timestamp.
        return Err(Rinex2Error::BadEpochHeader(line_no));
    }

    let num_entries_raw = parse_int_field(line, 29, 3);
    if num_entries_raw < 0 {
        return Err(Rinex2Error::BadEpochHeader(line_no));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let num_entries = num_entries_raw as usize;

    // Receiver clock offset: F12.9 at positions 68-79.
    let clock_offset_e12 = if line.len() > 68 {
        let clk_str = &line[68..line.len().min(80)];
        let clk_f64: f64 = clk_str.trim().parse().unwrap_or(0.0);
        #[allow(clippy::cast_possible_truncation)]
        let raw = (clk_f64 * 1e12).round() as i64;
        raw
    } else {
        0
    };

    let rec = EpochRecord {
        year: expand_year(y2),
        month,
        day,
        hour,
        minute,
        seconds_e7,
        flag,
        clock_offset_e12,
    };

    Ok((rec, flag, num_entries))
}

/// Append satellite IDs found in `line[32..68]` to `sats`.
///
/// Handles both normal epoch lines and continuation lines — both have the
/// satellite IDs at positions 32–67 (12 × A1,I2).
fn collect_sats(line: &str, sats: &mut Vec<SatId>) {
    for slot in 0..12 {
        let start = 32 + slot * 3;
        if start + 3 > 80 {
            break;
        }
        let sys_byte = byte_at(line, start);
        let prn_tens = byte_at(line, start + 1);
        let prn_ones = byte_at(line, start + 2);

        // A blank system or PRN field means no more satellites on this line.
        // Also break if the system byte is blank but PRN chars aren't both digits
        // (this avoids misreading observation data as GPS satellite IDs).
        if sys_byte == b' ' && (!prn_tens.is_ascii_digit() || !prn_ones.is_ascii_digit()) {
            break;
        }

        // Blank system byte = GPS (backward compatibility with GPS-only files).
        let system = if sys_byte == b' ' {
            SatSystem::GPS
        } else {
            SatSystem(sys_byte)
        };

        let p1 = match prn_tens {
            b'0'..=b'9' => prn_tens - b'0',
            _ => 0, // non-digit (e.g. truncated line) → treat as 0
        };
        let p0 = match prn_ones {
            b'0'..=b'9' => prn_ones - b'0',
            _ => 0,
        };
        let prn = p1 * 10 + p0;
        sats.push(SatId::new(system, prn));
    }
}

/// Parse one observation field from `line` at the given slot (0–4).
///
/// Each slot is 16 bytes: 14-byte F14.3 value + 1-byte LLI + 1-byte SSI.
/// Returns `None` if the value field is entirely blank (absent observation).
///
/// F14.3 values have exactly 3 decimal places.  We parse them directly as
/// integers by skipping the decimal point — no `String` allocation and no
/// `f64` round-trip.
fn parse_obs_field(line: &str, slot: usize) -> Option<Obs> {
    let start = slot * 16;

    // Parse the 14-byte F14.3 value field as an integer (×1000 milliunits).
    let mut acc: i64 = 0;
    let mut negative = false;
    let mut any_digit = false;

    for i in 0..14 {
        let b = byte_at(line, start + i);
        match b {
            b'-' => negative = true,
            b'0'..=b'9' => {
                acc = acc * 10 + i64::from(b - b'0');
                any_digit = true;
            }
            b'.' | b' ' | b'+' => {}
            _ => return None, // unexpected character → treat as absent
        }
    }

    if !any_digit {
        return None;
    }

    let value = if negative { -acc } else { acc };

    let lli = byte_at(line, start + 14);
    let ssi = byte_at(line, start + 15);

    Some(Obs {
        value,
        scale: 1,
        lli,
        ssi,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- unit tests -----------------------------------------------

    #[test]
    fn obs_lines_count_cases() {
        assert_eq!(obs_lines_count(0), 0);
        assert_eq!(obs_lines_count(1), 1);
        assert_eq!(obs_lines_count(5), 1);
        assert_eq!(obs_lines_count(6), 2);
        assert_eq!(obs_lines_count(10), 2);
        assert_eq!(obs_lines_count(11), 3);
        assert_eq!(obs_lines_count(20), 4);
        assert_eq!(obs_lines_count(21), 5);
    }

    #[test]
    fn expand_year_cases() {
        assert_eq!(expand_year(0), 2000);
        assert_eq!(expand_year(20), 2020);
        assert_eq!(expand_year(79), 2079);
        assert_eq!(expand_year(80), 1980);
        assert_eq!(expand_year(99), 1999);
    }

    #[test]
    fn parse_obs_field_present() {
        // " 128250890.432 5" = F14.3 value + LLI=' ' + SSI='5'
        let line = " 128250890.432 5  99935750.10842";
        let obs = parse_obs_field(line, 0).unwrap();
        assert_eq!(obs.value, 128_250_890_432);
        assert_eq!(obs.scale, 1);
        assert_eq!(obs.lli, b' ');
        assert_eq!(obs.ssi, b'5');

        let obs2 = parse_obs_field(line, 1).unwrap();
        assert_eq!(obs2.value, 99_935_750_108);
        assert_eq!(obs2.lli, b'4');
        assert_eq!(obs2.ssi, b'2');
    }

    #[test]
    fn parse_obs_field_blank() {
        // Entirely blank field = absent.
        let line = "                                ";
        assert!(parse_obs_field(line, 0).is_none());
        assert!(parse_obs_field(line, 1).is_none());
    }

    #[test]
    fn parse_obs_field_short_line() {
        // Line shorter than 80 chars: missing fields treated as blank.
        let line = "        35.250";
        let obs = parse_obs_field(line, 0).unwrap();
        assert_eq!(obs.value, 35_250);
        // LLI and SSI beyond end of line → spaces.
        assert_eq!(obs.lli, b' ');
        assert_eq!(obs.ssi, b' ');
        // Second slot is entirely beyond the line.
        assert!(parse_obs_field(line, 1).is_none());
    }

    #[test]
    fn parse_obs_field_negative() {
        // Doppler: -1010.755.  F14.3 = "     -1010.755" (5 spaces + 9 chars = 14).
        // LLI = ' ', SSI = ' '.  Total field = 16 chars.
        let line = "     -1010.755  ";
        let obs = parse_obs_field(line, 0).unwrap();
        assert_eq!(obs.value, -1_010_755);
        assert_eq!(obs.lli, b' ');
        assert_eq!(obs.ssi, b' ');
    }

    #[test]
    fn collect_sats_first_line() {
        // First 12 sats from the at012000 epoch line (after pos 32).
        let line = " 20  7 18  0  0  0.0000000  0 29G13R10E12E01R03E26G16G11R02G08E31G27";
        let mut sats = Vec::new();
        collect_sats(line, &mut sats);
        assert_eq!(sats.len(), 12);
        assert_eq!(sats[0], SatId::new(SatSystem::GPS, 13));
        assert_eq!(sats[1], SatId::new(SatSystem::GLONASS, 10));
        assert_eq!(sats[2], SatId::new(SatSystem::GALILEO, 12));
        assert_eq!(sats[11], SatId::new(SatSystem::GPS, 27));
    }

    #[test]
    fn collect_sats_continuation() {
        let line = "                                G20G30E13E24R16G10E07R09G21R19R18E03";
        let mut sats = Vec::new();
        collect_sats(line, &mut sats);
        assert_eq!(sats.len(), 12);
        assert_eq!(sats[0], SatId::new(SatSystem::GPS, 20));
        assert_eq!(sats[11], SatId::new(SatSystem::GALILEO, 3));
    }

    // ---- fixture tests -----------------------------------------------

    fn try_load_rinex2(name: &str) -> Option<ObsFile> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(name);
        let bytes = std::fs::read(&path).ok()?;
        parse(&bytes).ok()
    }

    #[test]
    fn fixture_at012000_header_and_epoch_count() {
        let Some(obs) = try_load_rinex2("at012000.20o") else {
            return;
        };
        assert_eq!(obs.header.get_line("MARKER NAME"), Some("AT01".to_string()));
        // at012000 is a full day at 1s intervals = 86400 epochs.
        assert_eq!(obs.epochs.len(), 86_400);
    }

    #[test]
    fn fixture_at012000_first_epoch_timestamp() {
        let Some(obs) = try_load_rinex2("at012000.20o") else {
            return;
        };
        let e0 = &obs.epochs[0];
        assert_eq!(e0.year, 2020);
        assert_eq!(e0.month, 7);
        assert_eq!(e0.day, 18);
        assert_eq!(e0.hour, 0);
        assert_eq!(e0.minute, 0);
        assert_eq!(e0.seconds_e7, 0);
        assert_eq!(e0.flag, 0);
    }

    #[test]
    fn fixture_at012000_g13_l1_first_epoch() {
        let Some(obs) = try_load_rinex2("at012000.20o") else {
            return;
        };
        // In the first epoch, G13 L1 = 128250890.432, SSI=5.
        let l1 = ObsCode::from_v2("L1").unwrap();
        let g13 = SatId::new(SatSystem::GPS, 13);
        let series = obs.observations.get(&(g13, l1)).expect("G13/L1 present");
        // Sparse series: first entry should be (epoch_idx=0, obs).
        let &(ep_idx, first) = series.first().expect("at least one G13/L1 observation");
        assert_eq!(ep_idx, 0, "G13/L1 observed at epoch 0");
        assert_eq!(first.value, 128_250_890_432);
        assert_eq!(first.scale, 1);
        assert_eq!(first.lli, b' ');
        assert_eq!(first.ssi, b'5');
    }

    #[test]
    fn fixture_at012000_s1_first_epoch() {
        let Some(obs) = try_load_rinex2("at012000.20o") else {
            return;
        };
        // G13 S1 = 35.250 in epoch 0 (obs index 5 on obs line 2).
        let s1 = ObsCode::from_v2("S1").unwrap();
        let g13 = SatId::new(SatSystem::GPS, 13);
        let series = obs.observations.get(&(g13, s1)).expect("G13/S1 present");
        let &(ep_idx, first) = series.first().expect("at least one G13/S1 observation");
        assert_eq!(ep_idx, 0);
        assert_eq!(first.value, 35_250);
    }

    #[test]
    fn fixture_at012000_observation_series_sorted() {
        let Some(obs) = try_load_rinex2("at012000.20o") else {
            return;
        };
        // Every observation series must have epoch indices in ascending order,
        // within the valid range, and contain at least one entry.
        let num_epochs = obs.epochs.len();
        for (key, series) in &obs.observations {
            assert!(
                !series.is_empty(),
                "empty series for {}/{}",
                key.0,
                key.1.as_str()
            );
            assert!(
                series.windows(2).all(|w| w[0].0 < w[1].0),
                "series for {}/{} is not sorted",
                key.0,
                key.1.as_str()
            );
            assert!(
                series.last().map(|&(i, _)| i).unwrap() < num_epochs,
                "epoch index out of range for {}/{}",
                key.0,
                key.1.as_str()
            );
        }
    }

    #[test]
    fn fixture_mty22000_parses() {
        let Some(obs) = try_load_rinex2("mty22000.20o") else {
            return;
        };
        // Confirm header is parseable (obs codes tested in rinex_header module).
        assert!(obs.header.get_line("RINEX VERSION / TYPE").is_some());
        // mty22000 is also a full day at 15s intervals = 86400/15 = 5760 epochs.
        assert_eq!(obs.epochs.len(), 5_760);
    }

    #[test]
    fn fixture_okcb1200_parses() {
        let Some(obs) = try_load_rinex2("okcb1200.26o") else {
            return;
        };
        // Confirm header is parseable (obs codes tested in rinex_header module).
        assert!(obs.header.get_line("RINEX VERSION / TYPE").is_some());
        // okcb1200 runs 2026-04-30 for 24 hours at 1s = 86400 epochs.
        assert_eq!(obs.epochs.len(), 86_400);
    }

    #[test]
    fn fixture_sbst1200_parses() {
        let Some(obs) = try_load_rinex2("sbst1200.26o") else {
            return;
        };
        assert!(!obs.epochs.is_empty());
        let num_epochs = obs.epochs.len();
        for series in obs.observations.values() {
            // Sparse series: epoch indices must be sorted and within range.
            assert!(series.windows(2).all(|w| w[0].0 < w[1].0));
            assert!(series.last().map_or(0, |&(i, _)| i) < num_epochs);
        }
    }
}

