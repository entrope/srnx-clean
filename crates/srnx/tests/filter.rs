//! Satellite/signal filtering and catalogs, checked across formats.
//!
//! A filtered read must equal the unfiltered read restricted to the
//! selected pairs.  The SRNX fixtures use SOC2 value and rate predictors and
//! SSI references, so filters that drop the reference columns exercise the
//! lazy decoding of unselected references.  Tests are skipped when a
//! fixture is unavailable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use srnx::model::{Catalog, HeaderText, ObsCode, ObsFile, SatId, SatSystem, Version};
use srnx::reader::{self, Format};

fn try_load(name: &str) -> Option<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rnx-data")
        .join(name);
    std::fs::read(path).ok()
}

/// Read `bytes` with and without `keep` and compare.
fn check_filtered(name: &str, bytes: &[u8], keep: fn(SatId, ObsCode) -> bool) {
    let full = reader::read(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut calls = BTreeMap::<(SatId, ObsCode), usize>::new();
    let part = reader::read_filtered(bytes, |sat, code| {
        *calls.entry((sat, code)).or_default() += 1;
        keep(sat, code)
    })
    .unwrap_or_else(|e| panic!("{name}: {e}"));

    assert!(
        calls.values().all(|&n| n == 1),
        "{name}: filter called more than once for a pair"
    );
    assert!(
        full.observations.keys().all(|key| calls.contains_key(key)),
        "{name}: filter not consulted for an observed pair"
    );
    let expected: BTreeMap<_, _> = full
        .observations
        .iter()
        .filter(|&(&(sat, code), _)| keep(sat, code))
        .map(|(key, series)| (*key, series.clone()))
        .collect();
    assert!(!expected.is_empty(), "{name}: filter selects nothing");
    assert!(
        expected.len() < full.observations.len(),
        "{name}: filter selects everything"
    );
    assert_eq!(part.observations, expected, "{name}: observations");
    assert_eq!(part.epochs, full.epochs, "{name}: epochs");
    assert_eq!(part.header, full.header, "{name}: header");
    let events = |f: &ObsFile| -> Vec<(usize, Vec<String>)> {
        f.events
            .iter()
            .map(|(i, e)| (*i, e.lines.clone()))
            .collect()
    };
    assert_eq!(events(&part), events(&full), "{name}: events");
}

/// Pseudoranges and Doppler of odd PRNs, without the carrier phases they
/// are predicted from.
fn no_phase_odd_prn(sat: SatId, code: ObsCode) -> bool {
    sat.prn % 2 == 1 && !code.as_str().starts_with('L')
}

/// GPS carrier phase only, without the signal strengths that its SSIs may
/// be predicted from.
fn gps_phase(sat: SatId, code: ObsCode) -> bool {
    sat.system == SatSystem::GPS && code.as_str().starts_with('L')
}

fn check_all_filters(name: &str) {
    let Some(bytes) = try_load(name) else {
        return;
    };
    check_filtered(name, &bytes, no_phase_odd_prn);
    check_filtered(name, &bytes, gps_phase);
}

#[test]
fn filtered_srnx_abmf() {
    check_all_filters("abmf.srnx");
}

#[test]
fn filtered_srnx_ctbk1830() {
    check_all_filters("ctbk1830.srnx");
}

#[test]
fn filtered_rinex2_ctbk1830() {
    check_all_filters("ctbk1830.22o");
}

#[test]
fn filtered_crinex1_ctbk1830() {
    check_all_filters("ctbk1830.22d");
}

#[test]
fn filtered_rinex3_and_crinex3_abmf() {
    check_all_filters("abmf.rnx");
    check_all_filters("abmf.crx");
}

fn observed_pairs(file: &ObsFile) -> BTreeSet<(SatId, ObsCode)> {
    file.observations.keys().copied().collect()
}

fn catalog_pairs(catalog: &Catalog) -> BTreeSet<(SatId, ObsCode)> {
    catalog
        .satellites
        .as_ref()
        .expect("catalog lists satellites")
        .iter()
        .flat_map(|(sat, codes)| codes.iter().map(move |&code| (*sat, code)))
        .collect()
}

#[test]
fn srnx_catalog_matches_observed_columns() {
    for name in ["abmf.srnx", "ctbk1830.srnx"] {
        let Some(bytes) = try_load(name) else {
            continue;
        };
        let catalog = reader::catalog(&bytes).unwrap();
        assert!(!catalog.codes_by_system.is_empty(), "{name}");
        let file = reader::read(&bytes).unwrap();
        assert_eq!(catalog_pairs(&catalog), observed_pairs(&file), "{name}");
    }
}

#[test]
fn rinex_catalogs_agree_across_formats() {
    // ctbk1830.22o has PRN / # OF OBS records; every satellite may carry
    // every v2 observation code.
    if let (Some(rnx), Some(crx), Some(srnx)) = (
        try_load("ctbk1830.22o"),
        try_load("ctbk1830.22d"),
        try_load("ctbk1830.srnx"),
    ) {
        assert_eq!(Format::detect(&rnx), Some(Format::Rinex2));
        assert_eq!(Format::detect(&crx), Some(Format::Crinex));
        let catalog = reader::catalog(&rnx).unwrap();
        assert_eq!(reader::catalog(&crx).unwrap(), catalog);
        let [(system, codes)] = catalog.codes_by_system.as_slice() else {
            panic!("v2 has one code list");
        };
        assert_eq!(*system, SatSystem::MIXED);
        let sats = catalog.satellites.as_ref().unwrap();
        assert!(sats.iter().all(|(_, c)| c == codes));
        let pairs = catalog_pairs(&catalog);
        let file = reader::read(&rnx).unwrap();
        assert!(observed_pairs(&file).is_subset(&pairs));
        let srnx_catalog = reader::catalog(&srnx).unwrap();
        assert!(catalog_pairs(&srnx_catalog).is_subset(&pairs));
    }

    // abmf.rnx has no PRN / # OF OBS records.
    if let (Some(rnx), Some(crx), Some(srnx)) = (
        try_load("abmf.rnx"),
        try_load("abmf.crx"),
        try_load("abmf.srnx"),
    ) {
        assert_eq!(Format::detect(&rnx), Some(Format::Rinex3));
        let catalog = reader::catalog(&rnx).unwrap();
        assert_eq!(catalog.satellites, None);
        assert_eq!(reader::catalog(&crx).unwrap(), catalog);
        assert_eq!(
            reader::catalog(&srnx).unwrap().codes_by_system,
            catalog.codes_by_system
        );
    }
}

#[test]
fn version_is_the_rinex_version_in_every_format() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let fixtures = [
        ("crinex_v211_features", 2, 11),
        ("crinex_v304_features", 3, 4),
        ("rinex402_picoseconds", 4, 2),
    ];
    for (stem, major, minor) in fixtures {
        for ext in ["rnx", "crx"] {
            let name = format!("{stem}.{ext}");
            let bytes = std::fs::read(data.join(&name)).unwrap();
            let file = reader::read(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(file.version(), Some(Version { major, minor }), "{name}");
        }
    }

    for (name, major, minor) in [("ctbk1830.srnx", 2, 11), ("abmf.srnx", 3, 4)] {
        if let Some(bytes) = try_load(name) {
            let file = reader::read(&bytes).unwrap();
            assert_eq!(file.version(), Some(Version { major, minor }), "{name}");
        }
    }

    let unversioned = ObsFile {
        header: HeaderText::from_text("END OF HEADER\n"),
        epochs: Vec::new(),
        events: Vec::new(),
        observations: BTreeMap::new(),
    };
    assert_eq!(unversioned.version(), None);
}
