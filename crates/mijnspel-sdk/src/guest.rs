//! Guest-side helpers that per-game WASM modules use to expose a `GameModule`
//! impl as the WASM exports the host expects.
//!
//! The macro takes a type that implements [`GuestRuntime`] (or
//! [`GuestMigration`]) and emits the JSON-boundary glue: `__alloc`, `__free`,
//! `abi_version`, plus one wrapper per JSON export.

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use crate::abi::{HostJson, SignalOutcome, ValidateOutcome};

/// Trait the per-game runtime crate implements. The host calls JSON in / JSON
/// out; this trait expresses each method in typed Rust over `serde_json::Value`
/// to keep the boundary uniform.
///
/// Inputs and outputs are intentionally `Value` rather than per-method types:
/// the per-game crate owns the schema and validates internally, so the SDK
/// surface stays stable as game schemas evolve.
pub trait GuestRuntime {
    fn current_schema_version() -> i64;

    fn validate_create_request(input: Value) -> ValidateOutcome;
    fn validate_update_request(input: Value) -> ValidateOutcome;
    fn validate_lobby_create(input: Value) -> ValidateOutcome;
    fn lobby_policy() -> crate::abi::LobbyPolicy;

    fn create_initial_state(input: Value) -> Value;
    fn state_for_lobby_start(input: Value) -> Value;

    fn prior_stat_ids() -> crate::abi::PriorStatIds;
    fn process_update(input: Value) -> crate::abi::ProcessUpdateOutput;
    fn leaderboard_stat_ids() -> crate::abi::LeaderboardStatIds;
    /// Extract player ids referenced by per-game fields on a row
    /// (e.g. gunzi's `blackSub` and `whiteSub`).
    fn players_in_row(row: Value) -> crate::abi::PlayersInRow;

    /// Optional. Per-game session signal handler. Default rejects so
    /// games that have not opted in just return "signaling unsupported";
    /// games that want signaling override this AND emit the wasm32
    /// `process_signal` extern wrapper so the host can probe the export.
    ///
    /// `input` shape: `{ appId, gameId, state, signalType, callerSub,
    /// payload, row }`. The default returns Reject so the host's optional
    /// probe is the only check that matters in practice — the override
    /// is what the host will actually call.
    fn process_signal(_input: Value) -> SignalOutcome {
        SignalOutcome::Reject {
            message: "signaling unsupported".to_string(),
        }
    }
}

/// Trait the per-game migration crate implements. One pairwise stepper per
/// direction; the host chains them.
pub trait GuestMigration {
    fn current_schema_version() -> i64;
    fn min_supported_version() -> i64;

    /// Walk a state from `version - 1` to `version`. Receives the row shape
    /// `{ "version": i64, "state": Value }` and returns the migrated state.
    fn upgrade_from_previous(version: i64, state: Value) -> Value;

    /// Walk a state from `version` to `version - 1`.
    fn downgrade_to_previous(version: i64, state: Value) -> Value;
}

/// Encode any `Serialize` payload as a JSON `HostJson` envelope.
pub fn encode_host_json<T: Serialize>(value: T) -> Vec<u8> {
    let envelope = HostJson::new(serde_json::to_value(value).unwrap_or(Value::Null));
    serde_json::to_vec(&envelope).unwrap_or_default()
}

/// Decode a `HostJson`-shaped JSON document into the payload's `data` field.
pub fn decode_host_json<T: DeserializeOwned + Default>(bytes: &[u8]) -> T {
    let envelope: HostJson = serde_json::from_slice(bytes).unwrap_or_default();
    serde_json::from_value(envelope.data).unwrap_or_default()
}

/// Macro-generated allocators sit at fixed export names `__alloc` / `__free`.
/// Per-game crates would put `mijnspel_sdk::guest::game_module!(MyGame)`
/// at the top of their lib.rs to wire the exports.
///
/// We don't ship the macro's output here because the macro lives behind a
/// `#[cfg(target_arch = "wasm32")]` cfg in the per-game crate; the SDK only
/// needs to expose the helpers above and let the per-game crate own the
/// `#[no_mangle] extern "C"` declarations. Keeping it in the per-game crate
/// avoids forcing every workspace consumer of the SDK to compile the
/// `wasm32`-only attributes.
///
/// The reference implementation lives in
/// [`crate::guest::reference`] and is what each per-game crate instantiates.
pub mod reference {
    //! Reference shape for what a per-game crate's wasm32 glue looks like.
    //! Documented as comments rather than a `macro_rules!` because the
    //! `#[no_mangle] extern "C"` blocks must live in the leaf crate to be
    //! picked up by the linker.
    //!
    //! ```text
    //! // in per-game-runtime/src/lib.rs
    //! use mijnspel_sdk::abi::{HOST_ABI_VERSION, pack_ptr_len, unpack_ptr_len};
    //! use mijnspel_sdk::guest::{decode_host_json, encode_host_json, GuestRuntime};
    //!
    //! pub struct MyGame;
    //! impl GuestRuntime for MyGame { /* … */ }
    //!
    //! #[no_mangle]
    //! pub extern "C" fn __alloc(len: u32) -> u32 {
    //!     let mut buf = Vec::<u8>::with_capacity(len as usize);
    //!     let ptr = buf.as_mut_ptr() as u32;
    //!     std::mem::forget(buf);
    //!     ptr
    //! }
    //!
    //! #[no_mangle]
    //! pub extern "C" fn __free(ptr: u32, len: u32) {
    //!     unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, len as usize); }
    //! }
    //!
    //! #[no_mangle]
    //! pub extern "C" fn abi_version() -> i64 { HOST_ABI_VERSION }
    //!
    //! #[no_mangle]
    //! pub extern "C" fn current_schema_version() -> i64 {
    //!     MyGame::current_schema_version()
    //! }
    //!
    //! // … and one wrapper per JSON export, of the shape:
    //! //
    //! // #[no_mangle]
    //! // pub extern "C" fn process_update(ptr: u32, len: u32) -> u64 {
    //! //     let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    //! //     let input = decode_host_json::<serde_json::Value>(bytes);
    //! //     let out = MyGame::process_update(input);
    //! //     let buf = encode_host_json(out);
    //! //     let len = buf.len() as u32;
    //! //     let leaked = Box::leak(buf.into_boxed_slice());
    //! //     pack_ptr_len(leaked.as_ptr() as u32, len)
    //! // }
    //! ```
}
