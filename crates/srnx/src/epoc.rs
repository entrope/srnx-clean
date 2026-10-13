//! Parser for the `EPOC` (Epoch Records) chunk's payload.
//!
//! Per `docs/Succinct_RINEX.md` §EPOC:
//!
//! ```text
//! ULEB128 total_epoch_count
//! one or more spans:
//!   SLEB128 interval
//!   ULEB128 (epochs_in_span - 1)
//!   ULEB128 packed_date  =  year*10000 + month*100 + day
//!   ULEB128 packed_time  =  hour*1e11 + minute*1e9 + second*1e7
//! optional RLE-encoded receiver clock offsets:
//!   pairs of  (SLEB128 value*1e12, ULEB128 (run_count - 1))
//! ```
//!
//! ## Interval encoding
//!
//! The SLEB128 interval is asymmetric for compactness:
//! * **negative** values mean integer-second intervals — interval is
//!   `-stored_value` seconds (so `SLEB128(-1)` is 1 s, `SLEB128(-30)` is 30 s).
//! * **positive** values are sub-second intervals expressed as
//!   `seconds × 10⁷` (so `SLEB128(5_000_000)` is 0.5 s).
//!
//! ## Time advancement
//!
//! Within a span, each successive epoch is at `prev + interval`. When the
//! resulting seconds value is **at least 60**, the excess carries into the
//! next higher unit (minutes, then hours).  The sole exception is the
//! leap-second range `23:59:60.0` through `23:59:60.9999999`, which is
//! preserved as-is.  A timestamp `>= 23:59:61` is illegal per the spec.
//! Encoders MUST start a new span at each day boundary (the first epoch of
//! each day), so within a single span hours stay in `0..=23`.
//!
//! ## Clock offsets
//!
//! Stored as `(value, run_count - 1)` pairs. Trailing epochs not covered by
//! any pair are implicitly zero. RINEX 2.x files SHOULD omit clock offsets
//! entirely, in which case the section is empty and all offsets are zero.

use thiserror::Error;

use crate::leb128::{read_sleb128, read_uleb128, LebError};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EpocError {
    #[error("invalid LEB128 in EPOC chunk: {0}")]
    Leb(#[from] LebError),
    #[error("EPOC declared {declared} epochs but spans cover {covered}")]
    SpanCountMismatch { declared: u64, covered: u64 },
    #[error("EPOC date encoding {0} is invalid")]
    BadDate(u64),
    #[error("EPOC time encoding {0} is invalid")]
    BadTime(u64),
    #[error("EPOC epoch count {0} exceeds addressable memory")]
    TooManyEpochs(u64),
}

/// One materialized epoch record. Time fields are kept verbatim from the
/// span expansion: `seconds_e7` may exceed `60 × 10⁷` during leap seconds,
/// and `hour` may grow past 23 if a (non-conformant) span crosses a day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Epoch {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    /// Seconds within the minute, as `s × 10⁷`.
    pub seconds_e7: u64,
    /// Receiver clock offset, as `value × 10¹²`. Zero if not specified.
    pub clock_offset_e12: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Epoc {
    pub epochs: Vec<Epoch>,
}

pub fn parse(payload: &[u8]) -> Result<Epoc, EpocError> {
    let mut cursor: &[u8] = payload;
    let total = read_uleb128(&mut cursor)?;
    let total_usize = usize::try_from(total).map_err(|_| EpocError::TooManyEpochs(total))?;

    if total == 0 {
        return Ok(Epoc::default());
    }

    let mut epochs: Vec<Epoch> = Vec::with_capacity(total_usize);
    while u64::try_from(epochs.len()).expect("len fits in u64") < total {
        let interval = read_sleb128(&mut cursor)?;
        let span_count_m1 = read_uleb128(&mut cursor)?;
        let span_count = span_count_m1
            .checked_add(1)
            .ok_or(EpocError::Leb(LebError::Overflow))?;
        let date = read_uleb128(&mut cursor)?;
        let time = read_uleb128(&mut cursor)?;

        let (year, month, day) = decode_date(date)?;
        let (mut hour, mut minute, mut seconds_e7) = decode_time(time)?;
        let interval_e7 = interval_to_e7(interval);

        for i in 0..span_count {
            if i > 0 {
                advance_time(&mut hour, &mut minute, &mut seconds_e7, interval_e7);
            }
            epochs.push(Epoch {
                year,
                month,
                day,
                hour,
                minute,
                seconds_e7,
                clock_offset_e12: 0,
            });
            if u64::try_from(epochs.len()).expect("len fits in u64") == total {
                break;
            }
        }
    }

    let covered = u64::try_from(epochs.len()).expect("len fits in u64");
    if covered != total {
        return Err(EpocError::SpanCountMismatch {
            declared: total,
            covered,
        });
    }

    // RLE clock offsets, if any. Trailing zeros are implicit.
    let mut idx: usize = 0;
    while !cursor.is_empty() && idx < epochs.len() {
        let value = read_sleb128(&mut cursor)?;
        let run_count_m1 = read_uleb128(&mut cursor)?;
        let run_count = usize::try_from(run_count_m1.saturating_add(1)).unwrap_or(usize::MAX);
        let end = idx.saturating_add(run_count).min(epochs.len());
        for slot in &mut epochs[idx..end] {
            slot.clock_offset_e12 = value;
        }
        idx = end;
    }

    Ok(Epoc { epochs })
}

fn decode_date(packed: u64) -> Result<(u16, u8, u8), EpocError> {
    let day_u64 = packed % 100;
    let month_u64 = (packed / 100) % 100;
    let year_part = packed / 10_000;

    let year: u16 = if year_part < 100 {
        // RINEX 2 two-digit year: 80..=99 → 1980..=1999, 0..=79 → 2000..=2079.
        let y = if year_part >= 80 {
            year_part + 1900
        } else {
            year_part + 2000
        };
        u16::try_from(y).map_err(|_| EpocError::BadDate(packed))?
    } else {
        u16::try_from(year_part).map_err(|_| EpocError::BadDate(packed))?
    };

    if !(1..=12).contains(&month_u64) || !(1..=31).contains(&day_u64) {
        return Err(EpocError::BadDate(packed));
    }

    // Casts safe by the range checks above.
    #[allow(clippy::cast_possible_truncation)]
    Ok((year, month_u64 as u8, day_u64 as u8))
}

fn decode_time(packed: u64) -> Result<(u8, u8, u64), EpocError> {
    const PER_HOUR: u64 = 100_000_000_000;
    const PER_MINUTE: u64 = 1_000_000_000;

    let hour_u64 = packed / PER_HOUR;
    let after_hour = packed % PER_HOUR;
    let minute_u64 = after_hour / PER_MINUTE;
    let seconds_e7 = after_hour % PER_MINUTE;

    if hour_u64 > 23 || minute_u64 > 59 {
        return Err(EpocError::BadTime(packed));
    }

    #[allow(clippy::cast_possible_truncation)]
    Ok((hour_u64 as u8, minute_u64 as u8, seconds_e7))
}

fn interval_to_e7(interval: i64) -> i64 {
    if interval < 0 {
        // Saturating because i64::MIN cannot be negated; real intervals are tiny.
        interval
            .checked_neg()
            .and_then(|n| n.checked_mul(10_000_000))
            .unwrap_or(i64::MAX)
    } else {
        interval
    }
}

#[allow(clippy::cast_possible_truncation)]
fn advance_time(hour: &mut u8, minute: &mut u8, seconds_e7: &mut u64, interval_e7: i64) {
    let new_s = i128::from(*seconds_e7) + i128::from(interval_e7);
    if new_s < 0 {
        *seconds_e7 = 0;
    } else {
        let new_s64 = u64::try_from(new_s).unwrap_or(u64::MAX);
        // Leap-second exception: 23:59:60.0 through 23:59:60.9999999 are
        // preserved as-is.  The spec forbids >= 23:59:61 and requires new
        // spans at day boundaries, so we don't need to carry past this.
        if *hour == 23 && *minute == 59 && new_s64 < 610_000_000 {
            *seconds_e7 = new_s64;
        } else if new_s64 >= 60 * 10_000_000 {
            let carry_minutes = new_s64 / 10_000_000 / 60;
            *seconds_e7 = new_s64 % (60 * 10_000_000);
            let new_minute = u64::from(*minute) + carry_minutes;
            if new_minute >= 60 {
                *hour = hour.saturating_add((new_minute / 60) as u8);
                *minute = (new_minute % 60) as u8;
            } else {
                *minute = new_minute as u8;
            }
        } else {
            *seconds_e7 = new_s64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_uleb(mut v: u64) -> Vec<u8> {
        let mut out = vec![];
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
        // ZigZag encode: (v << 1) ^ (v >> 63), as u64.
        #[allow(clippy::cast_sign_loss)]
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        enc_uleb(zz)
    }

    /// Build an EPOC payload from one or more spans, optionally followed by
    /// (value, run-count-minus-1) clock offset pairs.
    fn build_payload(
        total: u64,
        spans: &[(i64, u64, u64, u64)], // (interval, count_m1, date, time)
        clock: &[(i64, u64)],
    ) -> Vec<u8> {
        let mut out = enc_uleb(total);
        for (interval, count_m1, date, time) in spans {
            out.extend(enc_sleb(*interval));
            out.extend(enc_uleb(*count_m1));
            out.extend(enc_uleb(*date));
            out.extend(enc_uleb(*time));
        }
        for (value, count_m1) in clock {
            out.extend(enc_sleb(*value));
            out.extend(enc_uleb(*count_m1));
        }
        out
    }

    #[test]
    fn empty_epoc() {
        let payload = enc_uleb(0);
        let e = parse(&payload).unwrap();
        assert!(e.epochs.is_empty());
    }

    #[test]
    fn single_span_integer_interval() {
        // 10 epochs, 1 s interval (encoded as -1), starting 2020-07-18 00:00:00.
        let p = build_payload(10, &[(-1, 9, 20_200_718, 0)], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(e.epochs.len(), 10);
        for (i, ep) in e.epochs.iter().enumerate() {
            assert_eq!(ep.year, 2020);
            assert_eq!(ep.month, 7);
            assert_eq!(ep.day, 18);
            assert_eq!(ep.hour, 0);
            assert_eq!(ep.minute, 0);
            assert_eq!(ep.seconds_e7, (i as u64) * 10_000_000);
            assert_eq!(ep.clock_offset_e12, 0);
        }
    }

    #[test]
    fn single_span_fractional_interval() {
        // 4 epochs at 0.5 s starting at 00:00:00 → seconds 0.0, 0.5, 1.0, 1.5.
        let p = build_payload(4, &[(5_000_000, 3, 20_200_718, 0)], &[]);
        let e = parse(&p).unwrap();
        let secs: Vec<u64> = e.epochs.iter().map(|ep| ep.seconds_e7).collect();
        assert_eq!(secs, vec![0, 5_000_000, 10_000_000, 15_000_000]);
    }

    #[test]
    fn minute_rollover_on_exact_60() {
        // 70 epochs at 1 s starting at 00:00:00 → epoch 60 is 00:01:00.
        let p = build_payload(70, &[(-1, 69, 20_200_718, 0)], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(e.epochs[59].minute, 0);
        assert_eq!(e.epochs[59].seconds_e7, 59 * 10_000_000);
        assert_eq!(e.epochs[60].minute, 1);
        assert_eq!(e.epochs[60].seconds_e7, 0);
        assert_eq!(e.epochs[69].minute, 1);
        assert_eq!(e.epochs[69].seconds_e7, 9 * 10_000_000);
    }

    #[test]
    fn hour_rollover_on_exact_60_minute() {
        // Start at 00:59:50, advance 1 s for 20 epochs.
        // At i=10 we hit 01:00:00 (m += 1 → 60 → reset to 0, h += 1).
        let start_time = 59 * 1_000_000_000 + 50 * 10_000_000;
        let p = build_payload(20, &[(-1, 19, 20_200_718, start_time)], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(
            (e.epochs[9].hour, e.epochs[9].minute, e.epochs[9].seconds_e7),
            (0, 59, 59 * 10_000_000)
        );
        assert_eq!(
            (
                e.epochs[10].hour,
                e.epochs[10].minute,
                e.epochs[10].seconds_e7
            ),
            (1, 0, 0)
        );
    }

    #[test]
    fn leap_second_preserved_through_60_dot_9() {
        // Start at 23:59:60.0 (leap second), 0.5 s interval, 4 epochs.
        // The leap-second range 23:59:60.0 – 23:59:60.9999999 is preserved
        // without carrying.  23:59:61.0 is illegal per the spec, and
        // conformant encoders MUST split spans at leap seconds, but we
        // still handle it gracefully by carrying.
        let start = 23 * 100_000_000_000 + 59 * 1_000_000_000 + 60 * 10_000_000;
        let p = build_payload(4, &[(5_000_000, 3, 20_200_718, start)], &[]);
        let e = parse(&p).unwrap();
        // Epoch 0: 23:59:60.0 (leap second, preserved)
        assert_eq!(e.epochs[0].hour, 23);
        assert_eq!(e.epochs[0].minute, 59);
        assert_eq!(e.epochs[0].seconds_e7, 600_000_000);
        // Epoch 1: 23:59:60.5 (still in leap-second range, preserved)
        assert_eq!(e.epochs[1].hour, 23);
        assert_eq!(e.epochs[1].minute, 59);
        assert_eq!(e.epochs[1].seconds_e7, 605_000_000);
        // Epoch 2: 61.0 s >= 61 → carry → 24:00:01.0
        assert_eq!(e.epochs[2].hour, 24);
        assert_eq!(e.epochs[2].minute, 0);
        assert_eq!(e.epochs[2].seconds_e7, 10_000_000);
        // Epoch 3: 24:00:01.5
        assert_eq!(e.epochs[3].hour, 24);
        assert_eq!(e.epochs[3].minute, 0);
        assert_eq!(e.epochs[3].seconds_e7, 15_000_000);
    }

    #[test]
    fn multi_span() {
        // 5 epochs at 1s, then day boundary, then 5 more epochs at 1s.
        let span1 = (
            -1,
            4,
            20_200_718,
            23 * 100_000_000_000 + 59 * 1_000_000_000 + 55 * 10_000_000,
        );
        let span2 = (-1, 4, 20_200_719, 0);
        let p = build_payload(10, &[span1, span2], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(e.epochs.len(), 10);
        assert_eq!(
            (
                e.epochs[0].day,
                e.epochs[0].hour,
                e.epochs[0].minute,
                e.epochs[0].seconds_e7
            ),
            (18, 23, 59, 55 * 10_000_000)
        );
        assert_eq!(
            (
                e.epochs[4].day,
                e.epochs[4].hour,
                e.epochs[4].minute,
                e.epochs[4].seconds_e7
            ),
            (18, 23, 59, 59 * 10_000_000)
        );
        assert_eq!(
            (
                e.epochs[5].day,
                e.epochs[5].hour,
                e.epochs[5].minute,
                e.epochs[5].seconds_e7
            ),
            (19, 0, 0, 0)
        );
    }

    #[test]
    fn clock_offsets_basic() {
        // 5 epochs, two clock-offset runs: value=100 ×3, value=-50 ×2.
        let p = build_payload(5, &[(-1, 4, 20_200_718, 0)], &[(100, 2), (-50, 1)]);
        let e = parse(&p).unwrap();
        let offsets: Vec<i64> = e.epochs.iter().map(|ep| ep.clock_offset_e12).collect();
        assert_eq!(offsets, vec![100, 100, 100, -50, -50]);
    }

    #[test]
    fn clock_offsets_padded_with_zeros() {
        // 5 epochs but only one offset run covering 2 epochs → trailing 3 are zero.
        let p = build_payload(5, &[(-1, 4, 20_200_718, 0)], &[(42, 1)]);
        let e = parse(&p).unwrap();
        let offsets: Vec<i64> = e.epochs.iter().map(|ep| ep.clock_offset_e12).collect();
        assert_eq!(offsets, vec![42, 42, 0, 0, 0]);
    }

    #[test]
    fn clock_offsets_omitted_means_all_zero() {
        // RINEX 2 case: no clock offsets at all.
        let p = build_payload(3, &[(-1, 2, 20_200_718, 0)], &[]);
        let e = parse(&p).unwrap();
        for ep in &e.epochs {
            assert_eq!(ep.clock_offset_e12, 0);
        }
    }

    #[test]
    fn rinex2_year_disambiguation() {
        // year_part=99 → 1999, year_part=20 → 2020.
        let p = build_payload(1, &[(-1, 0, 99 * 10_000 + 100 + 2, 0)], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(e.epochs[0].year, 1999);
        assert_eq!(e.epochs[0].month, 1);
        assert_eq!(e.epochs[0].day, 2);

        let p = build_payload(1, &[(-1, 0, 20 * 10_000 + 7 * 100 + 18, 0)], &[]);
        let e = parse(&p).unwrap();
        assert_eq!(e.epochs[0].year, 2020);
    }

    #[test]
    fn span_count_mismatch_detected() {
        // Declare 10 epochs but span only covers 3.
        let p = build_payload(10, &[(-1, 2, 20_200_718, 0)], &[]);
        // First span supplies 3 epochs; loop will try to read another span
        // header from the (likely empty) cursor → either Leb error or
        // SpanCountMismatch on subsequent iterations. The exact failure
        // depends on the truncation point. Assert that we get *some* error.
        assert!(parse(&p).is_err());
    }

    fn try_load_epoc(name: &str) -> Option<Vec<u8>> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(name);
        let bytes = std::fs::read(path).ok()?;
        let file = crate::file::SrnxFile::parse(&bytes).ok()?;
        let sdir = file.sdir().ok().flatten()?;
        let chunk = file.chunk_at_offset(sdir.epoc_offset)?;
        Some(chunk.payload.to_vec())
    }

    #[test]
    fn fixture_at012000_epoc() {
        let Some(payload) = try_load_epoc("at012000.srnx") else {
            return;
        };
        let e = parse(&payload).unwrap();
        assert_eq!(e.epochs.len(), 86_400, "at012000 should have 86400 epochs");
        let first = &e.epochs[0];
        assert_eq!((first.year, first.month, first.day), (2020, 7, 18));
        assert_eq!((first.hour, first.minute, first.seconds_e7), (0, 0, 0));
        let last = e.epochs.last().unwrap();
        // 86400 seconds = 24:00:00, but encoder splits at midnight, so the
        // last epoch within this single span is 23:59:59 (1 s before next day).
        assert_eq!(
            (last.hour, last.minute, last.seconds_e7),
            (23, 59, 59 * 10_000_000)
        );
        // RINEX 2: clock offsets all zero.
        assert!(e.epochs.iter().all(|ep| ep.clock_offset_e12 == 0));
    }

    #[test]
    fn fixture_abmf_epoc() {
        let Some(payload) = try_load_epoc("abmf.srnx") else {
            return;
        };
        let e = parse(&payload).unwrap();
        assert_eq!(
            e.epochs.len(),
            2880,
            "abmf should have 2880 epochs (24h × 30s)"
        );
        let first = &e.epochs[0];
        assert_eq!((first.year, first.month, first.day), (2020, 7, 7));
        let last = e.epochs.last().unwrap();
        assert_eq!(
            (last.hour, last.minute, last.seconds_e7),
            (23, 59, 30 * 10_000_000)
        );
    }

    #[test]
    fn fixture_okcb1200_epoc() {
        let Some(payload) = try_load_epoc("okcb1200.srnx") else {
            return;
        };
        let e = parse(&payload).unwrap();
        assert_eq!(e.epochs.len(), 86_400);
    }

    #[test]
    fn fixture_sbst1200_epoc() {
        let Some(payload) = try_load_epoc("sbst1200.srnx") else {
            return;
        };
        let e = parse(&payload).unwrap();
        assert_eq!(e.epochs.len(), 86_400);
    }
}
