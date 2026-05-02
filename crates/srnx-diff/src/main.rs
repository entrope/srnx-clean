use std::process::ExitCode;

use srnx::digest::DigestId;
use srnx::file::SrnxFile;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return usage();
    };
    match first.as_str() {
        "--dump-chunks" => {
            let Some(path) = args.next() else {
                eprintln!("--dump-chunks requires a file argument");
                return ExitCode::from(2);
            };
            if args.next().is_some() {
                eprintln!("--dump-chunks accepts exactly one file argument");
                return ExitCode::from(2);
            }
            match dump_chunks(&path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{path}: {e}");
                    ExitCode::from(1)
                }
            }
        }
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
    eprintln!("       srnx-diff --help");
    ExitCode::from(2)
}

fn print_help() {
    println!("srnx-diff: SRNX file inspector and (eventually) semantic comparator");
    println!();
    println!("Subcommands:");
    println!("  --dump-chunks <file.srnx>   Print chunk-level summary of an SRNX file.");
    println!("  --help                      Show this help.");
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
