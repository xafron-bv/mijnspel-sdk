//! WASM ABI types crossed via JSON between the host and the guest.
//!
//! The boundary is intentionally JSON — the WASM module exports a small set of
//! `i32 -> i32` functions that take a `(ptr, len)` pair pointing at a UTF-8
//! JSON document in linear memory and return a `(ptr, len)` pair encoded as
//! `((ptr as u64) << 32) | len as u64` packed into a single `u64` return.
//!
//! Allocation is owned by the guest. The host calls `__alloc(len)` to request
//! a buffer in guest memory, writes the JSON input there, then calls the
//! export. The export returns the `(ptr, len)` of the JSON output buffer.
//! The host reads the output and calls `__free(ptr, len)` afterwards.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// ABI version advertised by the host. Bumped when the JSON shape, the export
/// list, or the memory protocol changes in a backwards-incompatible way.
pub const HOST_ABI_VERSION: i64 = 1;

/// Names of the runtime exports the host calls. Kept in one place so both the
/// guest macro and the host loader stay in sync.
pub mod runtime_exports {
    pub const ABI_VERSION: &str = "abi_version";
    pub const CURRENT_SCHEMA_VERSION: &str = "current_schema_version";
    pub const VALIDATE_CREATE_REQUEST: &str = "validate_create_request";
    pub const VALIDATE_UPDATE_REQUEST: &str = "validate_update_request";
    pub const VALIDATE_LOBBY_CREATE: &str = "validate_lobby_create";
    pub const LOBBY_POLICY: &str = "lobby_policy";
    pub const CREATE_INITIAL_STATE: &str = "create_initial_state";
    pub const STATE_FOR_LOBBY_START: &str = "state_for_lobby_start";
    /// Stat ids whose previous values must be loaded by the host and
    /// passed to `process_update` as `priors`. The mijnspel host has no
    /// opinion on what those stats mean (Elo, MMR, accuracy, etc.) — it
    /// just shuttles the recorded values per (player, stat).
    pub const PRIOR_STAT_IDS: &str = "prior_stat_ids";
    pub const PROCESS_UPDATE: &str = "process_update";
    pub const LEADERBOARD_STAT_IDS: &str = "leaderboard_stat_ids";
    /// Extracts the list of player ids that appear in a game row
    /// (e.g. gunzi's `blackSub` / `whiteSub`). The host calls this before
    /// `process_update` so it can fetch each player's
    /// `prior_stat_ids()` stat values and pass them in as priors.
    pub const PLAYERS_IN_ROW: &str = "players_in_row";
    /// Optional. Per-game session signaling. The host calls this from
    /// `POST /games/{id}/signal`; if a guest does not export it the
    /// route returns 404 (signaling unsupported for this appId).
    pub const PROCESS_SIGNAL: &str = "process_signal";
    /// Optional. Per-viewer redaction. When a guest exports it, the host
    /// sends every outgoing game row with `state` replaced by this export's
    /// result for the receiving user; without it rows go out unchanged.
    pub const STATE_FOR_VIEWER: &str = "state_for_viewer";
}

/// Names of the migration exports.
pub mod migration_exports {
    pub const ABI_VERSION: &str = "abi_version";
    pub const CURRENT_SCHEMA_VERSION: &str = "current_schema_version";
    pub const MIN_SUPPORTED_VERSION: &str = "min_supported_version";
    pub const UPGRADE_FROM_PREVIOUS: &str = "upgrade_from_previous";
    pub const DOWNGRADE_TO_PREVIOUS: &str = "downgrade_to_previous";
}

/// Memory-protocol export names.
pub mod memory_exports {
    pub const ALLOC: &str = "__alloc";
    pub const FREE: &str = "__free";
    pub const MEMORY: &str = "memory";
}

/// JSON payload wrapper used by every JSON-typed export. Keeps a single shape
/// across the boundary so the guest macro can encode/decode without
/// per-export ceremony.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostJson {
    /// Free-form JSON payload. Each export's input/output schema is documented
    /// at the trait level; the JSON shape is what each handler agrees on.
    #[serde(default)]
    pub data: Value,
}

impl HostJson {
    pub fn new(data: Value) -> Self {
        Self { data }
    }
}

/// `validate_*` outcome. `Ok` means accepted; `Err` is a user-visible message
/// that the host turns into a 400. Validation does not produce next-state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValidateOutcome {
    Ok,
    #[serde(rename_all = "camelCase")]
    Reject {
        message: String,
    },
}

impl Default for ValidateOutcome {
    fn default() -> Self {
        ValidateOutcome::Ok
    }
}

impl ValidateOutcome {
    pub fn ok() -> Self {
        ValidateOutcome::Ok
    }
    pub fn reject(message: impl Into<String>) -> Self {
        ValidateOutcome::Reject {
            message: message.into(),
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, ValidateOutcome::Ok)
    }
}

/// One stat write the WASM emits as a result of `process_update`. The host
/// applies these to the generic `player-stats` store; the host does not know
/// the meaning of any specific stat id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatUpdate {
    pub player_id: String,
    pub stat_id: String,
    pub value: f64,
    /// One of `set`, `add`, `max`. The host maps these to the `UpdateMode`
    /// variants exposed by the stats store.
    pub mode: String,
}

/// Result of a single `process_update` call. The next state is what the host
/// persists; stat updates are forwarded to the generic stats store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUpdateOutput {
    #[serde(default)]
    pub next_state: Value,
    #[serde(default)]
    pub stat_updates: Vec<StatUpdate>,
}

/// Stat ids the WASM declares as priors: stored values the host should
/// fetch for each player and pass into the next `process_update` call as
/// `priors[playerId][statId]`. The semantics of those stats are entirely
/// game-side (rating, accuracy, streak, anything) — the host just routes
/// the values.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PriorStatIds {
    #[serde(default)]
    pub stat_ids: Vec<String>,
}

/// Result of `players_in_row`. A flat list of player ids the WASM
/// extracted from a game row's per-game fields. The host uses this to know
/// whose `prior_stat_ids()` stats to fetch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlayersInRow {
    #[serde(default)]
    pub player_ids: Vec<String>,
}

/// Lobby policy declared by the WASM on lobby creation. The host stamps
/// these onto the lobby row at creation and enforces the resulting
/// state machine generically — the host does not interpret per-game
/// `payload`. `autoStartWhenFull` triggers the host to atomically create
/// a game (via `state_for_lobby_start`) when a join brings the member
/// count up to `maxMembers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LobbyPolicy {
    pub min_members: i64,
    pub max_members: i64,
    pub auto_start_when_full: bool,
    pub allow_guest: bool,
}

impl Default for LobbyPolicy {
    fn default() -> Self {
        Self {
            min_members: 1,
            max_members: 32,
            auto_start_when_full: false,
            allow_guest: false,
        }
    }
}

/// Stat ids the WASM declares as leaderboard-relevant. The DDB-stream
/// consumer fans events out only for these ids.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardStatIds {
    #[serde(default)]
    pub stat_ids: Vec<String>,
}

/// Result of `process_signal`. Either an accepted signal — host writes
/// `new_state` back to the game row, dispatches the WS event to each
/// `targets` sub, and (if `webpush`) emits a WebPush to those subs with
/// `push_title` / `push_body` — or a reject the host turns into a 403.
///
/// `new_state` may be the unchanged input state if the WASM has nothing
/// to record (e.g. a pure broadcast signal). Cooldown bookkeeping
/// typically lives inside the per-game state, written here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SignalOutcome {
    #[serde(rename_all = "camelCase")]
    Ok {
        new_state: Value,
        #[serde(default)]
        targets: Vec<String>,
        #[serde(default)]
        webpush: bool,
        #[serde(default)]
        push_title: String,
        #[serde(default)]
        push_body: String,
    },
    #[serde(rename_all = "camelCase")]
    Reject {
        message: String,
    },
}

impl Default for SignalOutcome {
    fn default() -> Self {
        SignalOutcome::Reject {
            message: "signaling unsupported".to_string(),
        }
    }
}

/// Reference to a state envelope handed across the WASM boundary. The state
/// itself is opaque JSON to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateEnvelopeRef {
    pub schema_version: i64,
    pub state: Value,
}

/// Pack `(ptr, len)` into the `u64` shape returned by guest exports. The
/// upper 32 bits are the pointer; the lower 32 bits are the length.
#[inline]
pub fn pack_ptr_len(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | (len as u64)
}

#[inline]
pub fn unpack_ptr_len(packed: u64) -> (u32, u32) {
    let ptr = (packed >> 32) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;
    (ptr, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let cases = [(0, 0), (1, 1), (0xDEADBEEF, 0xCAFEBABE), (u32::MAX, 0)];
        for (ptr, len) in cases {
            let (p, l) = unpack_ptr_len(pack_ptr_len(ptr, len));
            assert_eq!((p, l), (ptr, len));
        }
    }

    #[test]
    fn validate_outcome_is_ok() {
        assert!(ValidateOutcome::ok().is_ok());
        assert!(!ValidateOutcome::reject("nope").is_ok());
    }
}
