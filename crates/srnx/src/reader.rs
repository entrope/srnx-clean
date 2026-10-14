//! Format-agnostic entry points: detect whether a file is SRNX, Compact
//! RINEX, or RINEX v2 or v3/v4 observation data, and dispatch to its reader.

use thiserror::Error;

use crate::model::{Catalog, ObsFile, SignalFilter};
use crate::{crinex, rinex2_obs, rinex3_obs, rinex_header, srnx_reader};

pub use crate::crinex::CrinexError;
pub use crate::rinex2_obs::Rinex2Error;
pub use crate::rinex3_obs::Rinex3Error;
pub use crate::srnx_reader::SrnxReadError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError {
    #[error(transparent)]
    Srnx(#[from] SrnxReadError),
    #[error(transparent)]
    Crinex(#[from] CrinexError),
    #[error(transparent)]
    Rinex2(#[from] Rinex2Error),
    #[error(transparent)]
    Rinex3(#[from] Rinex3Error),
}

/// An observation file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    Srnx,
    /// Compact RINEX (Hatanaka) 1.0, 3.0 or 3.1.
    Crinex,
    Rinex2,
    /// RINEX v3 or v4.
    Rinex3,
}

impl Format {
    /// Detect the format from a file's contents.  Anything that is not
    /// SRNX, Compact RINEX, or RINEX v3 or later is taken to be RINEX v2.
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"SRNX") {
            Self::Srnx
        } else if crinex::is_crinex(bytes) {
            Self::Crinex
        } else if rinex_header::sniff_version(bytes)
            .is_some_and(rinex_header::Version::is_v3_or_later)
        {
            Self::Rinex3
        } else {
            Self::Rinex2
        }
    }

    /// List the satellites and signals of a file in this format; see
    /// [`Catalog`].
    pub fn catalog(self, bytes: &[u8]) -> Result<Catalog, ReadError> {
        Ok(match self {
            Self::Srnx => srnx_reader::catalog(bytes)?,
            Self::Crinex => crinex::catalog(bytes)?,
            Self::Rinex2 => rinex2_obs::catalog(bytes)?,
            Self::Rinex3 => rinex3_obs::catalog(bytes)?,
        })
    }

    /// Read a file in this format, keeping only the satellite/signal pairs
    /// that `filter` selects (all of them for `None`).
    pub fn read(
        self,
        bytes: &[u8],
        filter: Option<SignalFilter<'_>>,
    ) -> Result<ObsFile, ReadError> {
        Ok(match self {
            Self::Srnx => srnx_reader::read(bytes, filter)?,
            Self::Crinex => crinex::parse(bytes, filter)?,
            Self::Rinex2 => rinex2_obs::parse(bytes, filter)?,
            Self::Rinex3 => rinex3_obs::parse(bytes, filter)?,
        })
    }
}

/// List the satellites and signals of an observation file in any supported
/// format; see [`Catalog`].
pub fn catalog(bytes: &[u8]) -> Result<Catalog, ReadError> {
    Format::detect(bytes).catalog(bytes)
}

/// Read an observation file in any supported format, keeping only the
/// satellite/signal pairs that `filter` selects (all of them for `None`).
///
/// ```no_run
/// use srnx::model::ObsCode;
///
/// let bytes = std::fs::read("abmf1190.26o")?;
/// let l1c = ObsCode::from_v3("L1C").unwrap();
/// let file = srnx::reader::read(&bytes, Some(&mut |_sat, code| code == l1c))?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn read(bytes: &[u8], filter: Option<SignalFilter<'_>>) -> Result<ObsFile, ReadError> {
    Format::detect(bytes).read(bytes, filter)
}
