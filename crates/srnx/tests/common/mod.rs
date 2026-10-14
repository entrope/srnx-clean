//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use srnx::diff;
use srnx::file::SrnxFile;
use srnx::model::ObsFile;
use srnx::{crinex, socd};

/// The committed fixture directory, `tests/data`.
pub fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Read a fixture from `tests/data`, panicking if it is missing.
pub fn data(name: &str) -> Vec<u8> {
    let path = data_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The RINEX text of fixture `stem`: `stem.rnx`, or for fixtures stored only
/// in compact form, `stem.crx` decompressed.
pub fn rinex_text(stem: &str) -> Vec<u8> {
    std::fs::read(data_dir().join(format!("{stem}.rnx")))
        .unwrap_or_else(|_| crinex::decompress(&data(&format!("{stem}.crx"))).unwrap())
}

/// Assert that two observation files are semantically equal, and that their
/// events (which `diff::compare` does not check) are at the same epochs.
pub fn assert_same(a: &ObsFile, b: &ObsFile, la: &str, lb: &str) {
    assert!(!a.epochs.is_empty(), "{la}: no epochs");
    let diffs = diff::compare(a, b, la, lb);
    assert!(
        diffs.is_empty(),
        "{la} vs {lb}: {} diff(s):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
    let positions = |f: &ObsFile| f.events.iter().map(|(i, _)| *i).collect::<Vec<_>>();
    assert_eq!(positions(a), positions(b), "{la} vs {lb}: event positions");
}

// ── Aggregate SRNX statistics ─────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct SrnxStats {
    chunk_counts: BTreeMap<[u8; 4], usize>,
    total_epochs: usize,
    scale_histogram: BTreeMap<u32, usize>,
    delta_order_histogram: BTreeMap<u8, usize>,
    block_header_histogram: BTreeMap<u8, usize>,
}

pub fn compute_stats(bytes: &[u8]) -> SrnxStats {
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

#[allow(clippy::too_many_lines)]
pub fn parse_scan_output(text: &str) -> Option<SrnxStats> {
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

    Some(SrnxStats {
        chunk_counts,
        total_epochs,
        scale_histogram,
        delta_order_histogram,
        block_header_histogram,
    })
}

/// Assert that statistics computed from `bytes` match an `srnx_scan` report.
pub fn assert_matches_scan(bytes: &[u8], scan: &str, name: &str) {
    let expected = parse_scan_output(scan).unwrap_or_else(|| panic!("{name}: bad scan output"));
    let actual = compute_stats(bytes);
    for (tag, &exp) in &expected.chunk_counts {
        let act = actual.chunk_counts.get(tag).copied().unwrap_or(0);
        assert_eq!(
            act,
            exp,
            "{name}: chunk count for {}",
            String::from_utf8_lossy(tag)
        );
    }
    assert_eq!(
        actual.total_epochs, expected.total_epochs,
        "{name}: epoch count"
    );
    assert_eq!(
        actual.scale_histogram, expected.scale_histogram,
        "{name}: scale-factor histogram"
    );
    assert_eq!(
        actual.delta_order_histogram, expected.delta_order_histogram,
        "{name}: delta-order histogram"
    );
    assert_eq!(
        actual.block_header_histogram, expected.block_header_histogram,
        "{name}: block-header histogram"
    );
}
