---
title: ABI reference
sidebar_position: 3
---

# ABI reference

Every host↔guest crossing is a JSON document. Each export is an
`extern "C" fn(ptr: u32, len: u32) -> u64` taking a pointer/length to
a UTF-8 JSON document in guest memory and returning a packed
`(ptr, len)` u64 pointing at a JSON output buffer in guest memory.

## Memory protocol

The guest owns its memory. The host calls into the guest's allocator
to ask for buffers.

| Export | Signature | Meaning |
|---|---|---|
| `__alloc` | `fn(len: u32) -> u32` | Allocate `len` bytes; return the pointer. |
| `__free` | `fn(ptr: u32, len: u32)` | Release a buffer the host had received. |
| `memory` | exported memory | The single linear memory the host reads/writes. |

Sequence per call:

1. Host asks `__alloc(input_len)`.
2. Host writes the input JSON into guest memory at the returned pointer.
3. Host calls the export with `(ptr, len)`.
4. The export returns a packed `(out_ptr, out_len)` u64.
5. Host reads `out_len` bytes from `out_ptr`, decodes JSON.
6. Host calls `__free(out_ptr, out_len)`.

Packing helpers ship in the SDK:

```rust
mijnspel_sdk::abi::pack_ptr_len(ptr: u32, len: u32) -> u64
mijnspel_sdk::abi::unpack_ptr_len(packed: u64) -> (u32, u32)
```

## Versioning exports

Both the runtime and the migration WASM expose:

| Export | Returns | Used for |
|---|---|---|
| `abi_version` | `i64` | Must equal `HOST_ABI_VERSION`. Mismatch → 502 from the host. |
| `current_schema_version` | `i64` | The version *this* binary considers current. The intake script asserts the runtime's value equals the migration's value. |

The migration also exposes `min_supported_version` so the host knows
how far down a downgrade chain it can walk.

## Runtime exports

Names are constants on `mijnspel_sdk::abi::runtime_exports`:

| Export | Input | Output |
|---|---|---|
| `validate_create_request` | `{ appId, gameId, state }` | `ValidateOutcome` |
| `validate_update_request` | `{ appId, gameId, priorState, state }` | `ValidateOutcome` |
| `validate_lobby_create` | `{ appId, lobbyId, ownerSub, members, payload }` | `ValidateOutcome` |
| `lobby_policy` | `{}` | `LobbyPolicy` |
| `create_initial_state` | `{ appId, gameId, state }` | inner state JSON |
| `state_for_lobby_start` | `{ appId, gameId, lobbyId, ownerSub, members, payload }` | inner state JSON (plus optional row-extension fields the host lifts) |
| `prior_stat_ids` | `{}` | `PriorStatIds` |
| `players_in_row` | `{ row }` | `PlayersInRow` |
| `process_update` | `{ appId, gameId, priorState, state, row, priors }` | `ProcessUpdateOutput` |
| `leaderboard_stat_ids` | `{}` | `LeaderboardStatIds` |
| `process_signal` *(optional)* | `{ appId, gameId, state, signalType, callerSub, payload, row, nowMs }` | `SignalOutcome` |

When the host sees no `process_signal` export, the corresponding
`POST /games/{id}/signal` route returns 404 for that appId.

## Migration exports

| Export | Input | Output |
|---|---|---|
| `upgrade_from_previous` | `{ version, state }` | new state at `version` |
| `downgrade_to_previous` | `{ version, state }` | new state at `version - 1` |

The host chains pairwise calls to walk any source version to any
target. Steppers must therefore be reversible if you want to support
rollbacks.

## JSON shapes

The generated host payload schema is published at
`docs/api/wasm-abi.schema.json`. Rust result shapes live in
`mijnspel-sdk/src/abi.rs` and are re-exported at the crate root. The
most important ones:

```rust
pub struct ValidateOutcome { Ok | Reject { message: String } }

pub struct LobbyPolicy {
    pub min_members: i64,
    pub max_members: i64,
    pub auto_start_when_full: bool,
    pub allow_guest: bool,
}

pub struct PriorStatIds {
    pub stat_ids: Vec<String>,
}

pub struct PlayersInRow {
    pub player_ids: Vec<String>,
}

pub struct StatUpdate {
    pub player_id: String,
    pub stat_id: String,
    pub value: f64,
    pub mode: String, // "set" | "add" | "max"
}

pub struct ProcessUpdateOutput {
    pub next_state: serde_json::Value,
    pub stat_updates: Vec<StatUpdate>,
}

pub struct LeaderboardStatIds {
    pub stat_ids: Vec<String>,
}

pub enum SignalOutcome {
    Ok {
        new_state: serde_json::Value,
        targets: Vec<String>,
        webpush: bool,
        push_title: String,
        push_body: String,
    },
    Reject { message: String },
}
```

All structs use `serde(rename_all = "camelCase")`, so `min_members`
crosses the boundary as `minMembers`.

## Boilerplate (the guest-side macro)

`mijnspel_sdk::guest::reference` is a documented reference
implementation of the export glue (allocation, packing, JSON
encode/decode, `#[no_mangle] extern "C"` wrappers). Copy it into your
runtime/migration crates — it lives there rather than as a `macro_rules!`
in the SDK because the `#[no_mangle]` blocks must reside in the leaf
`cdylib` crate to be picked up by the linker.

## What the host *does not* expose

There are no host imports of any kind. The WASM module cannot:

- Read or write DynamoDB.
- Make network requests.
- Read environment variables.
- Read system time directly (the host injects time into the request
  payload where needed).
- Generate randomness from the OS (the host injects nonces / seeds
  where needed).

If your game needs any of the above, the host computes it before
calling and includes it in the JSON input.
