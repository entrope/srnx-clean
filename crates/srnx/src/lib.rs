//! Clean-room reader for the Succinct RINEX (SRNX) format, RINEX v2, v3 and
//! v4 observation files, and Compact RINEX (Hatanaka) files.
//!
//! See `CLAUDE.md` and `PLAN.md` at the workspace root for scope and roadmap.

pub mod chunk;
mod compat;
pub mod crinex;
pub mod diff;
pub mod digest;
pub mod epoc;
pub mod evtf;
pub mod file;
pub mod leb128;
pub mod model;
pub mod reader;
pub mod rinex2_obs;
pub mod rinex3_obs;
pub mod rinex_header;
pub mod sate;
pub mod sdir;
pub mod soc2;
pub mod socd;
pub mod srnx_header;
pub mod srnx_reader;
