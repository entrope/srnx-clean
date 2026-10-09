//! Black-box reference-encoder interoperability tests. No reference source is used.
use std::path::PathBuf;
use std::process::Command;

use srnx::{diff, file::SrnxFile, rinex2_obs, srnx_reader};

fn fixture(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let local = root.join("rnx-data").join(name);
    if local.exists() {
        local
    } else {
        root.join("../rnx-data").join(name)
    }
}

fn helper(name: &str) -> PathBuf {
    let home_bin = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("bin").join(name));
    home_bin
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn reference_round_trip(name: &str, socd: bool) {
    let input = fixture(name);
    if !input.exists() {
        eprintln!("skip reference test: {} unavailable", input.display());
        return;
    }
    let stem = format!(
        "srnx-soc2-{}-{name}-{}",
        std::process::id(),
        if socd { "socd" } else { "soc2" }
    );
    let output = std::env::temp_dir().join(format!("{stem}.srnx"));
    let decoded_output = std::env::temp_dir().join(format!("{stem}.rnx"));
    let mut encoder = Command::new(helper("rnx2srnx"));
    if socd {
        encoder.arg("--socd");
    }
    let result = encoder.arg(&input).arg(&output).output();
    let result = match result {
        Ok(result) => result,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skip reference test: rnx2srnx unavailable");
            return;
        }
        Err(e) => panic!("reference encoder: {e}"),
    };
    assert!(
        result.status.success(),
        "encoder failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = std::fs::read(&output).unwrap();
    let framing = SrnxFile::parse(&bytes).unwrap();
    assert_eq!(framing.header.minor, u64::from(!socd));
    assert!(framing
        .chunks
        .iter()
        .any(|c| c.fourcc == if socd { *b"SOCD" } else { *b"SOC2" }));
    assert!(framing.chunks.iter().all(|c| c.digest_ok));
    let actual = srnx_reader::read(&bytes).unwrap();
    let original = rinex2_obs::parse(&std::fs::read(&input).unwrap()).unwrap();
    let differences = diff::compare(&original, &actual, name, "reference SRNX");
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    let decoder = Command::new(helper("srnx2rnx"))
        .arg(&output)
        .arg(&decoded_output)
        .output()
        .unwrap();
    assert!(
        decoder.status.success(),
        "decoder failed: {}",
        String::from_utf8_lossy(&decoder.stderr)
    );
    let oracle = rinex2_obs::parse(&std::fs::read(&decoded_output).unwrap()).unwrap();
    let differences = diff::compare(&oracle, &actual, "reference RINEX", "SRNX reader");
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(decoded_output).unwrap();
}

#[test]
fn soc2_value_prediction_and_ssi() {
    reference_round_trip("n3021830.22o", false);
}
#[test]
fn soc2_rate_prediction_and_events() {
    reference_round_trip("mty22000.20o", false);
}
#[test]
fn revision_1_0_remains_compatible() {
    reference_round_trip("n3021830.22o", true);
}

#[test]
fn rinex3_soc2_matches_revision_1_0() {
    let input = fixture("abmf.rnx");
    if !input.exists() {
        eprintln!("skip RINEX 3 reference test: abmf.rnx unavailable");
        return;
    }
    let stem = format!("srnx-soc2-{}-abmf", std::process::id());
    let mut observations = Vec::new();
    for socd in [false, true] {
        let output = std::env::temp_dir().join(format!("{stem}-{socd}.srnx"));
        let mut encoder = Command::new(helper("rnx2srnx"));
        if socd {
            encoder.arg("--socd");
        }
        let result = match encoder.arg(&input).arg(&output).output() {
            Ok(result) => result,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skip RINEX 3 reference test: rnx2srnx unavailable");
                return;
            }
            Err(e) => panic!("reference encoder: {e}"),
        };
        assert!(
            result.status.success(),
            "encoder failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        observations.push(srnx_reader::read(&std::fs::read(&output).unwrap()).unwrap());
        std::fs::remove_file(output).unwrap();
    }
    let differences = diff::compare(&observations[0], &observations[1], "SOC2", "SOCD");
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
