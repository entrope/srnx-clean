# srnx-rs

srnx-rs is a Rust package for reading SRNX files.
SRNX is a succinct format for representing GNSS observation data that is
semantically equivalent to standard RINEX observation-file formats.
srnx-rs does not read navigation messages or meteorological data files,
and does not write files.

This library can read observation files formatted as:

- RINEX v2.11 (probably earlier) through v4.02;
- CRINEX 1.0, 3.0 and 3.1;
- SRNX 1.0 through 1.2.

It presents a unified API for reading those formats, so that most code
can be agnostic of whether the input file is RINEX, CRINEX or SRNX.
Observation codes differ for RINEX v2/CRINEX 1 compared to the v3/v4
variants; this library does not normalize them.
This library supports picosecond-resolution epoch timestamps (an optional
feature since RINEX v4.02, CRINEX 3.1 and SRNX 1.2).

For more details, see [SRNX](https://github.com/entrope/srnx/).

## crate `srnx`

This crate implements the reader library, with the primary entry point
being `srnx::reader::read()`.
This returns an `srnx::model::ObsFile`, which contains:

- the file header, normalized to linefeed endings, with lookup by label;
- a vector of flag-0 observation epoch metadata;
- a vector of flag-1 through flag-6 special event records, with their
  respective (non-decreasing) indexes into the observation epochs; and
- a map of (satellite, observation code) -> observation lists, with
  each observation list containing tuples of epoch index and observations.

The map does not preserve the original order of satellites within an epoch.
Observation values are returned as i64 integers counting milli-units,
along with LLI and SSI bytes.
Absent values, including ones spelled as 0.000, are omitted.

```rust
use srnx::rinex_header::ObsCode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("abmf1190.26o")?;
    let l1c = ObsCode::from_v3("L1C").unwrap();
    let file = srnx::reader::read(&bytes, Some(&mut |_sat, code| code == l1c))?;
    Ok(())
}
```

## crate `srnx-diff`

This crate builds a `srnx-diff` executable that compares two RINEX-ish
files and reports whether they are semantically different.
It works across formats, and ignores details that the standards say are
not relevant (end-of-line conventions, trailing whitespace, observations
that are blank vs 0.000, order of satellites per epoch, and so forth).

Usage:

```sh
srnx-diff [--quiet] <file-a> <file-b>
srnx-diff --dump-chunks <file.srnx>
srnx-diff --show-header <file.srnx>
srnx-diff --show-epochs <file.srnx>
```

The first mode returns 0 if the files are equivalent, 1 if they differ
and 2 on bad arguments and corrupted files.
This allows using `--quiet` for automated testing or CI.
