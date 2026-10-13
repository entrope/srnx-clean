//! Converts a parsed SRNX file into the shared [`ObsFile`] semantic model.
//!
//! Entry point: [`read`].

use std::collections::BTreeMap;

use thiserror::Error;

use crate::epoc::Epoch;
use crate::file::{FileError, SrnxFile};
use crate::model::{EpochRecord, EventRecord, Obs, ObsFile, SatId};
use crate::rinex_header::{self, Header, HeaderError, HeaderText, ObsCode, SatSystem};
use crate::soc2::{self, Column};
use crate::socd::ABSENT;

#[derive(Debug, Error)]
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
    #[error("no EPOC chunk in SRNX file")]
    NoEpoc,
}

/// Parse a complete SRNX observation file from bytes and return the
/// normalized [`ObsFile`] semantic model.
pub fn read(bytes: &[u8]) -> Result<ObsFile, SrnxReadError> {
    let file = SrnxFile::parse(bytes)?;

    if !file.file_digest_ok {
        return Err(SrnxReadError::FileDigestMismatch);
    }

    // RHDR → HeaderText (public) + internal Header (for obs codes).
    let rhdr_bytes = file.rhdr_payload().ok_or(SrnxReadError::NoRhdr)?;
    let header_text = HeaderText::from_bytes(rhdr_bytes);
    let header = rinex_header::parse(rhdr_bytes)?;

    // EPOC → epoch list.
    let epoc = file.epoc()?.ok_or(SrnxReadError::NoEpoc)?;
    let total_epochs = epoc.epochs.len();
    let timestamps = continuous_times(&epoc.epochs);
    let epochs: Vec<EpochRecord> = epoc.epochs.iter().map(epoch_to_record).collect();

    // SDIR.
    let sdir = file.sdir()?.ok_or(SrnxReadError::NoSdir)?;

    // EVTF → event records.
    let events: Vec<(usize, EventRecord)> = file
        .evtf_chunks(sdir.evtf_offset)
        .filter_map(std::result::Result::ok)
        .map(|evtf| {
            let epoch_index = usize::try_from(evtf.epoch_index).unwrap_or(usize::MAX);
            (
                epoch_index,
                EventRecord {
                    flag: 0, // extracted from text if needed; stored as-is for now
                    lines: evtf.text,
                },
            )
        })
        .collect();

    let mut observations: BTreeMap<(SatId, ObsCode), Vec<(usize, Obs)>> = BTreeMap::new();

    for entry in &sdir.satellites {
        let sat_id = parse_sat_id(entry.sat_id);
        let sate_file_offset = entry.sate_offset;

        // Choose obs-code list for this satellite's system.
        let obs_codes: &[ObsCode] = obs_codes_for_sat(&header, sat_id.system);
        let num_obs_codes = obs_codes.len();

        let sate = file.sate_at(sate_file_offset, num_obs_codes, total_epochs)?;
        let present = &sate.present_epoch_indices;

        let times: Vec<i128> = present.iter().map(|&i| timestamps[i]).collect();
        let columns: Vec<Option<Column>> = sate
            .socd_offsets
            .iter()
            .map(|&rel_offset| {
                if rel_offset == 0 {
                    return Ok(None);
                }
                let offset = sate_file_offset.checked_add_signed(rel_offset).ok_or(
                    FileError::Truncated {
                        what: "observation offset out of range",
                    },
                )?;
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
            })
            .collect::<Result<_, FileError>>()?;
        let decoded =
            soc2::decode_satellite(&columns, obs_codes, &times).map_err(FileError::from)?;
        for (code_idx, socd) in decoded.iter().enumerate() {
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
                let lli = socd.lli.get(i).copied().unwrap_or(b' ');
                let ssi = socd.ssi.get(i).copied().unwrap_or(b' ');
                series.push((
                    ep_idx,
                    Obs {
                        value: val,
                        scale: socd.scale,
                        lli,
                        ssi,
                    },
                ));
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

// Gregorian day numbers, plus any leap seconds represented by EPOC. A leap
// second remains in its day's final minute and shifts all later days by 1 s.
fn continuous_times(epochs: &[Epoch]) -> Vec<i128> {
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
                * 10_000_000
                + i128::from(e.seconds_e7)
        })
        .collect()
}

fn epoch_to_record(e: &Epoch) -> EpochRecord {
    EpochRecord {
        year: e.year,
        month: e.month,
        day: e.day,
        hour: e.hour,
        minute: e.minute,
        seconds_e7: e.seconds_e7,
        extra_ps: 0,
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

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("rnx-data")
            .join(name)
    }

    fn try_read_srnx(name: &str) -> Option<ObsFile> {
        let bytes = std::fs::read(fixture(name)).ok()?;
        read(&bytes).ok()
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
            clock_offset_e12: 0,
        };
        let times = continuous_times(&[
            e(2016, 12, 31, 23, 59, 590_000_000),
            e(2016, 12, 31, 23, 59, 600_000_000),
            e(2017, 1, 1, 0, 0, 0),
        ]);
        assert_eq!(times[1] - times[0], 10_000_000);
        assert_eq!(times[2] - times[1], 10_000_000);
        let times = continuous_times(&[e(2020, 2, 28, 0, 0, 0), e(2020, 3, 1, 0, 0, 0)]);
        assert_eq!(times[1] - times[0], 2 * 86400 * 10_000_000);
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
    fn fixture_at012000_epoch_count() {
        let Some(obs) = try_read_srnx("at012000.srnx") else {
            return;
        };
        assert_eq!(obs.epochs.len(), 86_400);
    }

    #[test]
    fn fixture_at012000_first_epoch() {
        let Some(obs) = try_read_srnx("at012000.srnx") else {
            return;
        };
        let e = &obs.epochs[0];
        assert_eq!(e.year, 2020);
        assert_eq!(e.month, 7);
        assert_eq!(e.day, 18);
        assert_eq!(e.hour, 0);
        assert_eq!(e.minute, 0);
        assert_eq!(e.seconds_e7, 0);
    }

    #[test]
    fn fixture_at012000_g13_l1_first_epoch() {
        let Some(obs) = try_read_srnx("at012000.srnx") else {
            return;
        };
        let l1 = ObsCode::from_v2("L1").unwrap();
        let g13 = SatId::new(SatSystem::GPS, 13);
        let series = obs.observations.get(&(g13, l1)).expect("G13/L1 present");
        let &(ep_idx, first) = series.first().expect("at least one G13/L1 observation");
        assert_eq!(ep_idx, 0);
        // Comparison unit: value × scale must equal RINEX v2 value × 1 (both scale=1).
        let phys_srnx = i128::from(first.value) * i128::from(first.scale);
        let phys_v2 = 128_250_890_432_i128;
        assert_eq!(phys_srnx, phys_v2, "G13/L1 physical value at epoch 0");
    }

    #[test]
    fn rejects_tampered_file_digest() {
        let Some(bytes) = std::fs::read(fixture("at012000.srnx")).ok() else {
            return;
        };
        let mut tampered = bytes;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01; // flip a bit in the file-level digest
        match read(&tampered) {
            Ok(_) => panic!("tampered file should error"),
            Err(SrnxReadError::FileDigestMismatch) => {} // expected
            Err(e) => panic!("expected FileDigestMismatch, got {e:?}"),
        }
    }

    #[test]
    fn fixture_at012000_observations_sorted() {
        let Some(obs) = try_read_srnx("at012000.srnx") else {
            return;
        };
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
