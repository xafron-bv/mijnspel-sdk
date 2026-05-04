//! mijnspel-backend SDK — shared between per-game WASM modules and the host
//! loader. The crate has two halves selected by Cargo features:
//!
//! - default: ABI types and guest-side macro that per-game WASM modules
//!   depend on.
//! - `host`: wasmtime + S3-backed loader and the per-(appId, sha) compiled
//!   module cache. Pulled in by the lambda crate.

pub mod abi;
pub mod guest;

#[cfg(feature = "host")]
pub mod cache;
#[cfg(feature = "host")]
pub mod host;

pub use abi::{
    HostJson, LeaderboardStatIds, PriorStatIds, ProcessUpdateOutput, StatUpdate,
    StateEnvelopeRef, ValidateOutcome, HOST_ABI_VERSION,
};
