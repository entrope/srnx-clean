# Implementation plan: srnx-clean (MVP)

Goal: a Rust library that reads SRNX and RINEX v2 observation files, plus a
`srnx-diff` CLI that reports whether two files (any combination of SRNX /
RINEX v2) are semantically equivalent.

See [CLAUDE.md](CLAUDE.md) for clean-room rules and format gotchas.

## Crate layout

```text
Cargo.toml                 # workspace
crates/
  srnx/                    # library: SRNX + RINEX v2 reader, semantic model
    src/
      lib.rs
      leb128.rs            # ULEB128 / SLEB128 codecs
      digest.rs            # CRC32C + BLAKE3 verification
      chunk.rs             # FOURCC framing, chunk iterator
      srnx/                # SRNX-specific decoders
        header.rs          # SRNX, RHDR, SDIR
        epoch.rs           # EPOC: timestamps, clock offsets
        satellite.rs       # SATE: per-sat presence + SOCD offsets
        observation.rs     # SOCD: LLI/SSI RLE + bit-matrix delta decoder
      rinex2/              # RINEX v2 text parser
        header.rs
        epoch.rs
        observation.rs
      model.rs             # shared semantic model (Header, Epoch, Obs, ...)
      reader.rs            # unified `Reader` trait over both formats
  srnx-diff/               # binary: semantic comparison CLI
    src/main.rs
```

## Semantic model (shared)

The diff tool needs a normalized in-memory representation that both readers
populate. Sketch:

```rust
struct File {
    header: Header,                      // station, antenna, obs codes per system, ...
    epochs: Vec<EpochMeta>,              // timestamp, flag, clock offset
    events: Vec<(usize, EventRecord)>,   // (epoch_index, raw record text)
    // Observations indexed by (satellite, obs_code) -> per-epoch slot.
    // Absent epochs stored as None/sentinel.
    observations: BTreeMap<(SatId, ObsCode), Vec<Option<Observation>>>,
}

struct Observation {
    value: i64,    // scaled integer, with scale recorded in series header
    scale: u32,    // 1, 250 (=0.25), 2 (=0.002 * 1000), ...
    lli: u8,       // 0..=9 or b' '
    ssi: u8,
}
```

Equality compares the normalized model, not byte-level file contents.

## Phases

### Phase 0 — Bootstrap (½ day)

- `cargo init --lib crates/srnx`, `cargo init --bin crates/srnx-diff`,
  workspace `Cargo.toml`.
- `.gitignore` for `target/`.
- Wire `cargo fmt`, `cargo clippy -D warnings` into a `justfile` or
  `Makefile`.

### Phase 1 — Codec primitives (1 day)

- `leb128.rs`: `read_uleb128(&mut &[u8]) -> Result<u64>` and SLEB128 variant.
  Reject overlong / overflowing encodings.
- `digest.rs`: thin wrappers around `crc32c` and `blake3` keyed by digest ID
  (0/2/4/5). Verification helpers, not generation.
- Unit tests with hand-built fixtures including edge cases (zero, single
  byte, maximum values, negative SLEB128).

### Phase 2 — Chunk framing (½ day)

- `chunk.rs`: `ChunkIter` over a byte slice. Each step yields
  `{ fourcc: [u8;4], payload: &[u8], digest_ok: bool }`. Per-chunk digest is
  decoded based on the digest ID found in the SRNX chunk.
- Tolerate unknown FOURCCs (skip with warning) per spec — additional chunks
  may appear in any order.

### Phase 3 — SRNX top-level (½ day)

- Decode `SRNX` chunk: version, digest IDs, SDIR offset (treat trailing
  padding correctly).
- Decode `RHDR` chunk: hand off bytes to a header parser.
- Decode `SDIR` chunk if present: `EPOC` offset, first `EVTF` offset,
  `(sat_id, sate_offset)` table.
- Verify the file-level digest if non-null.
- Goal: `srnx-diff --dump-chunks` produces a chunk listing matching
  `srnx_scan`'s FOURCC counts on every file in `rnx-data/`.

### Phase 4 — RHDR parsing for RINEX v2 (1 day)

- Identify version from `RINEX VERSION / TYPE`.
- For v2: parse `# / TYPES OF OBSERV` (with continuation lines), `INTERVAL`,
  `TIME OF FIRST OBS`, `WAVELENGTH FACT L1/2`, antenna/marker fields.
- Build the per-system observation-code table that `SATE`/`SOCD` chunks
  index into.
- For v3 RHDRs that appear in SRNX files in `rnx-data/`: parse just enough
  (`SYS / # / OBS TYPES`) to map satellite-system → obs-code list.

### Phase 5 — EPOC decoding (½ day)

- Read epoch span list, materialize a `Vec<DateTime>` (or
  `Vec<(date, time_ns)>` to avoid the chrono dependency).
- Apply the leap-second / day-boundary "do not wrap hours, do reset
  minutes/seconds when sum is exactly 60" rule from the spec.
- Decode RLE'd receiver clock offsets, padding trailing zeros.
- Cross-check epoch count against `srnx_scan` for each fixture.

### Phase 6 — SATE + SOCD decoding (2 days, the hard part)

- `SATE`: parse satellite name, list of SLEB128 SOCD offsets (relative to
  SATE FOURCC), then epoch-presence RLE (one bit per epoch, conceptually).
- `SOCD`:
  - 8-byte name field, count-minus-1 of observations.
  - LLI / SSI RLE decoders (shared helper).
  - Packed-observation block decoder:
    - Read length, schema byte → `(delta_order, has_explicit_scale)`.
    - Read explicit scale if present (default scale = 1000).
    - Read `delta_order` initial-state SLEB128 values.
    - Loop blocks until the declared length is consumed:
      - `0x00..0x9F` → bit matrix (`m`, `k+1`) decode + sign-extend +
        delta-integrate.
      - `0xFD` → absent run, emit `INT64_MIN`, do NOT advance delta state.
      - `0xFE` → zero run, emit zeros THROUGH the delta integrator.
      - `0xFF` → SLEB128 run, emit values THROUGH the delta integrator.
- Combine `SATE` presence with `SOCD` to fill `observations[(sat, code)]`.
- Validation: total observation count per satellite must match what
  presence + SOCD lengths imply.

### Phase 7 — RINEX v2 text reader (2 days)

- Header parser: same v2 logic as Phase 4, but consuming the file directly
  (line-oriented, fixed-column).
- Epoch parser: line ` YY MM DD HH MM SS.SSSSSSS  EFLAG  NSAT  G01G02...`
  with continuation lines for >12 satellites; clock offset in the trailing
  field.
- Observation parser: 5 obs per 80-char line, 14.3 fixed-format value plus
  LLI + SSI single-char fields. Handle blanks (= absent observation).
- Special event records (epoch flag != 0): capture verbatim into
  `events[]`.
- Populate the same semantic model as the SRNX reader.

### Phase 8 — `srnx-diff` CLI (½ day)

- `srnx-diff <a> <b>` — auto-detect format by extension / magic bytes.
- Compare normalized models, walking in a deterministic order:
  - Header fields (with normalization rules from CLAUDE.md: LF newlines,
    satellite-order independence, leading zeros stripped).
  - Epoch timestamps and counts.
  - Per-(sat, code) observation series — values, LLI, SSI, scale
    equivalence (e.g. `value=2, scale=500` ≡ `value=1, scale=1000`).
  - Special event records.
- Output: human-readable diff (first N divergences) + nonzero exit code.
  Optional `--quiet` for CI use.

### Phase 9 — Test corpus + CI (½ day)

- Golden tests driven by `srnx_scan` output: parse the fixture, recompute
  chunk-count / scale / delta-order / block-header histograms, compare.
- Round-trip: for each `*.rnx` in `rnx-data/`, run
  `rnx2srnx`, parse both files with our reader, assert `srnx-diff`-style
  semantic equality. Skip large files in fast CI; full corpus in a slow
  job.
- Negative tests: corrupt one byte in a fixture, ensure digest verification
  catches it.

## Risk / unknowns

- **RINEX v2 has dialectal variation** (teqc-isms, missing optional headers,
  satellites identified with leading-blank `G 1` etc). Plan to harden by
  feeding every `*.20o`/`*.26o` in `rnx-data/` and `rnx-data/2022_183/`
  through the parser; expect edge cases to surface.
- **Bit-matrix sign extension** is the most error-prone codec. Will write
  property tests: encode random `Vec<i64>`-with-bounded-bit-width, decode,
  round-trip equality.
- **Scale equivalence in diff** — e.g. an observation series stored with
  scale 250 in one file and scale 1000 in another, with values rescaled
  accordingly, is semantically identical. The diff must compare against a
  canonical scale (probably "smallest scale that exactly represents all
  values").

## Estimated total: ~8–10 working days to MVP

Post-MVP candidates (not in this plan): SRNX writer,
streaming / mmap-friendly chunked iteration, Python bindings.

Done after MVP: RINEX v3/v4 observation reader (`rinex3_obs.rs`, covering
3.00–4.02 including the 4.02 picosecond epoch extension), wired into
`srnx-diff`.

SRNX revision 1.2: `EPO2` epoch chunk (picosecond epochs) in `epoc.rs`,
with SOC2 rate predictors using the epoch chunk's time unit.

Compact RINEX (Hatanaka) reader: `crinex.rs` decompresses CRINEX 1.0, 3.0
and 3.1 to RINEX text (byte-identical to RNXCMP 4.2.0 `crx2rnx`) and parses
it with the RINEX v2 or v3/v4 reader; wired into `srnx-diff`.
