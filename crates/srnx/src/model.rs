//! Shared semantic model populated by the SRNX and RINEX text readers.
//!
//! Both formats express the same data — satellite observations per epoch —
//! but encode it very differently.  This module provides the canonical
//! in-memory representation used by the `srnx-diff` comparison tool.

use std::collections::BTreeMap;

use crate::rinex_header::{HeaderText, ObsCode, SatSystem};

/// Satellite identifier: system code byte (`b'G'`, `b'R'`, `b'E'`, …) + PRN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SatId {
    pub system: SatSystem,
    pub prn: u8,
}

impl SatId {
    #[must_use]
    pub fn new(system: SatSystem, prn: u8) -> Self {
        Self { system, prn }
    }
}

impl std::fmt::Display for SatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{:02}", self.system.as_char(), self.prn)
    }
}

/// One decoded scalar observation.
///
/// Physical value = `value * scale / 1_000`.  For RINEX v2 text files the
/// scale is always 1000 (millimeter/millicycle resolution).  SRNX files may
/// use a different scale; comparisons must normalize to the same units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obs {
    /// Scaled integer: `physical_value × (1000 / scale)`.
    pub value: i64,
    /// Scale factor × 1000.  1000 → physical × 1, 250 → physical × 0.25.
    pub scale: u32,
    /// Loss-of-lock indicator byte.  `b' '` or `b'0'` = OK / not known.
    pub lli: u8,
    /// Signal-strength indicator byte.  `b' '` or `b'0'` = not known.
    pub ssi: u8,
}

/// Calendar epoch metadata from an observation epoch header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRecord {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    /// Seconds within the minute as `s × 10⁷`.
    pub seconds_e7: u64,
    /// Additional picoseconds beyond `seconds_e7` (0–99 999), from the
    /// optional five-digit epoch extension introduced in RINEX 4.02.  Always
    /// zero for RINEX v2 and SRNX, whose epochs have 10⁻⁷ s resolution.
    pub extra_ps: u32,
    /// Epoch flag: 0 = OK, 1 = power failure, 2–5 = event, 6 = cycle slips.
    pub flag: u8,
    /// Receiver clock offset as `value × 10¹²`.  Zero if not supplied.
    pub clock_offset_e12: i64,
}

/// Raw text lines accompanying a special-event epoch record (flag 1–6).
///
/// For flags 1 and 6 these are the record's satellite continuation lines
/// (RINEX v2) and observation / cycle-slip lines.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub flag: u8,
    /// The verbatim lines that follow the epoch header.
    pub lines: Vec<String>,
}

/// Normalized in-memory representation of a RINEX observation file.
///
/// The SRNX reader and the RINEX v2 and v3/v4 text readers populate this
/// struct.
/// The observations map is indexed by `(SatId, ObsCode)`.  Each value is a
/// sorted `Vec<(usize, Obs)>` of `(epoch_index, observation)` pairs.  Only
/// epochs where the observation is present are listed; absent epochs have no
/// entry.  Epoch indices refer to positions in the `epochs` field.
pub struct ObsFile {
    /// RINEX header as raw text with label-based query helpers.
    /// Normalized to LF newlines.
    pub header: HeaderText,
    /// Observation epochs in file order (flag-0 records only; flag-1 and
    /// flag-6 records are events, as in SRNX).
    pub epochs: Vec<EpochRecord>,
    /// Special event records with their position (index into `epochs` of the
    /// epoch immediately following the event; equal to `epochs.len()` if the
    /// event appears after the last observation epoch).
    pub events: Vec<(usize, EventRecord)>,
    /// Per-(satellite, obs-code) sparse observation time series.
    ///
    /// Each inner `Vec` holds `(epoch_index, Obs)` pairs in ascending epoch
    /// order.  Epochs where the observation is absent are simply omitted.
    /// Use binary search on the first element for O(log n) lookup by epoch.
    pub observations: BTreeMap<(SatId, ObsCode), Vec<(usize, Obs)>>,
}
