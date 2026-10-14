//! Compact RINEX (Hatanaka) reader tests.
//!
//! 1. Golden outputs — each `tests/data/*.crx` was produced from the `.rnx`
//!    of the same name by `RNX2CRX` (RNXCMP 4.2.0), and `*.crx.rnx` is the
//!    output of `CRX2RNX` 4.2.0 on it.  Our decompressor must reproduce it
//!    byte for byte, and parse to the same model as the original `.rnx`.
//! 2. Corpus pairs — `.crx` / `.??d` files in `rnx-data/` against their
//!    RINEX or SRNX counterparts.  Skipped when fixtures are absent.
//! 3. Oracle — byte-for-byte comparison with a `CRX2RNX` binary, named by
//!    the `CRX2RNX` environment variable (default `crx2rnx` in `PATH`).
//!    Skipped for files the binary cannot decode (e.g. CRINEX 3.1 for
//!    releases before 4.2.0).
//! 4. Cross-check against `rnx2srnx`, which reads Compact RINEX directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use srnx::crinex::{self, CrinexVersion};
use srnx::diff;
use srnx::model::ObsFile;
use srnx::rinex2_obs;
use srnx::rinex3_obs;
use srnx::rinex_header;
use srnx::srnx_reader;

fn test_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

fn rnx_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rnx-data")
        .join(name)
}

/// Parse an uncompressed RINEX file of any supported version.
fn parse_rinex(bytes: &[u8], name: &str) -> ObsFile {
    match rinex_header::sniff_version(bytes) {
        Some(v) if v.is_v3_or_later() => {
            rinex3_obs::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
        }
        _ => rinex2_obs::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}")),
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

    let from_crx = crinex::parse(&crx).unwrap();
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
    let file = crinex::parse(&crx).unwrap();
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

// ── Corpus pairs ─────────────────────────────────────────────────────────────

/// Compare `crx_name` with `other_name` (RINEX or SRNX) from `rnx-data/`.
fn corpus_pair(crx_name: &str, other_name: &str) {
    let (Ok(crx), Ok(other)) = (
        std::fs::read(rnx_data(crx_name)),
        std::fs::read(rnx_data(other_name)),
    ) else {
        eprintln!("skip {crx_name} vs {other_name}: fixture(s) not found");
        return;
    };
    let from_crx = crinex::parse(&crx).unwrap_or_else(|e| panic!("{crx_name}: {e}"));
    let from_other = if other.starts_with(b"SRNX") {
        srnx_reader::read(&other).unwrap_or_else(|e| panic!("{other_name}: {e}"))
    } else {
        parse_rinex(&other, other_name)
    };
    assert_same(&from_crx, &from_other, crx_name, other_name);
}

#[test]
fn corpus_n3021830() {
    corpus_pair("n3021830.22d", "n3021830.22o");
}

#[test]
fn corpus_ctbk1830() {
    corpus_pair("ctbk1830.22d", "ctbk1830.22o");
}

#[test]
fn corpus_azst1830() {
    corpus_pair("azst1830.22d", "azst1830.22o");
}

#[test]
fn corpus_twtf_v302() {
    corpus_pair(
        "TWTF00TWN_R_20201890000_01D_30S_MO.crx",
        "TWTF00TWN_R_20201890000_01D_30S_MO.rnx",
    );
}

#[test]
fn corpus_abmf_v304() {
    corpus_pair("abmf.crx", "abmf.rnx");
}

// chzz1760.19o and p1703260.19o lack the last few epochs of the matching
// .19d files, so those two are only checked by `oracle_large`.

#[test]
#[ignore = "large file: 86400 epochs × 31 sats"]
fn corpus_bet11830() {
    corpus_pair("bet11830.22d", "bet11830.srnx");
}

// ── CRX2RNX oracle ───────────────────────────────────────────────────────────

/// Run the `CRX2RNX` oracle on `path`.  `None` if it is unavailable or
/// rejects the file.
fn run_crx2rnx(path: &Path) -> Option<Vec<u8>> {
    let program = std::env::var_os("CRX2RNX").unwrap_or_else(|| "crx2rnx".into());
    let output = Command::new(program)
        .arg("-")
        .stdin(std::fs::File::open(path).ok()?)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn oracle(paths: &[PathBuf]) {
    for path in paths {
        let Ok(crx) = std::fs::read(path) else {
            eprintln!("skip oracle {}: not found", path.display());
            continue;
        };
        let Some(want) = run_crx2rnx(path) else {
            eprintln!(
                "skip oracle {}: CRX2RNX unavailable or failed",
                path.display()
            );
            continue;
        };
        let got = crinex::decompress(&crx).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            got == want,
            "{}: output differs from CRX2RNX",
            path.display()
        );
    }
}

#[test]
fn oracle_small() {
    let mut paths: Vec<PathBuf> = [
        "crinex_v211_features.crx",
        "crinex_v304_features.crx",
        "rinex211_flag1_flag6.crx",
        "rinex402_picoseconds.crx",
    ]
    .iter()
    .map(|n| test_data(n))
    .collect();
    paths.extend(
        [
            "migd3130.24d",
            "n3021830.22d",
            "ctbk1830.22d",
            "BALM00AUS_S_20262802345_15M_01S_MO.crx",
            "BALM00AUS_S_20262810000_15M_01S_MO.crx",
            "BALM00AUS_S_20262810015_15M_01S_MO.crx",
            "ALBY00AUS_S_20262810000_15M_01S_MO.crx",
            "ALBY00AUS_S_20262810015_15M_01S_MO.crx",
            "ALBY00AUS_S_20262810045_15M_01S_MO.crx",
            "TWTF00TWN_R_20201890000_01D_30S_MO.crx",
        ]
        .iter()
        .map(|n| rnx_data(n)),
    );
    oracle(&paths);
}

#[test]
#[ignore = "large files"]
fn oracle_large() {
    let paths: Vec<PathBuf> = [
        "abmf.crx",
        "azst1830.22d",
        "bet11830.22d",
        "chzz1760.19d",
        "p1703260.19d",
    ]
    .iter()
    .map(|n| rnx_data(n))
    .collect();
    oracle(&paths);
}

// ── rnx2srnx cross-check ─────────────────────────────────────────────────────

/// `rnx2srnx` reads Compact RINEX itself; its SRNX output must match our
/// decoding.  Skipped if the fixture or `rnx2srnx` is missing.
fn via_rnx2srnx(name: &str) {
    let path = rnx_data(name);
    let Ok(crx) = std::fs::read(&path) else {
        eprintln!("skip via_rnx2srnx {name}: not found");
        return;
    };
    let tmp = std::env::temp_dir().join(format!(
        "srnx_crinex_test_{}_{name}.srnx",
        std::process::id()
    ));
    match Command::new("rnx2srnx").arg(&path).arg(&tmp).status() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            let _ = std::fs::remove_file(&tmp);
            panic!("rnx2srnx {name} exited with {s}");
        }
        Err(_) => {
            eprintln!("skip via_rnx2srnx {name}: rnx2srnx not available");
            return;
        }
    }
    let srnx_b = std::fs::read(&tmp).expect("read temp srnx");
    let _ = std::fs::remove_file(&tmp);

    let from_crx = crinex::parse(&crx).unwrap_or_else(|e| panic!("{name}: {e}"));
    let from_srnx = srnx_reader::read(&srnx_b).expect("parse generated SRNX");
    assert_same(&from_crx, &from_srnx, name, "rnx2srnx-output");
}

#[test]
fn via_rnx2srnx_balm() {
    via_rnx2srnx("BALM00AUS_S_20262810000_15M_01S_MO.crx");
}

#[test]
fn via_rnx2srnx_alby() {
    via_rnx2srnx("ALBY00AUS_S_20262810015_15M_01S_MO.crx");
}
