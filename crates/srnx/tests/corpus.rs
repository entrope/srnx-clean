//! Phase 9 corpus-wide integration tests.
//!
//! Three test categories:
//!   1. Golden stats — parse each `.srnx` fixture, compute chunk-count /
//!      scale / delta-order / block-header histograms, compare against the
//!      `srnx_scan` reference tool output.  Tests are skipped gracefully when
//!      either the fixture or `srnx_scan` is unavailable.
//!   2. Round-trip semantics — for each RINEX v2 / SRNX file pair, parse
//!      both with our readers and assert `srnx-diff`-style equality.
//!   3. Digest-corruption negatives — flip bits in file regions covered by
//!      CRC32C and verify the mismatch is reported.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use srnx::file::SrnxFile;
use srnx::model::{EpochRecord, Obs, ObsFile};
use srnx::rinex2_obs;
use srnx::socd;
use srnx::srnx_reader;

// ── Fixture path helper ───────────────────────────────────────────────────────

fn rnx_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rnx-data")
        .join(name)
}

fn try_load(name: &str) -> Option<Vec<u8>> {
    std::fs::read(rnx_data(name)).ok()
}

// ── Aggregate SRNX statistics ─────────────────────────────────────────────────

struct SrnxStats {
    chunk_counts: BTreeMap<[u8; 4], usize>,
    total_epochs: usize,
    scale_histogram: BTreeMap<u32, usize>,
    delta_order_histogram: BTreeMap<u8, usize>,
    block_header_histogram: BTreeMap<u8, usize>,
}

fn compute_stats(bytes: &[u8]) -> SrnxStats {
    let file = SrnxFile::parse(bytes).expect("SrnxFile::parse");
    let chunk_counts = file.count_by_fourcc();
    let total_epochs = file.epoc().expect("epoc()").map_or(0, |e| e.epochs.len());

    let mut scale_histogram = BTreeMap::<u32, usize>::new();
    let mut delta_order_histogram = BTreeMap::<u8, usize>::new();
    let mut block_header_histogram = BTreeMap::<u8, usize>::new();

    for chunk in &file.chunks {
        if chunk.fourcc == *b"SOCD" {
            let s = socd::parse(chunk.payload)
                .unwrap_or_else(|e| panic!("SOCD at {} failed: {e}", chunk.file_offset));
            *scale_histogram.entry(s.scale).or_default() += 1;
            *delta_order_histogram.entry(s.delta_order).or_default() += 1;
            for (&hdr, &cnt) in &s.block_header_counts {
                *block_header_histogram.entry(hdr).or_default() += cnt;
            }
        }
    }

    SrnxStats {
        chunk_counts,
        total_epochs,
        scale_histogram,
        delta_order_histogram,
        block_header_histogram,
    }
}

// ── srnx_scan output parser ───────────────────────────────────────────────────

struct ScanSummary {
    chunk_counts: BTreeMap<[u8; 4], usize>,
    total_epochs: usize,
    /// Scale key uses our internal `Socd.scale` convention:
    /// - `srnx_scan` `"1"`    → 1   (default / implicit-1000×)
    /// - `srnx_scan` `"0.25"` → 250 (explicit 250)
    /// - `srnx_scan` `"0.02"` → 20  (explicit 20)
    scale_histogram: BTreeMap<u32, usize>,
    delta_order_histogram: BTreeMap<u8, usize>,
    block_header_histogram: BTreeMap<u8, usize>,
}

/// Convert a `srnx_scan` scale-factor display string to our internal u32 scale.
fn scan_scale_to_ours(s: &str) -> u32 {
    let f: f64 = s.trim().parse().unwrap_or(1.0);
    if (f - 1.0).abs() < 1e-9 {
        1 // default (no explicit scale) — our code stores 1
    } else {
        // Scale values are always small positive integers (1–1000); cast is safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (f * 1000.0).round() as u32
        }
    }
}

#[derive(PartialEq)]
enum Sec {
    None,
    Chunks,
    Scale,
    Delta,
    Blocks,
}

fn run_srnx_scan(path: &PathBuf) -> Option<ScanSummary> {
    let output = std::process::Command::new("srnx_scan")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_scan_output(std::str::from_utf8(&output.stdout).ok()?)
}

#[allow(clippy::too_many_lines)]
fn parse_scan_output(text: &str) -> Option<ScanSummary> {
    // Only look at the aggregate summary section (handles both 1-file and
    // multi-file invocations uniformly).
    let agg = text.find("=== Aggregate Summary")?;
    let text = &text[agg..];

    let mut chunk_counts = BTreeMap::new();
    let mut total_epochs = 0usize;
    let mut scale_histogram = BTreeMap::new();
    let mut delta_order_histogram = BTreeMap::new();
    let mut block_header_histogram = BTreeMap::new();

    let mut sec = Sec::None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("Chunk totals:") {
            sec = Sec::Chunks;
            continue;
        }
        if line.starts_with("Scale factors") {
            sec = Sec::Scale;
            continue;
        }
        if line.starts_with("Delta orders:") {
            sec = Sec::Delta;
            continue;
        }
        if line.starts_with("Block headers:") {
            sec = Sec::Blocks;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Total epochs:") {
            total_epochs = rest.trim().parse().unwrap_or(0);
            continue;
        }
        if line.starts_with("===") {
            sec = Sec::None;
            continue;
        }

        match sec {
            Sec::Chunks => {
                // "EPOC: 1"
                if let Some((name, n)) = line.split_once(": ") {
                    let name = name.trim();
                    if name.len() == 4 {
                        if let Ok(tag) = <[u8; 4]>::try_from(name.as_bytes()) {
                            let count: usize = n.trim().parse().unwrap_or(0);
                            chunk_counts.insert(tag, count);
                        }
                    }
                }
            }
            Sec::Scale => {
                // "1: 487 SOCDs"  or  "0.25: 218 SOCDs"
                if let Some((scale_s, rest)) = line.split_once(": ") {
                    let count: usize = rest
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let key = scan_scale_to_ours(scale_s.trim());
                    *scale_histogram.entry(key).or_default() += count;
                }
            }
            Sec::Delta => {
                // "order 1: 218 SOCDs"
                if let Some(rest) = line.strip_prefix("order ") {
                    if let Some((order_s, count_rest)) = rest.split_once(": ") {
                        let order: u8 = order_s.trim().parse().unwrap_or(255);
                        let count: usize = count_rest
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        *delta_order_histogram.entry(order).or_default() += count;
                    }
                }
            }
            Sec::Blocks => {
                // "0x00 (8×1-bit matrix): 11790"
                // "0xFD (absent run): 10713"
                if let Some(rest) = line.strip_prefix("0x") {
                    if rest.len() >= 2 {
                        if let Ok(header) = u8::from_str_radix(&rest[..2], 16) {
                            if let Some(pos) = line.rfind(": ") {
                                let count: usize = line[pos + 2..].trim().parse().unwrap_or(0);
                                *block_header_histogram.entry(header).or_default() += count;
                            }
                        }
                    }
                }
            }
            Sec::None => {}
        }
    }

    Some(ScanSummary {
        chunk_counts,
        total_epochs,
        scale_histogram,
        delta_order_histogram,
        block_header_histogram,
    })
}

// ── Golden test harness ───────────────────────────────────────────────────────

fn golden(name: &str) {
    let Some(bytes) = try_load(name) else {
        eprintln!("skip golden {name}: fixture not found");
        return;
    };
    let Some(expected) = run_srnx_scan(&rnx_data(name)) else {
        eprintln!("skip golden {name}: srnx_scan unavailable or failed");
        return;
    };
    let actual = compute_stats(&bytes);

    // Chunk counts
    for (tag, &exp) in &expected.chunk_counts {
        let act = actual.chunk_counts.get(tag).copied().unwrap_or(0);
        assert_eq!(
            act,
            exp,
            "{name}: chunk count for {}",
            std::str::from_utf8(tag).unwrap_or("????")
        );
    }

    // Total epoch count
    assert_eq!(
        actual.total_epochs, expected.total_epochs,
        "{name}: epoch count"
    );

    // Scale-factor distribution
    assert_eq!(
        actual.scale_histogram, expected.scale_histogram,
        "{name}: scale-factor histogram"
    );

    // Delta-order distribution
    assert_eq!(
        actual.delta_order_histogram, expected.delta_order_histogram,
        "{name}: delta-order histogram"
    );

    // Block-header distribution
    assert_eq!(
        actual.block_header_histogram, expected.block_header_histogram,
        "{name}: block-header histogram"
    );
}

#[test]
fn golden_n3021830() {
    golden("n3021830.srnx");
}

#[test]
fn golden_ctbk1830() {
    golden("ctbk1830.srnx");
}

#[test]
fn golden_azst1830() {
    golden("azst1830.srnx");
}

#[test]
fn golden_abmf() {
    golden("abmf.srnx");
}

#[test]
fn golden_mty22000() {
    golden("mty22000.srnx");
}

// Large files: skipped in fast CI, run with `cargo test -- --include-ignored`.

#[test]
#[ignore = "large file: 86400 epochs × 73 sats"]
fn golden_at012000() {
    golden("at012000.srnx");
}

#[test]
#[ignore = "large file: 86400 epochs × 31 sats"]
fn golden_bet11830() {
    golden("bet11830.srnx");
}

#[test]
#[ignore = "very large: 86400 epochs × 81 sats"]
fn golden_okcb1200() {
    golden("okcb1200.srnx");
}

#[test]
#[ignore = "very large: 86400 epochs × 81 sats"]
fn golden_sbst1200() {
    golden("sbst1200.srnx");
}

// ── Round-trip semantic comparison ───────────────────────────────────────────

const MAX_DIFFS: usize = 30;

fn fmt_epoch(e: &EpochRecord) -> String {
    let whole = e.seconds_e7 / 10_000_000;
    let frac = e.seconds_e7 % 10_000_000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}",
        e.year, e.month, e.day, e.hour, e.minute, whole, frac
    )
}

fn epochs_eq(a: &EpochRecord, b: &EpochRecord) -> bool {
    a.year == b.year
        && a.month == b.month
        && a.day == b.day
        && a.hour == b.hour
        && a.minute == b.minute
        && a.seconds_e7 == b.seconds_e7
}

fn obs_eq(a: Obs, b: Obs) -> bool {
    let pa = i128::from(a.value) * i128::from(a.scale);
    let pb = i128::from(b.value) * i128::from(b.scale);
    pa == pb && norm_ind(a.lli) == norm_ind(b.lli) && norm_ind(a.ssi) == norm_ind(b.ssi)
}

fn norm_ind(b: u8) -> u8 {
    if b == b'0' {
        b' '
    } else {
        b
    }
}

/// Returns a list of human-readable difference strings (empty = files match).
fn compare_obs_files(a: &ObsFile, b: &ObsFile, la: &str, lb: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Epoch counts and timestamps.
    if a.epochs.len() != b.epochs.len() {
        out.push(format!(
            "epoch count: {la}={} {lb}={}",
            a.epochs.len(),
            b.epochs.len()
        ));
    }
    let common = a.epochs.len().min(b.epochs.len());
    for i in 0..common {
        if out.len() >= MAX_DIFFS {
            break;
        }
        let (ea, eb) = (&a.epochs[i], &b.epochs[i]);
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

    // Observation series.
    let keys_a: BTreeSet<_> = a.observations.keys().collect();
    let keys_b: BTreeSet<_> = b.observations.keys().collect();
    for key in keys_a.union(&keys_b) {
        if out.len() >= MAX_DIFFS {
            break;
        }
        match (a.observations.get(key), b.observations.get(key)) {
            (None, _) => out.push(format!("{}/{}: only in {lb}", key.0, key.1.as_str())),
            (_, None) => out.push(format!("{}/{}: only in {la}", key.0, key.1.as_str())),
            (Some(sa), Some(sb)) => {
                let pfx = format!("{}/{}", key.0, key.1.as_str());
                let mut ia = 0;
                let mut ib = 0;
                while ia < sa.len() && ib < sb.len() && out.len() < MAX_DIFFS {
                    let (ea, oa) = sa[ia];
                    let (eb, ob) = sb[ib];
                    match ea.cmp(&eb) {
                        std::cmp::Ordering::Less => {
                            out.push(format!(
                                "{pfx} epoch[{ea}] {}: only in {la}",
                                fmt_epoch(&a.epochs[ea])
                            ));
                            ia += 1;
                        }
                        std::cmp::Ordering::Greater => {
                            out.push(format!(
                                "{pfx} epoch[{eb}] {}: only in {lb}",
                                fmt_epoch(&b.epochs[eb])
                            ));
                            ib += 1;
                        }
                        std::cmp::Ordering::Equal => {
                            if !obs_eq(oa, ob) {
                                let pa = i128::from(oa.value) * i128::from(oa.scale);
                                let pb = i128::from(ob.value) * i128::from(ob.scale);
                                out.push(format!(
                                    "{pfx} epoch[{ea}] {}: {la}(phys={pa}) vs {lb}(phys={pb})",
                                    fmt_epoch(&a.epochs[ea])
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
                        "{pfx} epoch[{ea}] {}: only in {la}",
                        fmt_epoch(&a.epochs[ea])
                    ));
                    ia += 1;
                }
                while ib < sb.len() && out.len() < MAX_DIFFS {
                    let (eb, _) = sb[ib];
                    out.push(format!(
                        "{pfx} epoch[{eb}] {}: only in {lb}",
                        fmt_epoch(&b.epochs[eb])
                    ));
                    ib += 1;
                }
            }
        }
    }
    out
}

// ── Static round-trip tests (pre-existing RINEX v2 / SRNX file pairs) ────────

fn round_trip(rnx_name: &str, srnx_name: &str) {
    let (Some(rnx_b), Some(srnx_b)) = (try_load(rnx_name), try_load(srnx_name)) else {
        eprintln!("skip round_trip {rnx_name}: fixture(s) not found");
        return;
    };
    let rnx = rinex2_obs::parse(&rnx_b).unwrap_or_else(|e| panic!("{rnx_name}: {e}"));
    let srnx = srnx_reader::read(&srnx_b).unwrap_or_else(|e| panic!("{srnx_name}: {e}"));
    let diffs = compare_obs_files(&rnx, &srnx, rnx_name, srnx_name);
    assert!(
        diffs.is_empty(),
        "{rnx_name} vs {srnx_name}: {} diff(s):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

#[test]
fn round_trip_at012000() {
    round_trip("at012000.20o", "at012000.srnx");
}

#[test]
fn round_trip_mty22000() {
    round_trip("mty22000.20o", "mty22000.srnx");
}

#[test]
fn round_trip_n3021830() {
    round_trip("n3021830.22o", "n3021830.srnx");
}

#[test]
fn round_trip_ctbk1830() {
    round_trip("ctbk1830.22o", "ctbk1830.srnx");
}

#[test]
fn round_trip_azst1830() {
    round_trip("azst1830.22o", "azst1830.srnx");
}

#[test]
#[ignore = "very large: 86400 epochs × 81 sats"]
fn round_trip_okcb1200() {
    round_trip("okcb1200.26o", "okcb1200.srnx");
}

#[test]
#[ignore = "very large: 86400 epochs × 81 sats"]
fn round_trip_sbst1200() {
    round_trip("sbst1200.26o", "sbst1200.srnx");
}

// ── Dynamic round-trip test (invokes rnx2srnx) ────────────────────────────────

/// Run `rnx2srnx` on n3021830.22o, then compare the freshly-generated SRNX
/// against our RINEX v2 reader.  Skipped if `rnx2srnx` is not in `PATH`.
#[test]
fn dynamic_round_trip_n3021830() {
    let rnx_path = rnx_data("n3021830.22o");
    if !rnx_path.exists() {
        eprintln!("skip dynamic_round_trip: n3021830.22o not found");
        return;
    }

    let tmp = std::env::temp_dir().join("srnx_corpus_test_n3021830.srnx");

    match std::process::Command::new("rnx2srnx")
        .arg(&rnx_path)
        .arg(&tmp)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            let _ = std::fs::remove_file(&tmp);
            panic!("rnx2srnx exited with {s}");
        }
        Err(_) => {
            eprintln!("skip dynamic_round_trip: rnx2srnx not available");
            return;
        }
    }

    let rnx_b = std::fs::read(&rnx_path).expect("read rnx");
    let srnx_b = std::fs::read(&tmp).expect("read temp srnx");
    let _ = std::fs::remove_file(&tmp);

    let rnx = rinex2_obs::parse(&rnx_b).expect("parse RINEX v2");
    let srnx = srnx_reader::read(&srnx_b).expect("parse generated SRNX");

    let diffs = compare_obs_files(&rnx, &srnx, "n3021830.22o", "rnx2srnx-output");
    assert!(
        diffs.is_empty(),
        "dynamic round-trip n3021830: {} diff(s):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

// ── Negative tests: digest corruption ────────────────────────────────────────

/// Return the number of LEB128 bytes needed to encode `n`.
fn leb128_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 128 {
        n >>= 7;
        len += 1;
    }
    len
}

/// Corrupt a byte deep inside a body chunk's payload; its CRC32C must fail.
#[test]
fn corrupt_body_chunk_detected() {
    let Some(bytes) = try_load("at012000.srnx") else {
        eprintln!("skip corrupt_body_chunk: fixture not found");
        return;
    };
    let file = SrnxFile::parse(&bytes).expect("initial parse");

    let rhdr = file
        .chunks
        .iter()
        .find(|c| c.fourcc == *b"RHDR")
        .expect("RHDR present");

    // Payload starts at: file_offset + 4 (FOURCC) + leb128_len(payload.len()).
    #[allow(clippy::cast_possible_truncation)]
    let payload_start = rhdr.file_offset as usize + 4 + leb128_len(rhdr.payload.len());
    let corrupt_at = payload_start + 128; // well inside the 2736-byte RHDR payload

    let mut tampered = bytes.clone();
    tampered[corrupt_at] ^= 0xFF;

    let file2 = SrnxFile::parse(&tampered).expect("parse still succeeds despite corruption");
    assert!(
        file2
            .chunks
            .iter()
            .find(|c| c.fourcc == *b"RHDR")
            .is_some_and(|c| !c.digest_ok),
        "RHDR per-chunk digest should fail after payload corruption"
    );
}

/// Flip a bit in the SRNX header chunk's own per-chunk CRC32C;
/// `srnx_digest_ok` must become `false`.
#[test]
fn corrupt_srnx_chunk_digest_detected() {
    let Some(bytes) = try_load("at012000.srnx") else {
        eprintln!("skip corrupt_srnx_digest: fixture not found");
        return;
    };

    let file = SrnxFile::parse(&bytes).expect("initial parse");
    assert!(
        file.srnx_digest_ok,
        "pristine file must have valid SRNX digest"
    );

    // The SRNX chunk's CRC32C (4 bytes) sits immediately before the first
    // body chunk.
    #[allow(clippy::cast_possible_truncation)]
    let body_start = file.chunks.first().expect("has body chunks").file_offset as usize;
    let crc_start = body_start - 4; // CRC32C is 4 bytes

    let mut tampered = bytes.clone();
    tampered[crc_start] ^= 0x01;

    let file2 = SrnxFile::parse(&tampered).expect("parse still succeeds");
    assert!(
        !file2.srnx_digest_ok,
        "SRNX chunk digest should fail after CRC corruption"
    );
}
