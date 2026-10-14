//! Compact RINEX (Hatanaka) reader tests.
//!
//! Golden outputs: each hand-written fixture's `.crx` was produced from the
//! `.rnx` of the same name by `RNX2CRX` (RNXCMP 4.2.0), and `*.crx.rnx` is
//! the output of `CRX2RNX` 4.2.0 on it.  Our decompressor must reproduce it
//! byte for byte, and parse to the same model as the original `.rnx`.
//!
//! `fixtures.rs` checks the other `.crx` fixtures against their RINEX and
//! SRNX forms, and `corpus.rs` compares our decompressor with `CRX2RNX` on
//! a corpus of your own.

use std::path::PathBuf;

use srnx::crinex::{self, CrinexVersion};
use srnx::diff;
use srnx::model::ObsFile;
use srnx::rinex2_obs;
use srnx::rinex3_obs;
use srnx::rinex_header;

fn test_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

/// Parse an uncompressed RINEX file of any supported version.
fn parse_rinex(bytes: &[u8], name: &str) -> ObsFile {
    match rinex_header::sniff_version(bytes) {
        Some(v) if v.is_v3_or_later() => {
            rinex3_obs::parse(bytes, None).unwrap_or_else(|e| panic!("{name}: {e}"))
        }
        _ => rinex2_obs::parse(bytes, None).unwrap_or_else(|e| panic!("{name}: {e}")),
    }
}

fn assert_same(a: &ObsFile, b: &ObsFile, la: &str, lb: &str) {
    assert!(!a.epochs.is_empty(), "{la}: no epochs");
    let diffs = diff::compare(a, b, la, lb);
    assert!(
        diffs.is_empty(),
        "{la} vs {lb}: {} diff(s):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

/// Decode `<stem>.crx` and compare with the golden `CRX2RNX` output and,
/// semantically, with the original `<stem>.rnx`.
fn golden(stem: &str, version: CrinexVersion) {
    let crx = std::fs::read(test_data(&format!("{stem}.crx"))).unwrap();
    let want = std::fs::read(test_data(&format!("{stem}.crx.rnx"))).unwrap();
    let rnx = std::fs::read(test_data(&format!("{stem}.rnx"))).unwrap();

    assert_eq!(crinex::sniff_version(&crx), Some(version));
    let got = crinex::decompress(&crx).unwrap_or_else(|e| panic!("{stem}.crx: {e}"));
    if got != want {
        let first_bad = got
            .split(|&b| b == b'\n')
            .zip(want.split(|&b| b == b'\n'))
            .position(|(g, w)| g != w);
        panic!(
            "{stem}.crx: output differs from CRX2RNX (first differing line: {:?})",
            first_bad.map(|i| i + 1)
        );
    }

    let from_crx = crinex::parse(&crx, None).unwrap();
    let from_rnx = parse_rinex(&rnx, stem);
    assert_same(
        &from_crx,
        &from_rnx,
        &format!("{stem}.crx"),
        &format!("{stem}.rnx"),
    );
}

#[test]
fn golden_v211_features() {
    golden("crinex_v211_features", CrinexVersion::V1_0);
}

#[test]
fn golden_v211_flag1_flag6() {
    golden("rinex211_flag1_flag6", CrinexVersion::V1_0);
}

#[test]
fn golden_v304_features() {
    golden("crinex_v304_features", CrinexVersion::V3_0);
}

#[test]
fn golden_v402_picoseconds() {
    golden("rinex402_picoseconds", CrinexVersion::V3_1);
}

/// The CRINEX 3.1 picosecond digits must reach the epoch records.
#[test]
fn picoseconds_are_decoded() {
    let crx = std::fs::read(test_data("rinex402_picoseconds.crx")).unwrap();
    let file = crinex::parse(&crx, None).unwrap();
    let ps: Vec<u32> = file.epochs.iter().map(|e| e.extra_ps).collect();
    assert_eq!(ps, [1, 23456, 99999, 99999, 0, 0, 7]);
}

/// Each satellite's arcs must restart where the file says so; truncating
/// a data line makes the following epoch undecodable.
#[test]
fn corrupt_input_is_rejected() {
    let crx = std::fs::read(test_data("crinex_v304_features.crx")).unwrap();
    let text = String::from_utf8(crx).unwrap();
    // Drop the "3&" from the first clock line after the header.
    let broken = text.replacen("\n3&123456\n", "\n123456\n", 1);
    assert!(matches!(
        crinex::decompress(broken.as_bytes()),
        Err(crinex::CrinexError::NotInitialized(_))
    ));
    // Cut the file in the middle of an epoch.
    let cut = &text[..text.find("1000 5000 0\n").unwrap()];
    assert!(matches!(
        crinex::decompress(cut.as_bytes()),
        Err(crinex::CrinexError::Truncated(_))
    ));
}
