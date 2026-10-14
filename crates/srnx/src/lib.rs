#![doc = include_str!("../README.md")]
//!
//! ## API stability
//!
//! The public API is [`reader`] and [`model`].  The hidden modules are
//! format internals, public only for `srnx-diff` and the integration tests,
//! and are not covered by semver.

pub mod model;
pub mod reader;

mod chunk;
mod compat;
mod evtf;
mod leb128;
mod sate;
mod sdir;
mod soc2;
mod srnx_header;

#[doc(hidden)]
pub mod crinex;
#[doc(hidden)]
pub mod diff;
#[doc(hidden)]
pub mod digest;
#[doc(hidden)]
pub mod epoc;
#[doc(hidden)]
pub mod file;
#[doc(hidden)]
pub mod rinex2_obs;
#[doc(hidden)]
pub mod rinex3_obs;
#[doc(hidden)]
pub mod rinex_header;
#[doc(hidden)]
pub mod socd;
#[doc(hidden)]
pub mod srnx_reader;
