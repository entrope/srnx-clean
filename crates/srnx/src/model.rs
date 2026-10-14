//! Shared semantic model populated by the SRNX and RINEX text readers.
//!
//! Both formats express the same data — satellite observations per epoch —
//! but encode it very differently.  This module provides the canonical
//! in-memory representation used by the `srnx-diff` comparison tool.

use std::collections::{BTreeMap, HashMap};

use crate::rinex_header::{Header, HeaderText, ObsCode, SatSystem};

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

/// The satellites and signals an observation file may contain, as known
/// without decoding its observations.
///
/// For SRNX files the satellites come from the `SDIR` chunk and each
/// satellite's signals from its `SATE` chunk: the observation codes with
/// at least one observation.  For RINEX and Compact RINEX files both come
/// from the header: the satellites from `PRN / # OF OBS` records, and each
/// satellite's signals are the observation codes declared for its system,
/// whether or not the satellite has any observations of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Observation codes declared in the header, per satellite system, in
    /// header order.  RINEX v2 files have a single list under
    /// [`SatSystem::MIXED`].
    pub codes_by_system: Vec<(SatSystem, Vec<ObsCode>)>,
    /// Satellites in file order, each with the observation codes it may
    /// carry in header order.  `None` when the file does not list its
    /// satellites: a RINEX or Compact RINEX header without
    /// `PRN / # OF OBS` records.
    pub satellites: Option<Vec<(SatId, Vec<ObsCode>)>>,
}

impl Catalog {
    /// The catalog of a RINEX header, given the code list the reader uses
    /// for each satellite system.
    pub(crate) fn from_header<'h>(
        header: &'h Header,
        codes_for: impl Fn(SatSystem) -> &'h [ObsCode],
    ) -> Self {
        Self {
            codes_by_system: header.obs_codes.by_system.clone(),
            satellites: (!header.satellites.is_empty()).then(|| {
                header
                    .satellites
                    .iter()
                    .map(|&sat| (sat, codes_for(sat.system).to_vec()))
                    .collect()
            }),
        }
    }
}

/// A caller's choice of satellite/signal pairs to read: `true` keeps the
/// pair's observations.  Readers take an `Option<SignalFilter>`, where
/// `None` keeps everything.
///
/// A reader calls the filter at most once per pair, with every observation
/// code declared for the satellite's system, before it reads that
/// satellite's observations.  The RINEX and Compact RINEX readers call it
/// for the satellites in `PRN / # OF OBS` records right after the header.
pub type SignalFilter<'a> = &'a mut dyn FnMut(SatId, ObsCode) -> bool;

/// A [`SignalFilter`] memoized as one mask per satellite over the
/// observation-code list the reader uses for it.
pub(crate) struct SignalMasks<'f> {
    filter: Option<SignalFilter<'f>>,
    masks: HashMap<SatId, Vec<bool>>,
}

impl<'f> SignalMasks<'f> {
    pub(crate) fn new(filter: Option<SignalFilter<'f>>) -> Self {
        Self {
            filter,
            masks: HashMap::new(),
        }
    }

    /// Which of `codes` to keep for `sat`; `None` keeps them all.  A
    /// satellite always gets the same code list, so the first call's mask
    /// is reused.
    pub(crate) fn get(&mut self, sat: SatId, codes: &[ObsCode]) -> Option<&[bool]> {
        let filter = self.filter.as_mut()?;
        let mask = self
            .masks
            .entry(sat)
            .or_insert_with(|| codes.iter().map(|&code| filter(sat, code)).collect());
        Some(mask)
    }
}
