//! Clean-room reader for the Succinct RINEX (SRNX) and RINEX v2 file formats.
//!
//! See `CLAUDE.md` and `PLAN.md` at the workspace root for scope and roadmap.

pub mod chunk;
pub mod digest;
pub mod epoc;
pub mod file;
pub mod leb128;
pub mod model;
pub mod rinex2_obs;
pub mod rinex_header;
pub mod sate;
pub mod sdir;
pub mod socd;
pub mod srnx_header;
pub mod srnx_reader;
