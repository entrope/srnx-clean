use std::process::ExitCode;

use srnx::digest::DigestId;
use srnx::file::SrnxFile;
use srnx::rinex_header;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return usage();
    };
    match first.as_str() {
        "--dump-chunks" => run_single_file_subcommand(&mut args, "--dump-chunks", dump_chunks),
        "--show-header" => run_single_file_subcommand(&mut args, "--show-header", show_header),
        "-h" | "--help" => {
            print_help();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown argument: {first}");
            usage()
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: srnx-diff --dump-chunks <file.srnx>");
    eprintln!("       srnx-diff --show-header <file.srnx>");
    eprintln!("       srnx-diff --help");
    ExitCode::from(2)
}

fn print_help() {
    println!("srnx-diff: SRNX file inspector and (eventually) semantic comparator");
    println!();
    println!("Subcommands:");
    println!("  --dump-chunks <file.srnx>   Print chunk-level summary of an SRNX file.");
    println!(
        "  --show-header <file.srnx>   Decode and print the RINEX header inside an SRNX file."
    );
    println!("  --help                      Show this help.");
}

fn run_single_file_subcommand(
    args: &mut impl Iterator<Item = String>,
    name: &str,
    f: fn(&str) -> Result<(), Box<dyn std::error::Error>>,
) -> ExitCode {
    let Some(path) = args.next() else {
        eprintln!("{name} requires a file argument");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("{name} accepts exactly one file argument");
        return ExitCode::from(2);
    }
    match f(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::from(1)
        }
    }
}

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
        // Render seconds with 7-decimal precision without going through f64.
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

fn digest_id_label(id: DigestId) -> &'static str {
    match id {
        DigestId::Null => "null",
        DigestId::Crc32c => "CRC32C",
        DigestId::Blake3_16 => "BLAKE3-16",
        DigestId::Blake3_32 => "BLAKE3-32",
    }
}

/// Render a FOURCC tag as a human-readable string. Non-ASCII bytes are
/// shown in `\xNN` form so the output stays printable on any input.
fn fourcc_str(tag: [u8; 4]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4);
    for b in tag {
        if b.is_ascii_graphic() {
            out.push(b as char);
        } else {
            // write! into a String is infallible.
            let _ = write!(out, "\\x{b:02X}");
        }
    }
    out
}
