//! Semantic comparison of two [`ObsFile`] instances.
//!
//! Entry point: [`compare`], which returns a `Vec<String>` of human-readable
//! difference descriptions (empty when the files are equivalent).
//!
//! This module is used by both the `srnx-diff` CLI and the corpus integration
//! tests so the comparison logic lives in exactly one place.

use std::collections::BTreeSet;

use crate::model::{EpochRecord, Obs, ObsFile};

// ---------------------------------------------------------------------------
// Public helpers (also useful standalone)
// ---------------------------------------------------------------------------

/// Format an epoch record as an ISO-like timestamp string.
#[must_use]
pub fn fmt_epoch(e: &EpochRecord) -> String {
    let whole = e.seconds_e7 / 10_000_000;
    let frac = e.seconds_e7 % 10_000_000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}",
        e.year, e.month, e.day, e.hour, e.minute, whole, frac
    )
}

/// Compare two epoch records on timestamp fields only (ignores flag and
/// clock offset).
#[must_use]
pub fn epochs_eq(a: &EpochRecord, b: &EpochRecord) -> bool {
    a.year == b.year
        && a.month == b.month
        && a.day == b.day
        && a.hour == b.hour
        && a.minute == b.minute
        && a.seconds_e7 == b.seconds_e7
}

/// Treat `b'0'` and `b' '` as equivalent (both mean "no information").
#[inline]
#[must_use]
pub fn norm_indicator(b: u8) -> u8 {
    if b == b'0' {
        b' '
    } else {
        b
    }
}

/// Compare two observations on physical value, LLI, and SSI.
///
/// Physical equivalence uses `value × scale` as the comparison key so that
/// different scale factors that represent the same physical quantity compare
/// equal.
#[must_use]
pub fn obs_eq(a: Obs, b: Obs) -> bool {
    let phys_a = i128::from(a.value) * i128::from(a.scale);
    let phys_b = i128::from(b.value) * i128::from(b.scale);
    phys_a == phys_b
        && norm_indicator(a.lli) == norm_indicator(b.lli)
        && norm_indicator(a.ssi) == norm_indicator(b.ssi)
}

// ---------------------------------------------------------------------------
// Full-file comparison
// ---------------------------------------------------------------------------

/// Maximum number of individual differences to report before stopping.
pub const MAX_DIFFS: usize = 50;

/// Compare two observation files and return a list of human-readable
/// difference descriptions.
///
/// Returns an empty `Vec` when the files are semantically equivalent.
/// At most [`MAX_DIFFS`] differences are collected.
#[must_use]
pub fn compare(a: &ObsFile, b: &ObsFile, label_a: &str, label_b: &str) -> Vec<String> {
    let mut out = Vec::new();

    compare_headers(a, b, label_a, label_b, &mut out);
    compare_epochs(a, b, label_a, label_b, &mut out);
    compare_observations(a, b, label_a, label_b, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Header comparison
// ---------------------------------------------------------------------------

fn compare_headers(a: &ObsFile, b: &ObsFile, label_a: &str, label_b: &str, out: &mut Vec<String>) {
    if out.len() >= MAX_DIFFS {
        return;
    }

    // HeaderText is already LF-normalized.
    let a_text = a.header.as_str();
    let b_text = b.header.as_str();

    if a_text != b_text {
        let a_lines = a_text.lines().count();
        let b_lines = b_text.lines().count();
        out.push(format!(
            "header text differs: {label_a}={a_lines} lines {label_b}={b_lines} lines"
        ));
    }
}

fn compare_epochs(
    a: &ObsFile,
    b: &ObsFile,
    label_a: &str,
    label_b: &str,
    out: &mut Vec<String>,
) {
    let na = a.epochs.len();
    let nb = b.epochs.len();
    if na != nb {
        out.push(format!(
            "epoch count: {label_a}={na}  {label_b}={nb}"
        ));
    }

    let common = na.min(nb);
    for i in 0..common {
        if out.len() >= MAX_DIFFS {
            break;
        }
        let ea = &a.epochs[i];
        let eb = &b.epochs[i];
        if !epochs_eq(ea, eb) {
            out.push(format!(
                "epoch[{i}] timestamp: {} vs {}",
                fmt_epoch(ea),
                fmt_epoch(eb)
            ));
        } else if ea.clock_offset_e12 != eb.clock_offset_e12 {
            out.push(format!(
                "epoch[{i}] clock offset: {} vs {} (×10⁻¹²)",
                ea.clock_offset_e12, eb.clock_offset_e12
            ));
        }
    }
}

fn compare_observations(
    a: &ObsFile,
    b: &ObsFile,
    label_a: &str,
    label_b: &str,
    out: &mut Vec<String>,
) {
    let keys_a: BTreeSet<_> = a.observations.keys().collect();
    let keys_b: BTreeSet<_> = b.observations.keys().collect();

    for key in keys_a.union(&keys_b) {
        if out.len() >= MAX_DIFFS {
            break;
        }
        match (a.observations.get(key), b.observations.get(key)) {
            (None, _) => out.push(format!(
                "{}/{}: only in {label_b}",
                key.0,
                key.1.as_str()
            )),
            (_, None) => out.push(format!(
                "{}/{}: only in {label_a}",
                key.0,
                key.1.as_str()
            )),
            (Some(sa), Some(sb)) => {
                compare_series(**key, sa, sb, a, b, label_a, label_b, out);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_series(
    key: (crate::model::SatId, crate::rinex_header::ObsCode),
    sa: &[(usize, Obs)],
    sb: &[(usize, Obs)],
    file_a: &ObsFile,
    file_b: &ObsFile,
    label_a: &str,
    label_b: &str,
    out: &mut Vec<String>,
) {
    let prefix = format!("{}/{}", key.0, key.1.as_str());
    let mut ia = 0;
    let mut ib = 0;

    while ia < sa.len() && ib < sb.len() && out.len() < MAX_DIFFS {
        let (ea, oa) = sa[ia];
        let (eb, ob) = sb[ib];

        match ea.cmp(&eb) {
            std::cmp::Ordering::Less => {
                out.push(format!(
                    "{prefix} epoch[{ea}] {}: only in {label_a}",
                    fmt_epoch(&file_a.epochs[ea])
                ));
                ia += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(format!(
                    "{prefix} epoch[{eb}] {}: only in {label_b}",
                    fmt_epoch(&file_b.epochs[eb])
                ));
                ib += 1;
            }
            std::cmp::Ordering::Equal => {
                if !obs_eq(oa, ob) {
                    let phys_a = i128::from(oa.value) * i128::from(oa.scale);
                    let phys_b = i128::from(ob.value) * i128::from(ob.scale);
                    out.push(format!(
                        "{prefix} epoch[{ea}] {}: value mismatch: {}(phys={phys_a})  {}(phys={phys_b})",
                        fmt_epoch(&file_a.epochs[ea]),
                        label_a,
                        label_b,
                    ));
                }
                ia += 1;
                ib += 1;
            }
        }
    }

    while ia < sa.len() && out.len() < MAX_DIFFS {
        let (ea, _) = sa[ia];
        out.push(format!(
            "{prefix} epoch[{ea}] {}: only in {label_a}",
            fmt_epoch(&file_a.epochs[ea])
        ));
        ia += 1;
    }
    while ib < sb.len() && out.len() < MAX_DIFFS {
        let (eb, _) = sb[ib];
        out.push(format!(
            "{prefix} epoch[{eb}] {}: only in {label_b}",
            fmt_epoch(&file_b.epochs[eb])
        ));
        ib += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_epoch_output() {
        let e = EpochRecord {
            year: 2020,
            month: 7,
            day: 18,
            hour: 0,
            minute: 0,
            seconds_e7: 0,
            flag: 0,
            clock_offset_e12: 0,
        };
        assert_eq!(fmt_epoch(&e), "2020-07-18 00:00:00.0000000");
    }

    #[test]
    fn norm_indicator_equivalence() {
        assert_eq!(norm_indicator(b'0'), b' ');
        assert_eq!(norm_indicator(b' '), b' ');
        assert_eq!(norm_indicator(b'1'), b'1');
    }

    #[test]
    fn obs_eq_same_physical_different_scale() {
        // value=2, scale=500 ≡ value=1, scale=1000 (both = 1.0 physical).
        let a = Obs {
            value: 2,
            scale: 500,
            lli: b' ',
            ssi: b' ',
        };
        let b = Obs {
            value: 1,
            scale: 1000,
            lli: b' ',
            ssi: b' ',
        };
        assert!(obs_eq(a, b));
    }

    #[test]
    fn obs_eq_lli_zero_vs_space() {
        let a = Obs {
            value: 100,
            scale: 1,
            lli: b'0',
            ssi: b' ',
        };
        let b = Obs {
            value: 100,
            scale: 1,
            lli: b' ',
            ssi: b' ',
        };
        assert!(obs_eq(a, b));
    }
}
