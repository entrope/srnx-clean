//! Converts a parsed SRNX file into the shared [`ObsFile`] semantic model.
//!
//! Entry points: [`read`] and [`catalog`].

use std::collections::BTreeMap;

use thiserror::Error;

use crate::epoc::{Epoch, EpochChunkKind};
use crate::file::{FileError, SrnxFile};
use crate::model::{
    Catalog, EpochRecord, EventRecord, Obs, ObsFile, SatId, SignalFilter, SignalMasks,
};
use crate::rinex2_obs::byte_at;
use crate::rinex_header::{self, Header, HeaderError, HeaderText, ObsCode, SatSystem};
use crate::soc2::{Column, SatelliteColumns};
use crate::socd::ABSENT;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SrnxReadError {
    #[error("SRNX file error: {0}")]
    File(#[from] FileError),
    #[error("RINEX header error: {0}")]
    Header(#[from] HeaderError),
    #[error("file-level digest mismatch")]
    FileDigestMismatch,
    #[error("no RHDR chunk in SRNX file")]
    NoRhdr,
    #[error("no SDIR chunk in SRNX file")]
    NoSdir,
    #[error("no EPOC or EPO2 chunk in SRNX file")]
    NoEpoc,
    #[error("EPO2 chunk spans 2^63 picoseconds or more")]
    EpochRangeTooLarge,
    #[error("scaled observation value does not fit in 64 bits")]
    ValueOverflow,
}

/// List the satellites in an SRNX file's `SDIR` chunk, each with the
/// observation codes that its `SATE` chunk gives observations for.
pub fn catalog(bytes: &[u8]) -> Result<Catalog, SrnxReadError> {
    let file = open(bytes)?;
    let rhdr_bytes = file.rhdr_payload().ok_or(SrnxReadError::NoRhdr)?;
    let header = rinex_header::parse(rhdr_bytes)?;
    let sdir = file.sdir()?.ok_or(SrnxReadError::NoSdir)?;
    let mut satellites = Vec::with_capacity(sdir.satellites.len());
    for entry in &sdir.satellites {
        let sat_id = parse_sat_id(entry.sat_id);
        let obs_codes = obs_codes_for_sat(&header, sat_id.system);
        let offsets = file.sate_offsets_at(entry.sate_offset, obs_codes.len())?;
        let observed = obs_codes
            .iter()
            .zip(&offsets)
            .filter(|&(_, &offset)| offset != 0)
            .map(|(&code, _)| code)
            .collect();
        satellites.push((sat_id, observed));
    }
    Ok(Catalog {
        codes_by_system: header.obs_codes.by_system,
        satellites: Some(satellites),
    })
}

/// Parse a complete SRNX observation file from bytes and return the
/// normalized [`ObsFile`] semantic model, keeping only the satellite/signal
/// pairs that `filter` selects (all of them for `None`).
///
/// Only the selected observation chunks, and those they reference for
/// prediction, are decoded.
pub fn read(bytes: &[u8], filter: Option<SignalFilter<'_>>) -> Result<ObsFile, SrnxReadError> {
    let file = open(bytes)?;

    // RHDR → HeaderText (public) + internal Header (for obs codes).
    let rhdr_bytes = file.rhdr_payload().ok_or(SrnxReadError::NoRhdr)?;
    let header_text = HeaderText::from_bytes(rhdr_bytes);
    let header = rinex_header::parse(rhdr_bytes)?;

    // EPOC or EPO2 → epoch list.
    let epoc = file.epoc()?.ok_or(SrnxReadError::NoEpoc)?;
    let units_per_second = epoc.kind.units_per_second();
    let total_epochs = epoc.epochs.len();
    let timestamps = continuous_times(&epoc.epochs, units_per_second);
    check_epoch_range(epoc.kind, &timestamps)?;
    let epochs: Vec<EpochRecord> = epoc.epochs.iter().map(epoch_to_record).collect();

    // SDIR.
    let sdir = file.sdir()?.ok_or(SrnxReadError::NoSdir)?;

    // EVTF → event records.  The text starts with the epoch line, whose
    // flag is in column 29 (v2) or 32 (v3/v4); like the RINEX readers, keep
    // only the lines after it.
    let flag_col = if header.version.is_v3_or_later() {
        31
    } else {
        28
    };
    let events: Vec<(usize, EventRecord)> = file
        .evtf_chunks(sdir.evtf_offset)
        .filter_map(std::result::Result::ok)
        .map(|evtf| {
            let epoch_index = usize::try_from(evtf.epoch_index).unwrap_or(usize::MAX);
            let mut lines = evtf.text.iter().map(|l| l.trim_end_matches('\n'));
            let flag = lines.next().map_or(0, |l| match byte_at(l, flag_col) {
                b @ b'0'..=b'9' => b - b'0',
                _ => 0,
            });
            let lines = lines.map(str::to_owned).collect();
            (epoch_index, EventRecord { flag, lines })
        })
        .collect();

    let mut observations: BTreeMap<(SatId, ObsCode), Vec<(usize, Obs)>> = BTreeMap::new();
    let mut masks = SignalMasks::new(filter);

    for entry in &sdir.satellites {
        let sat_id = parse_sat_id(entry.sat_id);
        let sate_file_offset = entry.sate_offset;

        // Choose obs-code list for this satellite's system.
        let obs_codes: &[ObsCode] = obs_codes_for_sat(&header, sat_id.system);
        let num_obs_codes = obs_codes.len();
        let keep = masks.get(sat_id, obs_codes);
        if keep.is_some_and(|keep| !keep.contains(&true)) {
            continue;
        }

        let sate = file.sate_at(sate_file_offset, num_obs_codes, total_epochs)?;
        let present = &sate.present_epoch_indices;

        let times: Vec<i128> = present.iter().map(|&i| timestamps[i]).collect();
        let mut load = |code_idx: usize| -> Result<Option<Column>, FileError> {
            let rel_offset = sate.socd_offsets[code_idx];
            if rel_offset == 0 {
                return Ok(None);
            }
            let offset =
                sate_file_offset
                    .checked_add_signed(rel_offset)
                    .ok_or(FileError::Truncated {
                        what: "observation offset out of range",
                    })?;
            let chunk = file.chunk_at_offset(offset).ok_or(FileError::Truncated {
                what: "observation chunk at advertised offset",
            })?;
            match &chunk.fourcc {
                b"SOCD" => Ok(Some(Column::Socd(file.socd_at(offset)?))),
                b"SOC2" => Ok(Some(Column::Soc2(file.soc2_at(offset)?))),
                _ => Err(FileError::Truncated {
                    what: "observation FOURCC mismatch",
                }),
            }
        };
        let mut columns = SatelliteColumns::new(obs_codes, &times, units_per_second);
        for code_idx in 0..num_obs_codes {
            if keep.map_or(true, |keep| keep[code_idx]) {
                columns.decode(code_idx, &mut load)?;
            }
        }
        for (code_idx, socd) in columns.into_columns().into_iter().enumerate() {
            let Some(socd) = socd else {
                continue;
            };
            let obs_code = obs_codes[code_idx];

            let series = observations.entry((sat_id, obs_code)).or_default();
            let n = present.len().min(socd.values.len());
            for (i, &ep_idx) in present.iter().enumerate().take(n) {
                let val = socd.values[i];
                if val == ABSENT {
                    continue;
                }
                let value = val
                    .checked_mul(i64::from(socd.scale))
                    .ok_or(SrnxReadError::ValueOverflow)?;
                let lli = socd.lli.get(i).copied().unwrap_or(b' ');
                let ssi = socd.ssi.get(i).copied().unwrap_or(b' ');
                series.push((ep_idx, Obs { value, lli, ssi }));
            }
        }
    }

    Ok(ObsFile {
        header: header_text,
        epochs,
        events,
        observations,
    })
}

/// Parse the file framing and check the file-level digest.
fn open(bytes: &[u8]) -> Result<SrnxFile<'_>, SrnxReadError> {
    let file = SrnxFile::parse(bytes)?;
    if !file.file_digest_ok {
        return Err(SrnxReadError::FileDigestMismatch);
    }
    Ok(file)
}

// Gregorian day numbers, plus any leap seconds represented by the epoch
// chunk, in units of `1 / units_per_second` s. A leap second remains in its
// day's final minute and shifts all later days by 1 s.
fn continuous_times(epochs: &[Epoch], units_per_second: u64) -> Vec<i128> {
    let mut leap_days = std::collections::BTreeSet::new();
    let day_number = |e: &Epoch| {
        let year = i128::from(e.year) - 1;
        let mut days = year * 365 + year / 4 - year / 100 + year / 400;
        for month in 1..e.month {
            days += match month {
                4 | 6 | 9 | 11 => 30,
                2 => {
                    if e.year % 4 == 0 && (e.year % 100 != 0 || e.year % 400 == 0) {
                        29
                    } else {
                        28
                    }
                }
                _ => 31,
            };
        }
        days + i128::from(e.day) - 1
    };
    for e in epochs {
        if e.hour == 23 && e.minute == 59 && e.seconds_e7 >= 600_000_000 {
            leap_days.insert(day_number(e));
        }
    }
    epochs
        .iter()
        .map(|e| {
            let day = day_number(e);
            let leaps = i128::try_from(leap_days.range(..day).count()).unwrap();
            (day * 86_400 + leaps + i128::from(e.hour) * 3600 + i128::from(e.minute) * 60)
                * i128::from(units_per_second)
                + i128::from(e.seconds_in(units_per_second))
        })
        .collect()
}

// EPO2 requires its timestamp differences to fit in a signed 64-bit integer.
fn check_epoch_range(kind: EpochChunkKind, timestamps: &[i128]) -> Result<(), SrnxReadError> {
    if kind == EpochChunkKind::Epo2 {
        if let (Some(min), Some(max)) = (timestamps.iter().min(), timestamps.iter().max()) {
            if max - min >= 1 << 63 {
                return Err(SrnxReadError::EpochRangeTooLarge);
            }
        }
    }
    Ok(())
}

fn epoch_to_record(e: &Epoch) -> EpochRecord {
    EpochRecord {
        year: e.year,
        month: e.month,
        day: e.day,
        hour: e.hour,
        minute: e.minute,
        seconds_e7: e.seconds_e7,
        extra_ps: e.extra_ps,
        flag: 0,
        clock_offset_e12: e.clock_offset_e12,
    }
}

/// Parse a 3-byte SRNX satellite identifier into a [`SatId`].
///
/// Format: `[system_char, prn_tens, prn_ones]`, e.g. `b"G13"` → GPS PRN 13.
/// A space in the system byte is treated as GPS (RINEX v2 convention).
fn parse_sat_id(bytes: [u8; 3]) -> SatId {
    let system = if bytes[0] == b' ' {
        SatSystem::GPS
    } else {
        SatSystem(bytes[0])
    };
    let tens = if bytes[1].is_ascii_digit() {
        bytes[1] - b'0'
    } else {
        0
    };
    let ones = if bytes[2].is_ascii_digit() {
        bytes[2] - b'0'
    } else {
        0
    };
    SatId::new(system, tens * 10 + ones)
}

/// Return the observation-code slice to use for `system`.
///
/// For v2 MIXED files, all satellites share the single MIXED list.
/// For v3 files, each satellite system has its own list.
fn obs_codes_for_sat(header: &Header, system: SatSystem) -> &[ObsCode] {
    if let Some(codes) = header.obs_codes.for_system(SatSystem::MIXED) {
        return codes;
    }
    if let Some(codes) = header.obs_codes.for_system(system) {
        return codes;
    }
    header
        .obs_codes
        .by_system
        .first()
        .map_or(&[], |(_, v)| v.as_slice())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> ObsFile {
        read(&crate::test_data(name), None).unwrap()
    }

    #[test]
    fn timestamps_cross_month_year_and_leap_second() {
        let e = |year, month, day, hour, minute, seconds_e7| Epoch {
            year,
            month,
            day,
            hour,
            minute,
            seconds_e7,
            extra_ps: 0,
            clock_offset_e12: 0,
        };
        let u7 = EpochChunkKind::Epoc.units_per_second();
        let times = continuous_times(
            &[
                e(2016, 12, 31, 23, 59, 590_000_000),
                e(2016, 12, 31, 23, 59, 600_000_000),
                e(2017, 1, 1, 0, 0, 0),
            ],
            u7,
        );
        assert_eq!(times[1] - times[0], 10_000_000);
        assert_eq!(times[2] - times[1], 10_000_000);
        let times = continuous_times(&[e(2020, 2, 28, 0, 0, 0), e(2020, 3, 1, 0, 0, 0)], u7);
        assert_eq!(times[1] - times[0], 2 * 86400 * 10_000_000);

        // EPO2: picoseconds, including the sub-1e-7 s digits.
        let mut late = e(2016, 12, 31, 23, 59, 609_999_999);
        late.extra_ps = 99_999;
        let times = continuous_times(
            &[late, e(2017, 1, 1, 0, 0, 0)],
            EpochChunkKind::Epo2.units_per_second(),
        );
        assert_eq!(times[1] - times[0], 1);
    }

    #[test]
    fn parse_sat_id_gps() {
        let id = parse_sat_id(*b"G13");
        assert_eq!(id.system, SatSystem::GPS);
        assert_eq!(id.prn, 13);
    }

    #[test]
    fn parse_sat_id_glonass() {
        let id = parse_sat_id(*b"R10");
        assert_eq!(id.system, SatSystem::GLONASS);
        assert_eq!(id.prn, 10);
    }

    #[test]
    fn parse_sat_id_single_digit() {
        let id = parse_sat_id(*b"E01");
        assert_eq!(id.system, SatSystem::GALILEO);
        assert_eq!(id.prn, 1);
    }

    #[test]
    fn fixture_at01_epoch_count() {
        let obs = fixture("at01_v211_1hz.srnx");
        assert_eq!(obs.epochs.len(), 120);
    }

    #[test]
    fn fixture_at01_first_epoch() {
        let obs = fixture("at01_v211_1hz.srnx");
        let e = &obs.epochs[0];
        assert_eq!(e.year, 2020);
        assert_eq!(e.month, 7);
        assert_eq!(e.day, 18);
        assert_eq!(e.hour, 0);
        assert_eq!(e.minute, 0);
        assert_eq!(e.seconds_e7, 0);
    }

    #[test]
    fn fixture_at01_g13_l1_first_epoch() {
        let obs = fixture("at01_v211_1hz.srnx");
        let l1 = ObsCode::from_v2("L1").unwrap();
        let g13 = SatId::new(SatSystem::GPS, 13);
        let series = obs.observations.get(&(g13, l1)).expect("G13/L1 present");
        let &(ep_idx, first) = series.first().expect("at least one G13/L1 observation");
        assert_eq!(ep_idx, 0);
        assert_eq!(
            first.value, 128_250_890_432,
            "G13/L1 milli-cycles at epoch 0"
        );
    }

    #[test]
    fn rejects_tampered_file_digest() {
        let mut tampered = crate::test_data("at01_v211_1hz.srnx");
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01; // flip a bit in the file-level digest
        match read(&tampered, None) {
            Ok(_) => panic!("tampered file should error"),
            Err(SrnxReadError::FileDigestMismatch) => {} // expected
            Err(e) => panic!("expected FileDigestMismatch, got {e:?}"),
        }
    }

    #[test]
    fn fixture_at01_observations_sorted() {
        let obs = fixture("at01_v211_1hz.srnx");
        let num_epochs = obs.epochs.len();
        for (key, series) in &obs.observations {
            assert!(
                series.windows(2).all(|w| w[0].0 < w[1].0),
                "series for {}/{} not sorted",
                key.0,
                key.1.as_str()
            );
            assert!(
                series.last().map_or(0, |&(i, _)| i) < num_epochs,
                "epoch index out of range for {}/{}",
                key.0,
                key.1.as_str()
            );
        }
    }
}
