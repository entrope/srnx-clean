//! Format-agnostic entry points: detect whether a file is SRNX, Compact
//! RINEX, or RINEX v2 or v3/v4 observation data, and dispatch to its reader.

use thiserror::Error;

use crate::model::{Catalog, ObsCode, ObsFile, SatId, SignalFilter};
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
    #[error("not an SRNX, Compact RINEX or RINEX observation file")]
    UnrecognizedFormat,
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
    /// Detect the format from a file's contents, or `None` if the file
    /// does not start like any supported format.
    ///
    /// This looks only at the start of the file: the `SRNX` FOURCC, or a
    /// first line with the `CRINEX VERS   / TYPE` or `RINEX VERSION / TYPE`
    /// label.  A RINEX file of another type (navigation, meteorological, ...)
    /// is detected as RINEX and rejected by the reader.
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"SRNX") {
            Some(Self::Srnx)
        } else if crinex::is_crinex(bytes) {
            Some(Self::Crinex)
        } else if starts_with_rinex_version(bytes) {
            if rinex_header::sniff_version(bytes).is_some_and(rinex_header::Version::is_v3_or_later)
            {
                Some(Self::Rinex3)
            } else {
                Some(Self::Rinex2)
            }
        } else {
            None
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

    /// Read a file in this format.
    pub fn read(self, bytes: &[u8]) -> Result<ObsFile, ReadError> {
        self.read_with(bytes, None)
    }

    /// Read a file in this format, keeping only the satellite/signal pairs
    /// that `filter` selects; see [`read_filtered`].
    pub fn read_filtered(
        self,
        bytes: &[u8],
        mut filter: impl FnMut(SatId, ObsCode) -> bool,
    ) -> Result<ObsFile, ReadError> {
        self.read_with(bytes, Some(&mut filter))
    }

    fn read_with(
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

/// `true` if the first line of `bytes` is a `RINEX VERSION / TYPE` record:
/// no line break in columns 1–60, the label in columns 61–80, then a line
/// break.
fn starts_with_rinex_version(bytes: &[u8]) -> bool {
    let Some(content) = bytes.get(..60) else {
        return false;
    };
    !content.iter().any(|&b| b == b'\n' || b == b'\r')
        && bytes[60..].starts_with(b"RINEX VERSION / TYPE")
        && matches!(bytes.get(80..), Some([b'\n', ..] | [b'\r', b'\n', ..]))
}

fn detect(bytes: &[u8]) -> Result<Format, ReadError> {
    Format::detect(bytes).ok_or(ReadError::UnrecognizedFormat)
}

/// List the satellites and signals of an observation file in any supported
/// format; see [`Catalog`].
pub fn catalog(bytes: &[u8]) -> Result<Catalog, ReadError> {
    detect(bytes)?.catalog(bytes)
}

/// Read an observation file in any supported format.
///
/// ```no_run
/// let bytes = std::fs::read("abmf1190.26o")?;
/// let file = srnx::reader::read(&bytes)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn read(bytes: &[u8]) -> Result<ObsFile, ReadError> {
    detect(bytes)?.read(bytes)
}

/// Read an observation file in any supported format, keeping only the
/// satellite/signal pairs for which `filter` returns `true`.
///
/// The reader calls `filter` at most once per pair, with every observation
/// code declared for the satellite's system, before it reads that
/// satellite's observations.  The RINEX and Compact RINEX readers call it
/// for the satellites in `PRN / # OF OBS` records right after the header.
/// Observations that are not selected are skipped rather than decoded, so
/// a narrow filter makes reading faster.
///
/// ```no_run
/// use srnx::model::ObsCode;
///
/// let bytes = std::fs::read("abmf1190.26o")?;
/// let l1c = ObsCode::from_v3("L1C").unwrap();
/// let file = srnx::reader::read_filtered(&bytes, |_sat, code| code == l1c)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn read_filtered(
    bytes: &[u8],
    filter: impl FnMut(SatId, ObsCode) -> bool,
) -> Result<ObsFile, ReadError> {
    detect(bytes)?.read_filtered(bytes, filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first line with `content` in columns 1–60 and `label` in 61–80.
    fn line(content: &str, label: &str) -> String {
        format!("{content:<60}{label:<20}")
    }

    #[test]
    fn detects_rinex_from_first_line() {
        let v2 = line(
            "     2.11           OBSERVATION DATA    G",
            "RINEX VERSION / TYPE",
        );
        let v3 = line(
            "     3.04           OBSERVATION DATA    M",
            "RINEX VERSION / TYPE",
        );
        let v4 = line(
            "     4.02           OBSERVATION DATA    M",
            "RINEX VERSION / TYPE",
        );
        assert_eq!(
            Format::detect(format!("{v2}\n").as_bytes()),
            Some(Format::Rinex2)
        );
        assert_eq!(
            Format::detect(format!("{v2}\r\n").as_bytes()),
            Some(Format::Rinex2)
        );
        assert_eq!(
            Format::detect(format!("{v3}\n").as_bytes()),
            Some(Format::Rinex3)
        );
        assert_eq!(
            Format::detect(format!("{v4}\r\n").as_bytes()),
            Some(Format::Rinex3)
        );
        // An unreadable version number is left for the v2 reader to report.
        let bad = line(
            "     x.yz           OBSERVATION DATA    G",
            "RINEX VERSION / TYPE",
        );
        assert_eq!(
            Format::detect(format!("{bad}\n").as_bytes()),
            Some(Format::Rinex2)
        );
    }

    #[test]
    fn rejects_files_not_starting_with_a_version_record() {
        let v3 = line(
            "     3.04           OBSERVATION DATA    M",
            "RINEX VERSION / TYPE",
        );
        let cases: [Vec<u8>; 8] = [
            b"".to_vec(),
            b"hello world\n".to_vec(),
            vec![0xff; 200],
            b"BZ3v1\0\0\xf0\x1f".to_vec(),
            // No line break after the label.
            v3.clone().into_bytes(),
            format!("{v3}   \n").into_bytes(),
            // Line break before column 61.
            format!("\n{v3}\n").into_bytes(),
            // Version record that is not the first line.
            format!("{}\n{v3}\n", line("", "COMMENT")).into_bytes(),
        ];
        for bytes in &cases {
            assert_eq!(
                Format::detect(bytes),
                None,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
            assert!(matches!(read(bytes), Err(ReadError::UnrecognizedFormat)));
            assert!(matches!(catalog(bytes), Err(ReadError::UnrecognizedFormat)));
        }
    }
}
