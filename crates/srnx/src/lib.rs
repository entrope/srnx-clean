//! Clean-room reader for the Succinct RINEX (SRNX) and RINEX v2 file formats.
//!
//! See `CLAUDE.md` and `PLAN.md` at the workspace root for scope and roadmap.

pub mod chunk;
pub mod digest;
pub mod leb128;
