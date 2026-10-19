//! Guest-side helpers that per-game WASM modules use to expose a
//! [`GuestRuntime`] (or [`GuestMigration`]) impl as the WASM exports the host
//! expects.
//!
//! Put one macro call at the end of the runtime crate's `lib.rs`:
//!
//! ```text
//! pub struct MyGame;
//! impl mijnspel_sdk::guest::GuestRuntime for MyGame { /* … */ }
//! mijnspel_sdk::export_runtime!(MyGame, process_signal, state_for_viewer);
//! ```
//!
//! and one in the migration crate's `lib.rs`:
//!
//! ```text
//! mijnspel_sdk::export_migration!(mijnspel_sdk::guest::IdentityMigration<1>);
//! ```
//!
//! The macros expand in the calling crate, so the `#[no_mangle] extern "C"`
//! functions land in the leaf `cdylib` where the linker emits them as WASM
//! exports. They are only emitted for `target_arch = "wasm32"`, so the same
//! crate still builds and runs `cargo test` natively.

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use crate::abi::{pack_ptr_len, HostJson, SignalOutcome, ValidateOutcome};

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
    /// games that have not opted in just return "signaling unsupported".
    /// The host only calls it when the module exports `process_signal`,
    /// which `export_runtime!(T, process_signal)` emits.
    ///
    /// `input` shape: `{ appId, gameId, state, signalType, callerSub,
    /// payload, row, nowMs }`.
    fn process_signal(_input: Value) -> SignalOutcome {
        SignalOutcome::Reject {
            message: "signaling unsupported".to_string(),
        }
    }

    /// Optional. The state one user may see. The host only calls it when the
    /// module exports `state_for_viewer`, which
    /// `export_runtime!(T, state_for_viewer)` emits; it then sends every
    /// outgoing game row with `state` replaced by this result.
    ///
    /// `input` shape: `{ appId, gameId, viewerSub, state, row }`, where
    /// `state` is the stored `row.state` (the same shape as `priorState`).
    /// A viewer who is not a member should get the public view. The default
    /// returns `input.state` unchanged.
    fn state_for_viewer(input: Value) -> Value {
        input.get("state").cloned().unwrap_or(Value::Null)
    }
}

/// Trait the per-game migration crate implements. One pairwise stepper per
/// direction; the host chains them.
pub trait GuestMigration {
    fn current_schema_version() -> i64;
    fn min_supported_version() -> i64;

    /// Walk a state from `version - 1` to `version`. The host sends
    /// `{ "version": i64, "state": Value }`; `export_migration!` passes the
    /// two fields in and returns the migrated state.
    fn upgrade_from_previous(version: i64, state: Value) -> Value;

    /// Walk a state from `version` to `version - 1`.
    fn downgrade_to_previous(version: i64, state: Value) -> Value;
}

/// Migration for a game whose stored shape never changed: schema version `V`
/// is both current and the minimum, and both steppers return the state
/// unchanged.
pub struct IdentityMigration<const V: i64>;

impl<const V: i64> GuestMigration for IdentityMigration<V> {
    fn current_schema_version() -> i64 {
        V
    }

    fn min_supported_version() -> i64 {
        V
    }

    fn upgrade_from_previous(_version: i64, state: Value) -> Value {
        state
    }

    fn downgrade_to_previous(_version: i64, state: Value) -> Value {
        state
    }
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

/// `__alloc` body: hand the host a buffer of `len` bytes in guest memory.
#[doc(hidden)]
pub fn alloc(len: u32) -> u32 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr() as u32;
    core::mem::forget(buf);
    ptr
}

/// `__free` body: release a buffer handed out by [`alloc`] or returned by an
/// export.
///
/// # Safety
/// `ptr`/`len` must describe a buffer this module allocated.
#[doc(hidden)]
pub unsafe fn free(ptr: u32, len: u32) {
    let _ = Vec::from_raw_parts(ptr as *mut u8, 0, len as usize);
}

fn write_output<O: Serialize>(out: O) -> u64 {
    let buf = encode_host_json(out);
    let len = buf.len() as u32;
    let leaked = Box::leak(buf.into_boxed_slice());
    pack_ptr_len(leaked.as_ptr() as u32, len)
}

/// Body of a JSON export: decode the input envelope at `ptr`/`len`, run `f`,
/// and return the packed output buffer.
///
/// # Safety
/// `ptr`/`len` must describe the input buffer the host wrote.
#[doc(hidden)]
pub unsafe fn call_json<O: Serialize>(ptr: u32, len: u32, f: impl FnOnce(Value) -> O) -> u64 {
    let bytes = core::slice::from_raw_parts(ptr as *const u8, len as usize);
    write_output(f(decode_host_json::<Value>(bytes)))
}

/// Body of a JSON export whose trait method takes no input. The host still
/// sends a `null` envelope, which is ignored.
#[doc(hidden)]
pub fn call_no_input<O: Serialize>(f: impl FnOnce() -> O) -> u64 {
    write_output(f())
}

/// Body of a migration stepper: split the host's `{ version, state }` input
/// into the two trait arguments.
///
/// # Safety
/// `ptr`/`len` must describe the input buffer the host wrote.
#[doc(hidden)]
pub unsafe fn call_migration(ptr: u32, len: u32, f: impl FnOnce(i64, Value) -> Value) -> u64 {
    call_json(ptr, len, |input| {
        let version = input.get("version").and_then(Value::as_i64).unwrap_or(0);
        let state = input.get("state").cloned().unwrap_or(Value::Null);
        f(version, state)
    })
}

/// Emit every runtime export for a [`GuestRuntime`] impl: `__alloc`, `__free`,
/// `abi_version`, `current_schema_version`, and the ten required JSON
/// exports. Name optional hooks after the type to export them too:
/// `process_signal` and `state_for_viewer`.
///
/// ```text
/// mijnspel_sdk::export_runtime!(MyGame);
/// mijnspel_sdk::export_runtime!(MyGame, state_for_viewer);
/// mijnspel_sdk::export_runtime!(MyGame, process_signal, state_for_viewer);
/// ```
#[macro_export]
macro_rules! export_runtime {
    ($t:ty $(, $hook:ident)* $(,)?) => {
        $crate::__export_memory!();

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn abi_version() -> i64 {
            $crate::abi::HOST_ABI_VERSION
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn current_schema_version() -> i64 {
            <$t as $crate::guest::GuestRuntime>::current_schema_version()
        }

        $crate::__export_json!($t, validate_create_request);
        $crate::__export_json!($t, validate_update_request);
        $crate::__export_json!($t, validate_lobby_create);
        $crate::__export_no_input!($t, lobby_policy);
        $crate::__export_json!($t, create_initial_state);
        $crate::__export_json!($t, state_for_lobby_start);
        $crate::__export_no_input!($t, prior_stat_ids);
        $crate::__export_json!($t, process_update);
        $crate::__export_no_input!($t, leaderboard_stat_ids);
        $crate::__export_json!($t, players_in_row);
        $( $crate::__export_runtime_hook!($t, $hook); )*
    };
}

/// Emit every migration export for a [`GuestMigration`] impl: `__alloc`,
/// `__free`, `abi_version`, `current_schema_version`,
/// `min_supported_version`, `upgrade_from_previous` and
/// `downgrade_to_previous`.
///
/// ```text
/// mijnspel_sdk::export_migration!(mijnspel_sdk::guest::IdentityMigration<1>);
/// ```
#[macro_export]
macro_rules! export_migration {
    ($t:ty $(,)?) => {
        $crate::__export_memory!();

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn abi_version() -> i64 {
            $crate::abi::HOST_ABI_VERSION
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn current_schema_version() -> i64 {
            <$t as $crate::guest::GuestMigration>::current_schema_version()
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn min_supported_version() -> i64 {
            <$t as $crate::guest::GuestMigration>::min_supported_version()
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn upgrade_from_previous(ptr: u32, len: u32) -> u64 {
            unsafe {
                $crate::guest::call_migration(
                    ptr,
                    len,
                    <$t as $crate::guest::GuestMigration>::upgrade_from_previous,
                )
            }
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn downgrade_to_previous(ptr: u32, len: u32) -> u64 {
            unsafe {
                $crate::guest::call_migration(
                    ptr,
                    len,
                    <$t as $crate::guest::GuestMigration>::downgrade_to_previous,
                )
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_memory {
    () => {
        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn __alloc(len: u32) -> u32 {
            $crate::guest::alloc(len)
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn __free(ptr: u32, len: u32) {
            unsafe { $crate::guest::free(ptr, len) }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_json {
    ($t:ty, $name:ident) => {
        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn $name(ptr: u32, len: u32) -> u64 {
            unsafe {
                $crate::guest::call_json(ptr, len, <$t as $crate::guest::GuestRuntime>::$name)
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_no_input {
    ($t:ty, $name:ident) => {
        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn $name(_ptr: u32, _len: u32) -> u64 {
            $crate::guest::call_no_input(<$t as $crate::guest::GuestRuntime>::$name)
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_runtime_hook {
    ($t:ty, process_signal) => {
        $crate::__export_json!($t, process_signal);
    };
    ($t:ty, state_for_viewer) => {
        $crate::__export_json!($t, state_for_viewer);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_migration_keeps_state_and_version() {
        type M = IdentityMigration<3>;
        assert_eq!(M::current_schema_version(), 3);
        assert_eq!(M::min_supported_version(), 3);
        let state = json!({ "board": [1, 2], "state": "kept" });
        assert_eq!(M::upgrade_from_previous(3, state.clone()), state);
        assert_eq!(M::downgrade_to_previous(3, state.clone()), state);
    }
}
