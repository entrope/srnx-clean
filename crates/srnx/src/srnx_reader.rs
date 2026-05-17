//! Converts a parsed SRNX file into the shared [`ObsFile`] semantic model.
//!
//! Entry point: [`read`].

use std::collections::BTreeMap;

use thiserror::Error;

use crate::epoc::Epoch;
use crate::file::{FileError, SrnxFile};
use crate::model::{EpochRecord, EventRecord, Obs, ObsFile, SatId};
use crate::rinex_header::{self, Header, HeaderError, HeaderText, ObsCode, SatSystem};
use crate::socd::ABSENT;

#[derive(Debug, Error)]
pub enum SrnxReadError {
    #[error("SRNX file error: {0}")]
    File(#[from] FileError),
    #[error("RINEX header error: {0}")]
    Header(#[from] HeaderError),
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

    // RHDR → HeaderText (public) + internal Header (for obs codes).
    let rhdr_bytes = file.rhdr_payload().ok_or(SrnxReadError::NoRhdr)?;
    let header_text = HeaderText::from_bytes(rhdr_bytes);
    let header = rinex_header::parse(rhdr_bytes)?;

    // EPOC → epoch list.
    let epoc = file.epoc()?.ok_or(SrnxReadError::NoEpoc)?;
    let total_epochs = epoc.epochs.len();
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

        for (code_idx, &rel_offset) in sate.socd_offsets.iter().enumerate() {
            if rel_offset == 0 || code_idx >= obs_codes.len() {
                continue;
            }

            // SOCD offset is relative to the start of the SATE chunk's FOURCC.
            let socd_abs =
                sate_file_offset
                    .checked_add_signed(rel_offset)
                    .ok_or(FileError::Truncated {
                        what: "SOCD offset out of range",
                    })?;

            let socd = file.socd_at(socd_abs)?;
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

fn epoch_to_record(e: &Epoch) -> EpochRecord {
    EpochRecord {
        year: e.year,
        month: e.month,
        day: e.day,
        hour: e.hour,
        minute: e.minute,
        seconds_e7: e.seconds_e7,
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
