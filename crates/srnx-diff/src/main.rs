use std::collections::BTreeSet;
use std::process::ExitCode;

use srnx::digest::DigestId;
use srnx::epoc;
use srnx::file::SrnxFile;
use srnx::model::{EpochRecord, Obs, ObsFile, SatId};
use srnx::rinex2_obs;
use srnx::rinex_header::{self, ObsCode};
use srnx::srnx_reader;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        [flag, path] if flag == "--dump-chunks" => run(dump_chunks(path)),
        [flag, path] if flag == "--show-header" => run(show_header(path)),
        [flag, path] if flag == "--show-epochs" => run(show_epochs(path)),
        [path_a, path_b] if !path_a.starts_with('-') && !path_b.starts_with('-') => {
            run_diff(path_a, path_b, false)
        }
        [flag, path_a, path_b]
            if flag == "--quiet" && !path_a.starts_with('-') && !path_b.starts_with('-') =>
        {
            run_diff(path_a, path_b, true)
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: srnx-diff [--quiet] <file-a> <file-b>       compare two files");
    eprintln!("       srnx-diff --dump-chunks <file.srnx>          chunk-level summary");
    eprintln!("       srnx-diff --show-header <file.srnx>          decoded RINEX header");
    eprintln!("       srnx-diff --show-epochs <file.srnx>          epoch timestamps");
    eprintln!("       srnx-diff --help");
    ExitCode::from(2)
}

fn print_help() {
    println!("srnx-diff: semantic comparator and inspector for SRNX / RINEX v2 observation files");
    println!();
    println!("USAGE");
    println!("  srnx-diff [--quiet] <file-a> <file-b>");
    println!("      Compare two files (any combination of .srnx and RINEX v2 .rnx/.20o/.26o).");
    println!("      Exits 0 if files are semantically equivalent, 1 if they differ.");
    println!("      --quiet: suppress diff output (for CI pipelines).");
    println!();
    println!("  srnx-diff --dump-chunks <file.srnx>");
    println!("      Print chunk-level summary of an SRNX file.");
    println!();
    println!("  srnx-diff --show-header <file.srnx>");
    println!("      Decode and print the RINEX header inside an SRNX file.");
    println!();
    println!("  srnx-diff --show-epochs <file.srnx>");
    println!("      Decode epoch timestamps from an SRNX file.");
}

fn run(r: Result<(), Box<dyn std::error::Error>>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Format detection and loading
// ---------------------------------------------------------------------------

enum FileFormat {
    Srnx,
    Rinex2,
}

fn detect_format(path: &str, bytes: &[u8]) -> FileFormat {
    if bytes.starts_with(b"SRNX") {
        return FileFormat::Srnx;
    }
    if std::path::Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("srnx"))
    {
        return FileFormat::Srnx;
    }
    FileFormat::Rinex2
}

fn load_obs_file(path: &str) -> Result<ObsFile, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    match detect_format(path, &bytes) {
        FileFormat::Srnx => Ok(srnx_reader::read(&bytes)?),
        FileFormat::Rinex2 => Ok(rinex2_obs::parse(&bytes)?),
    }
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Maximum number of individual differences to report before stopping.
const MAX_DIFFS: usize = 50;

fn run_diff(path_a: &str, path_b: &str, quiet: bool) -> ExitCode {
    let a = match load_obs_file(path_a) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path_a}: {e}");
            return ExitCode::from(2);
        }
    };
    let b = match load_obs_file(path_b) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path_b}: {e}");
            return ExitCode::from(2);
        }
    };

    let mut diffs = 0usize;
    diff_all(&a, &b, path_a, path_b, quiet, &mut diffs);

    if diffs == 0 {
        if !quiet {
            println!("Files match.");
        }
        ExitCode::SUCCESS
    } else {
        if !quiet {
            if diffs >= MAX_DIFFS {
                println!("(stopped after {MAX_DIFFS} differences)");
            }
            println!("{diffs} difference(s) found.");
        }
        ExitCode::from(1)
    }
}

fn diff_all(
    a: &ObsFile,
    b: &ObsFile,
    label_a: &str,
    label_b: &str,
    quiet: bool,
    diffs: &mut usize,
) {
    diff_epochs(a, b, label_a, label_b, quiet, diffs);
    diff_observations(a, b, label_a, label_b, quiet, diffs);
}

// -- Epoch comparison --------------------------------------------------------

fn diff_epochs(
    a: &ObsFile,
    b: &ObsFile,
    label_a: &str,
    label_b: &str,
    quiet: bool,
    diffs: &mut usize,
) {
    let na = a.epochs.len();
    let nb = b.epochs.len();
    if na != nb {
        bump(diffs, quiet, || {
            format!("epoch count: {label_a}={na}  {label_b}={nb}")
        });
    }

    let common = na.min(nb);
    for i in 0..common {
        if *diffs >= MAX_DIFFS {
            break;
        }
        let ea = &a.epochs[i];
        let eb = &b.epochs[i];
        if !epochs_eq(ea, eb) {
            bump(diffs, quiet, || {
                format!(
                    "epoch[{i}] timestamp: {} vs {}",
                    fmt_epoch(ea),
                    fmt_epoch(eb)
                )
            });
        } else if ea.clock_offset_e12 != eb.clock_offset_e12 {
            bump(diffs, quiet, || {
                format!(
                    "epoch[{i}] clock offset: {} vs {} (×10⁻¹²)",
                    ea.clock_offset_e12, eb.clock_offset_e12
                )
            });
        }
    }
}

fn epochs_eq(a: &EpochRecord, b: &EpochRecord) -> bool {
    a.year == b.year
        && a.month == b.month
        && a.day == b.day
        && a.hour == b.hour
        && a.minute == b.minute
        && a.seconds_e7 == b.seconds_e7
}

fn fmt_epoch(e: &EpochRecord) -> String {
    let whole = e.seconds_e7 / 10_000_000;
    let frac = e.seconds_e7 % 10_000_000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}",
        e.year, e.month, e.day, e.hour, e.minute, whole, frac
    )
}

// -- Observation comparison --------------------------------------------------

fn diff_observations(
    a: &ObsFile,
    b: &ObsFile,
    label_a: &str,
    label_b: &str,
    quiet: bool,
    diffs: &mut usize,
) {
    let keys_a: BTreeSet<_> = a.observations.keys().collect();
    let keys_b: BTreeSet<_> = b.observations.keys().collect();

    for key in keys_a.union(&keys_b) {
        if *diffs >= MAX_DIFFS {
            break;
        }
        match (a.observations.get(key), b.observations.get(key)) {
            (None, _) => bump(diffs, quiet, || {
                format!("{}/{}: only in {label_b}", key.0, key.1.as_str())
            }),
            (_, None) => bump(diffs, quiet, || {
                format!("{}/{}: only in {label_a}", key.0, key.1.as_str())
            }),
            (Some(sa), Some(sb)) => {
                diff_series(**key, sa, sb, a, b, label_a, label_b, quiet, diffs);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn diff_series(
    key: (SatId, ObsCode),
    sa: &[(usize, Obs)],
    sb: &[(usize, Obs)],
    file_a: &ObsFile,
    file_b: &ObsFile,
    label_a: &str,
    label_b: &str,
    quiet: bool,
    diffs: &mut usize,
) {
    let prefix = format!("{}/{}", key.0, key.1.as_str());
    let mut ia = 0;
    let mut ib = 0;

    while ia < sa.len() && ib < sb.len() && *diffs < MAX_DIFFS {
        let (ea, oa) = sa[ia];
        let (eb, ob) = sb[ib];

        match ea.cmp(&eb) {
            std::cmp::Ordering::Less => {
                bump(diffs, quiet, || {
                    format!(
                        "{prefix} epoch[{ea}] {}: only in {label_a}",
                        fmt_epoch(&file_a.epochs[ea])
                    )
                });
                ia += 1;
            }
            std::cmp::Ordering::Greater => {
                bump(diffs, quiet, || {
                    format!(
                        "{prefix} epoch[{eb}] {}: only in {label_b}",
                        fmt_epoch(&file_b.epochs[eb])
                    )
                });
                ib += 1;
            }
            std::cmp::Ordering::Equal => {
                if !obs_eq(oa, ob) {
                    bump(diffs, quiet, || {
                        format!(
                            "{prefix} epoch[{ea}] {}: value mismatch: {}  {}",
                            fmt_epoch(&file_a.epochs[ea]),
                            fmt_obs_detail(oa, label_a),
                            fmt_obs_detail(ob, label_b),
                        )
                    });
                }
                ia += 1;
                ib += 1;
            }
        }
    }

    while ia < sa.len() && *diffs < MAX_DIFFS {
        let (ea, _) = sa[ia];
        bump(diffs, quiet, || {
            format!(
                "{prefix} epoch[{ea}] {}: only in {label_a}",
                fmt_epoch(&file_a.epochs[ea])
            )
        });
        ia += 1;
    }
    while ib < sb.len() && *diffs < MAX_DIFFS {
        let (eb, _) = sb[ib];
        bump(diffs, quiet, || {
            format!(
                "{prefix} epoch[{eb}] {}: only in {label_b}",
                fmt_epoch(&file_b.epochs[eb])
            )
        });
        ib += 1;
    }
}

fn obs_eq(a: Obs, b: Obs) -> bool {
    let phys_a = i128::from(a.value) * i128::from(a.scale);
    let phys_b = i128::from(b.value) * i128::from(b.scale);
    phys_a == phys_b
        && norm_indicator(a.lli) == norm_indicator(b.lli)
        && norm_indicator(a.ssi) == norm_indicator(b.ssi)
}

/// Treat `b'0'` and `b' '` as equivalent (both mean "no information").
#[inline]
fn norm_indicator(b: u8) -> u8 {
    if b == b'0' {
        b' '
    } else {
        b
    }
}

fn fmt_obs_detail(obs: Obs, label: &str) -> String {
    let pm = i128::from(obs.value) * i128::from(obs.scale);
    let lli = norm_indicator(obs.lli) as char;
    let ssi = norm_indicator(obs.ssi) as char;
    format!("{label}(phys={pm}, lli='{lli}', ssi='{ssi}')")
}

fn bump(diffs: &mut usize, quiet: bool, msg: impl FnOnce() -> String) {
    *diffs += 1;
    if !quiet && *diffs <= MAX_DIFFS {
        println!("{}", msg());
    }
}

// ---------------------------------------------------------------------------
// Existing single-file subcommands (dump-chunks, show-header, show-epochs)
// ---------------------------------------------------------------------------

fn dump_chunks(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let file = SrnxFile::parse(&bytes)?;
    let h = &file.header;

    println!("File: {path} ({} bytes)", bytes.len());
    println!(
        "SRNX: version {}.{}, chunk-digest={} ({}), file-digest={} ({}), sdir-offset={}",
        h.major,
        h.minor,
        digest_id_label(h.chunk_digest),
        h.chunk_digest.byte_len(),
        digest_id_label(h.file_digest),
        h.file_digest.byte_len(),
        h.sdir_offset
    );
    println!(
        "  SRNX-chunk digest: {}",
        if file.srnx_digest_ok {
            "ok"
        } else {
            "MISMATCH"
        }
    );
    println!(
        "  file-level digest: {}",
        if file.file_digest_ok {
            "ok"
        } else {
            "MISMATCH"
        }
    );

    let counts = file.count_by_fourcc();
    println!("Body chunk counts:");
    for (fourcc, count) in &counts {
        println!("  {} : {count}", fourcc_str(*fourcc));
    }
    let total_body: usize = counts.values().sum();
    println!("  total body chunks: {total_body} (excludes the SRNX chunk)");

    let bad: Vec<_> = file.chunks.iter().filter(|c| !c.digest_ok).collect();
    if !bad.is_empty() {
        println!("Per-chunk digest mismatches:");
        for c in bad {
            println!(
                "  {} at file_offset={}",
                fourcc_str(c.fourcc),
                c.file_offset
            );
        }
    }

    if let Some(sdir) = file.sdir()? {
        println!(
            "SDIR: epoc-offset={}, evtf-offset={}, satellites={}",
            sdir.epoc_offset,
            sdir.evtf_offset,
            sdir.satellites.len()
        );
    } else {
        println!("SDIR: (not present)");
    }

    Ok(())
}

fn show_header(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let file = SrnxFile::parse(&bytes)?;
    let rhdr = file.rhdr_payload().ok_or("SRNX file has no RHDR chunk")?;
    let header = rinex_header::parse(rhdr)?;

    println!("File: {path}");
    println!(
        "RINEX version: {}.{}, file_type={}, sat_system={}",
        header.version.major,
        header.version.minor,
        header.file_type as char,
        header.sat_system.as_char()
    );
    if let Some(s) = header.marker_name.as_deref() {
        println!("Marker name: {s}");
    }
    if let Some(s) = header.marker_number.as_deref() {
        println!("Marker number: {s}");
    }
    if let Some(s) = header.observer.as_deref() {
        println!("Observer: {s}");
    }
    if let Some(s) = header.agency.as_deref() {
        println!("Agency: {s}");
    }
    if let Some(r) = &header.receiver {
        println!(
            "Receiver: serial={:?} model={:?} version={:?}",
            r.serial, r.model, r.version
        );
    }
    if let Some(a) = &header.antenna {
        println!("Antenna: serial={:?} model={:?}", a.serial, a.model);
    }
    if let Some([x, y, z]) = header.approx_position {
        println!("Approx position XYZ: [{x}, {y}, {z}]");
    }
    if let Some([h, e, n]) = header.antenna_delta_hen {
        println!("Antenna delta H/E/N: [{h}, {e}, {n}]");
    }
    if let Some(i) = header.interval {
        println!("Interval: {i} s");
    }
    if let Some(t) = &header.time_of_first_obs {
        let whole = t.seconds_e7 / 10_000_000;
        let frac = t.seconds_e7 % 10_000_000;
        println!(
            "Time of first obs: {:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07} {}",
            t.year, t.month, t.day, t.hour, t.minute, whole, frac, t.time_system
        );
    }
    if let Some(s) = header.leap_seconds {
        println!("Leap seconds: {s}");
    }
    println!(
        "Observation codes ({} total):",
        header.obs_codes.total_codes()
    );
    for (sys, codes) in &header.obs_codes.by_system {
        let names: Vec<&str> = codes.iter().map(rinex_header::ObsCode::as_str).collect();
        println!(
            "  {} ({} codes): {}",
            sys.as_char(),
            codes.len(),
            names.join(" ")
        );
    }
    if !header.comments.is_empty() {
        println!("Comments ({}):", header.comments.len());
        for c in &header.comments {
            println!("  {c}");
        }
    }
    if !header.unknown_lines.is_empty() {
        println!("Unrecognized header labels:");
        for (k, v) in &header.unknown_lines {
            println!("  [{k}] {v}");
        }
    }
    Ok(())
}

fn show_epochs(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let file = SrnxFile::parse(&bytes)?;
    let epoc = file.epoc()?.ok_or("SRNX file has no EPOC chunk")?;

    println!("File: {path}");
    println!("Total epochs: {}", epoc.epochs.len());
    if epoc.epochs.is_empty() {
        return Ok(());
    }

    let nonzero_clock = epoc
        .epochs
        .iter()
        .filter(|e| e.clock_offset_e12 != 0)
        .count();
    println!("Nonzero receiver clock offsets: {nonzero_clock}");

    let print_one = |label: &str, e: &epoc::Epoch| {
        let whole = e.seconds_e7 / 10_000_000;
        let frac = e.seconds_e7 % 10_000_000;
        println!(
            "  {label}: {:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}  clock_e12={}",
            e.year, e.month, e.day, e.hour, e.minute, whole, frac, e.clock_offset_e12
        );
    };
    print_one("first", &epoc.epochs[0]);
    if epoc.epochs.len() >= 2 {
        print_one("second", &epoc.epochs[1]);
    }
    if epoc.epochs.len() >= 3 {
        print_one("last  ", epoc.epochs.last().unwrap());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn digest_id_label(id: DigestId) -> &'static str {
    match id {
        DigestId::Null => "null",
        DigestId::Crc32c => "CRC32C",
        DigestId::Blake3_16 => "BLAKE3-16",
        DigestId::Blake3_32 => "BLAKE3-32",
    }
}

fn fourcc_str(tag: [u8; 4]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4);
    for b in tag {
        if b.is_ascii_graphic() {
            out.push(b as char);
        } else {
            let _ = write!(out, "\\x{b:02X}");
        }
    }
    out
}
