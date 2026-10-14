//! Black-box reference-encoder interoperability tests. No reference source is used.
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use srnx::model::ObsFile;
use srnx::{diff, file::SrnxFile, rinex2_obs, rinex3_obs, srnx_reader};

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
    let actual = srnx_reader::read(&bytes, None).unwrap();
    let original = rinex2_obs::parse(&std::fs::read(&input).unwrap(), None).unwrap();
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
    let oracle = rinex2_obs::parse(&std::fs::read(&decoded_output).unwrap(), None).unwrap();
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
        observations.push(srnx_reader::read(&std::fs::read(&output).unwrap(), None).unwrap());
        std::fs::remove_file(output).unwrap();
    }
    let differences = diff::compare(&observations[0], &observations[1], "SOC2", "SOCD");
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// Encode a RINEX 4.02 file with picosecond epochs, check that the reference
/// encoder chose `EPO2`, and compare our reader with both the input and the
/// reference decoder's output. Returns the parsed SRNX file, or `None` if
/// the reference tools are unavailable.
fn epo2_round_trip(input: &Path, stem: &str) -> Option<ObsFile> {
    let stem = format!("srnx-epo2-{}-{stem}", std::process::id());
    let output = std::env::temp_dir().join(format!("{stem}.srnx"));
    let decoded_output = std::env::temp_dir().join(format!("{stem}.rnx"));
    let result = match Command::new(helper("rnx2srnx"))
        .arg(input)
        .arg(&output)
        .output()
    {
        Ok(result) => result,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skip EPO2 reference test: rnx2srnx unavailable");
            return None;
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
    assert_eq!(framing.header.minor, 2);
    let counts = framing.count_by_fourcc();
    assert_eq!(counts.get(b"EPO2"), Some(&1));
    assert_eq!(counts.get(b"EPOC"), None);

    let actual = srnx_reader::read(&bytes, None).unwrap();
    let original = rinex3_obs::parse(&std::fs::read(input).unwrap(), None).unwrap();
    let differences = diff::compare(&original, &actual, "RINEX 4.02", "reference SRNX");
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
    let oracle = rinex3_obs::parse(&std::fs::read(&decoded_output).unwrap(), None).unwrap();
    let differences = diff::compare(&oracle, &actual, "reference RINEX", "SRNX reader");
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(decoded_output).unwrap();
    Some(actual)
}

#[test]
fn rinex402_picoseconds_use_epo2() {
    let input =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/rinex402_picoseconds.rnx");
    let Some(actual) = epo2_round_trip(&input, "fixture") else {
        return;
    };
    assert_eq!(
        (actual.epochs[0].seconds_e7, actual.epochs[0].extra_ps),
        (1, 1)
    );
}

/// The picosecond fixture is too small for cross-signal prediction, so derive
/// a RINEX 4.02 file from the first epochs of `abmf.rnx`, adding picosecond
/// digits that make every interval irregular. This exercises SOC2 rate
/// predictors with `EPO2` time units.
#[test]
fn epo2_rate_prediction_uses_picoseconds() {
    let source = fixture("abmf.rnx");
    let Ok(text) = std::fs::read_to_string(&source) else {
        eprintln!("skip EPO2 rate test: {} unavailable", source.display());
        return;
    };
    let mut out = String::new();
    let mut in_header = true;
    let mut epochs = 0u32;
    for line in text.lines() {
        if in_header {
            if line.ends_with("RINEX VERSION / TYPE") {
                out.push_str("     4.02");
                out.push_str(&line[9..]);
            } else {
                out.push_str(line);
            }
            in_header = !line.ends_with("END OF HEADER");
        } else if line.starts_with('>') {
            if epochs == 200 {
                break;
            }
            epochs += 1;
            let ps = epochs * 37_123 % 100_000;
            let _ = write!(out, "{:<56} {ps:05}", &line[..35]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    let input = std::env::temp_dir().join(format!("srnx-epo2-{}-abmf402.rnx", std::process::id()));
    std::fs::write(&input, out).unwrap();
    let actual = epo2_round_trip(&input, "abmf");
    std::fs::remove_file(&input).unwrap();
    if let Some(actual) = actual {
        assert_eq!(actual.epochs.len(), 200);
        assert_eq!(actual.epochs[1].extra_ps, 74_246);
    }
}
