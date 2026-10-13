# srnx-clean

Clean-room Rust implementation of a **Succinct RINEX (SRNX)** reader, plus
RINEX v2 and v3/v4 observation readers, plus a utility to compare two files
(SRNX or RINEX) for semantic equivalence.

## Clean-room rule

This is a clean-room reimplementation. Do **not** consult any existing SRNX
implementation source code (the C reference, helper binaries' source, etc.).
Allowed inputs are:

- The format specs in [docs/](docs/) — `Succinct_RINEX.md`, `rinex211.txt`,
  `rinex_4.02.pdf`.
- The example files in [rnx-data/](rnx-data/).
- The behaviour of the prebuilt helper binaries (treated as black boxes — see
  below).

If a question can only be answered by reading the reference source, ask the
user instead of looking.

## Repository layout

- [docs/](docs/) — format specifications (read-only references).
  - [docs/Succinct_RINEX.md](docs/Succinct_RINEX.md) — SRNX spec, revision 1.
  - [docs/rinex211.txt](docs/rinex211.txt) — RINEX v2.11 spec.
  - [docs/rinex_4.02.pdf](docs/rinex_4.02.pdf) — RINEX v4.02 spec (v3 is a
    subset, modulo the version field). Earlier v3.0x/v4.0x specs are not in
    the repo; ask the user if a version-specific detail matters.
- [rnx-data/](rnx-data/) — sample inputs (symlink to `../rnx-data`). Mix of
  `.rnx` (RINEX v2/v3), `.20o`/`.26o` (RINEX v2 obs), `.crx` (Hatanaka), and
  `.srnx` files. The `.srnx` files were produced by `rnx2srnx` (default
  options, so revision 1.1 / SOC2) from the matching `.rnx`/`.2?o` source,
  or `.22d` Hatanaka source, which `rnx2srnx` reads directly. Tests that
  need revision 1.0 SOCD chunks generate them with `rnx2srnx --socd`.
- `PLAN.md` — implementation plan and milestone tracking.

The Rust workspace will live at the repo root once bootstrapped (see PLAN.md).
Suggested layout: a `srnx` library crate plus `srnx-diff` binary crate, in a
Cargo workspace.

## Helper binaries (in `$PATH`)

These are reference tools, useful for generating fixtures and cross-checking
behaviour. **Do not read their source.** Treat them as oracles.

- `rnx2srnx [--chunk-digest=<id>] [--file-digest=<id>] <input.rnx> [output.srnx]`
  — converts RINEX → SRNX. Default digests appear to be CRC32C (chunk) and
  BLAKE3-32 (file).
- `srnx2rnx <input.srnx> [output.rnx]` — converts SRNX → RINEX. Useful for
  round-trip testing.
- `srnx_scan <file.srnx>...` — prints chunk-level summary statistics
  (FOURCC counts, scale factors, delta orders, block-header histogram). Good
  for sanity-checking a parser without exposing observation values.

All `.srnx` files in `rnx-data/` are current-format and parse cleanly with
`srnx_scan`. If a fixture ever fails to parse, suspect a stale SRNX-revision
file before suspecting the parser; ask the user to regenerate.

## Format gotchas worth remembering

These are easy to miss on a first read of `docs/Succinct_RINEX.md`:

- Integers are **little-endian LEB128**. Signed values use ZigZag (SLEB128).
  `EPOC` interval is signed: negative = seconds, positive = seconds × 1e7.
- The `SRNX` chunk's payload may have **trailing padding** (so the SDIR offset
  can be patched in after the rest of the file is written). Read exactly the
  declared payload length; ignore extra bytes.
- Per-chunk digests cover `FOURCC || payload_length || payload`, **not** just
  the payload. The file-level digest covers the entire file from the start
  of the `SRNX` FOURCC until just before the file-level digest, with length
  supplied externally (file size).
- `RHDR` and `EVTF` newlines are LF only — even when the source RINEX used
  CRLF.
- `SATE` offsets to `SOCD` are **SLEB128 relative to the start of the SATE
  chunk's FOURCC**, not absolute file offsets. Zero means "never observed".
- `SOCD` packed observations: the encoding-schema byte is `delta_order +
  (8 if explicit_scale else 0)`, so values 0–7 = no explicit scale, values
  8–15 = explicit scale present. Default scale (no explicit) is 1000×.
- Bit-matrix blocks store values **bitwise-transposed**, MSB-first, sign-
  extended from the top bit. The matrix is `(k+1)` rows of `m/8` bytes each,
  where `m ∈ {8, 16, 32, 64, 128}`.
- Block tag `0xFD` = run of absent values (`INT64_MIN` sentinel), `0xFE` =
  run of zeros, `0xFF` = run of SLEB128 values. Absent values do **not**
  advance the delta accumulator.
- Per-satellite epoch presence in `SATE` is run-length encoded as
  `(num_runs - 1)` followed by interleaved `(absent_count - 1, present_count - 1)`
  pairs, all ULEB128. The first pair always starts with absence (which may be
  zero, encoded as ULEB 0).
- Receiver clock offsets in `EPOC` are RLE'd as `(SLEB128 value × 1e12,
  ULEB128 count - 1)`. Trailing zeros may be omitted.

RINEX text-format gotchas (all versions unless noted):

- "Missing observations are written as 0.0 or blanks": a value of exactly
  zero is absent, and `rnx2srnx` drops it (with its LLI/SSI). The shared
  F14.3 field parser in `rinex2_obs.rs` treats zero as absent for v2 and v3.
- SRNX keeps flag-1 (power failure) and flag-6 (cycle slip) records only in
  `EVTF`, never in `EPOC`/`SATE`/`SOCD`/`SOC2`. Both RINEX readers turn them
  into events, not observation epochs.
- RINEX 4.02 epoch records may end with `1X,I5.5`: five more digits of
  seconds (picoseconds), with the `F11.7` seconds floored. It's stored as
  `EpochRecord::extra_ps`. SRNX `EPOC` has only 1e-7 s resolution, and
  `rnx2srnx` rejects such files ("clock offset has bad format").
  `crates/srnx/tests/data/rinex402_picoseconds.rnx` exercises this.
- v3/v4 observation records are one line per satellite with no 80-column
  limit. Within an epoch, `srnx2rnx` sorts satellites, so line order is not
  semantic.

## Development conventions

- Rust edition 2021, stable toolchain. MSRV: 1.75 (`rust-version` in
  Cargo.toml); keep `Cargo.lock` at lockfile v3 and dependencies pinned to
  versions that build on 1.75. No nightly features.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass.
- Prefer `#![forbid(unsafe_code)]` in the library crate; the bit-matrix
  decoder does not require unsafe.
- Use the `byteorder`-free standard library + small focused crates. For
  digests it's fine to use the `crc32c` and `blake3` crates — clean-room
  applies to the SRNX format, not to standard cryptographic primitives.
- For LEB128 we may either implement inline (trivial, ~30 lines) or use the
  `leb128` crate; default to implementing inline so the codec stays auditable
  alongside the spec.
- Errors: a single `SrnxError` enum per crate, `thiserror` is fine. No
  panics on malformed input — return errors. Panics are reserved for true
  invariant violations.

## Testing strategy (summary; details in PLAN.md)

- Unit tests for each codec primitive (LEB128, CRC32C wiring, bit matrix,
  delta integrator) with hand-built byte sequences.
- Golden tests using `srnx_scan` output as ground truth for chunk counts,
  scale-factor distributions, delta-order distributions, and block-header
  histograms over the `rnx-data/` corpus.
- Round-trip test: `rnx2srnx <foo.rnx>` followed by parsing with our reader,
  cross-checked against `srnx2rnx`-then-our-RINEX-v2-reader on the original.
- The semantic-diff utility is itself the highest-level integration test:
  `(rnx, srnx)` pairs in `rnx-data/` should diff clean.

## Out of scope for MVP

- Writing SRNX (encoder).
- Hatanaka (`.crx`) compression.
- Streaming/async APIs — MVP is mmap or full read into memory.
