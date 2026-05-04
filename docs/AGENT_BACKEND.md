# mijnspel-backend reference for AI agents

You are an AI agent inside `mijnspel-creator`. You have generated a
frontend for a game and now need to wire it up to a `mijnspel-backend`
API. This document is the condensed reference you need. Everything in
it is load-bearing — read it end to end.

## What you are connecting to

The `mijnspel-backend` host is a game-agnostic AWS-style backend that
is also fully runnable as a single Docker container locally. It stores
opaque per-game state and dispatches per-game logic to per-game
**WebAssembly modules** loaded at runtime.

Two integration points exist:

1. **REST + WebSocket API** — what your generated frontend talks to.
   Same routes whether the host is running in production or as the
   local Docker container.
2. **Per-game WASM** — `runtime.wasm` (request-path logic) and
   `migration.wasm` (schema steppers). You author these in Rust against
   the public `mijnspel-sdk` crate, build to
   `wasm32-unknown-unknown`, and drop them into the local container's
   mounted directory for testing. Production registration is manual
   and out-of-band.

## Hard rules (do not violate)

1. **Every REST and WebSocket request URL must include
   `?appId=<your-app-id>`.** Missing → `400 missing app id`.
   Unregistered → `400 unregistered app id`. The appId is the tenancy
   key for the entire system.
2. **No host imports inside WASM.** Your WASM cannot call DDB, S3,
   `fetch`, the OS clock, or anything else outside its sandbox. The
   host injects whatever you need (claims, time if relevant) into the
   JSON input.
3. **State is opaque to the host.** Anything in your `state` JSON is
   yours; the host stores it verbatim and never reads it. Don't expect
   the host to enforce game rules — that's what your runtime WASM is
   for.
4. **`appId` lives only in the partition key.** Never duplicate it as a
   non-key attribute on any row.
5. **WS topics are namespaced `<appId>:<scope>:<id>`.** Other shapes
   are rejected.
6. **You cannot push to production.** Drop WASMs in
   `local-wasm/<appId>/` for the local container; the operator runs
   the manual intake protocol for production.

## Connecting your generated frontend

Make the local backend reachable on a known URL — the
`mijnspel-creator-server` that orchestrates you will allocate a host
port and pass it as `BACKEND_URL` into your build context.

In your frontend's HTTP client, configure:

```
BASE_URL = process.env.BACKEND_URL  // e.g. http://localhost:3010
APP_ID   = '<your-app-id>'          // matches local-wasm/<appId>/

function url(path)            { return `${BASE_URL}${path}?appId=${APP_ID}`; }
function urlWithExtra(path, q) { return `${url(path)}&${q}`; }
```

Every request URL — REST and WS — gets `?appId=<your-app-id>`
appended.

For WebSocket, derive the WS URL from the same base:

```
WS_URL = BACKEND_URL.replace(/^http/, 'ws') + '/ws?appId=' + APP_ID
```

The container's `AUTH_ALLOWED_ORIGINS` env var is set by the
orchestrator to include your frontend's origin. If you see CORS errors
in the browser console, that's the variable to fix at the
orchestrator level.

## REST surface (highlights)

All routes accept `?appId=<id>`. Many require an `Authorization:
Bearer <id_token>` header from a Cognito sign-in flow.

| Route | Purpose |
|---|---|
| `POST /auth/signup` | Sign up with username/password (auto-confirmed locally). |
| `POST /auth/login` | Returns `{ idToken, accessToken, refreshToken }`. |
| `POST /auth/guest` | Issue a short-lived guest ID token. |
| `POST /auth/refresh` | Refresh an expired ID token. |
| `POST /games` | Create a game. Body `{ state: { ... } }`; runs your runtime's `validate_create_request` and `create_initial_state`. |
| `GET /games/{id}` | Read a game. Triggers on-read coercion if the row's `state.schemaVersion` differs from the registry. |
| `PUT /games/{id}` | Update. Runs your runtime's `validate_update_request` and `process_update`. |
| `POST /games/{id}/abandon` | Abandon. |
| `POST /games/{id}/reset` | Reset to a fresh initial state. |
| `POST /games/{id}/signal` | Per-session signal. **404** if your runtime does not export `process_signal`. |
| `POST /lobbies` | Create a lobby. Runs `validate_lobby_create` and stamps `lobby_policy()` on the row. |
| `POST /lobbies/{id}/join` | Join. May trigger autostart if `auto_start_when_full = true`. |
| `POST /lobbies/by-hash/{hash}/join` | Resolve a lobby by short hash and join. |
| `DELETE /lobbies/{id}` | Cancel (owner only). |
| `POST /lobbies/{id}/leave` | Leave (non-owner). |
| `GET /lobbies?member=me` | List lobbies you're a member of. |
| `GET /lobbies?owner=me` | List lobbies you own. |
| `GET /stats?...` | Read player-stats values. |
| `GET /leaderboard/{statId}` | Read leaderboard for a stat. |
| `POST /push/subscribe` | Register a WebPush subscription. |
| `POST /push/unsubscribe` | Unregister. |

## WebSocket surface

Open `WS /ws?appId=<your-app-id>` with a Bearer ID token. The host:

- Validates the JWT.
- Seals `appId` into the connection row's PK at upgrade time.
- Accepts subscribe / unsubscribe frames over the upgraded socket.

Topics:

- `<appId>:game:<gameId>` — game-specific events.
- `<appId>:lobby:<lobbyId>` — lobby-specific events.
- `<appId>:user:<sub>` — direct events to one user.
- `<appId>:leaderboard:<statId>` — leaderboard refreshes.

A subscribe to a topic whose first segment is *not* your `appId` is
rejected as a partition miss. You can only subscribe to your own
appId's topics.

## Authoring `runtime.wasm`

Minimal Rust shape — see
[`docs/sdk/quickstart.md`](./sdk/quickstart.md) for the full layout
and [`docs/sdk/abi-reference.md`](./sdk/abi-reference.md) for the
JSON shapes and export-glue boilerplate.

```rust
use mijnspel_sdk::abi::{
    HOST_ABI_VERSION, ProcessUpdateOutput, StatUpdate, ValidateOutcome,
    LobbyPolicy, PriorStatIds, PlayersInRow, LeaderboardStatIds,
};

pub fn abi_version() -> i64 { HOST_ABI_VERSION }
pub fn current_schema_version() -> i64 { 1 }

pub fn validate_create_request(_in: serde_json::Value) -> ValidateOutcome {
    ValidateOutcome::ok()
}

pub fn validate_update_request(input: serde_json::Value) -> ValidateOutcome {
    // pure check on `input["current"]` and `input["request"]`
    ValidateOutcome::ok()
}

pub fn create_initial_state(input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": current_schema_version(),
        // ... your initial in-state shape ...
    })
}

pub fn process_update(input: serde_json::Value) -> ProcessUpdateOutput {
    // input has: current, request, priors, claims
    let next_state = compute_next(&input);
    let stat_updates = vec![]; // optional StatUpdates
    ProcessUpdateOutput { next_state, stat_updates }
}

pub fn lobby_policy() -> LobbyPolicy {
    LobbyPolicy { min_members: 1, max_members: 1, auto_start_when_full: false, allow_guest: false }
}

pub fn prior_stat_ids() -> PriorStatIds { PriorStatIds { stat_ids: vec![] } }
pub fn players_in_row(_row: serde_json::Value) -> PlayersInRow { PlayersInRow { player_ids: vec![] } }
pub fn leaderboard_stat_ids() -> LeaderboardStatIds { LeaderboardStatIds { stat_ids: vec![] } }
```

Each function above is wrapped by a thin `#[no_mangle] extern "C"`
shim that handles allocation and JSON encode/decode. Copy the
boilerplate from `mijnspel_sdk::guest::reference` — it must
live in your leaf crate, not the SDK.

## Authoring `migration.wasm`

Pairwise steppers. The host calls them one version at a time:

```rust
pub fn abi_version() -> i64 { HOST_ABI_VERSION }
pub fn current_schema_version() -> i64 { 1 }
pub fn min_supported_version() -> i64 { 1 }

pub fn upgrade_from_previous(target: i64, state: serde_json::Value)
    -> serde_json::Value
{
    match target {
        // 2 => v1_to_v2(state),
        _ => panic!("no upgrade defined for {target}"),
    }
}

pub fn downgrade_to_previous(target: i64, state: serde_json::Value)
    -> serde_json::Value
{
    match target {
        // 1 => v2_to_v1(state),
        _ => panic!("no downgrade defined for {target}"),
    }
}
```

Make every step total over the inputs you accept. A panic surfaces as
`502` from the host on read, or routes the row to a DLQ during a bulk
migration.

## Build and drop in

From your `backend/` directory:

```
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path runtime/Cargo.toml
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path migration/Cargo.toml

mkdir -p ../local-wasm/<your-app-id>
cp target/wasm32-unknown-unknown/release/<crate>_runtime.wasm \
   ../local-wasm/<your-app-id>/runtime.wasm
cp target/wasm32-unknown-unknown/release/<crate>_migration.wasm \
   ../local-wasm/<your-app-id>/migration.wasm
```

The orchestrator mounts `local-wasm/` into the container at
`/app/local-wasm/`. After rebuilding, send `SIGHUP` to the container
or restart it — the bootstrap re-scans, sha256s, uploads, and updates
the in-container `app-registry`.

## Testing checklist

Run these against the local container before declaring "done":

1. **Auth round-trip.** `POST /auth/signup` → `POST /auth/login` →
   call an authed route with the returned `idToken`.
2. **Opaque round-trip.** `POST /games` with a known `state`, `GET
   /games/{id}`, assert `state` is byte-identical (or the coerced
   shape if you stamped a `schemaVersion`).
3. **Update round-trip.** `PUT /games/{id}` with a request your
   runtime accepts, assert the response carries your computed
   `next_state`.
4. **Validation rejection.** `PUT /games/{id}` with a request your
   runtime rejects, assert `400` with the message from your
   `ValidateOutcome::Reject`.
5. **WebSocket subscribe.** Open WS, subscribe to
   `<appId>:game:<gameId>`, perform an update from a second client,
   assert the WS receives the event.
6. **Lobby autostart** (only if you exposed lobbies). Create a lobby
   with `auto_start_when_full = true`, fill it, assert the lobby
   transitions to `started` and a game row is created.
7. **Migration** (only if you bumped `current_schema_version`).
   Insert an older-version state directly into MiniStack DDB, read
   via the lambda, assert the response is at the current version.

## Known pitfalls

- **You forgot `?appId=`.** `400 missing app id`.
- **You signed in to a different appId pool.** Cognito user pools are
  shared across appIds in the local container, but JWT `aud`/`sub`
  binding is to whatever appId the request uses.
- **Two services both listen on `:3000`.** The orchestrator allocates
  a different host port via `LOCAL_BACKEND_PORT`. Use `BACKEND_URL`
  exactly as the orchestrator passed it.
- **You returned a non-camelCase JSON shape.** Every SDK type uses
  `serde(rename_all = "camelCase")`. Your bare `next_state` field on
  `ProcessUpdateOutput` is `nextState` on the wire.
- **You panicked in `upgrade_from_previous` for an in-flight version
  step.** The on-read coercion will surface `502` for affected rows.
  Add the missing match arm.
- **You expected the host to validate state contents.** It won't —
  enforce in `validate_*` and `process_update`.
- **You expected the host to compute outcomes** (winner, ratings,
  achievements). Compute them in `process_update` and emit
  `StatUpdate`s.

## Where to look when you need more detail

You don't have access to the host source. Use these instead:

- **`cargo doc --open -p mijnspel-sdk`** — the published SDK
  ships full Rust docs for every wire type (`ProcessUpdateOutput`,
  `StatUpdate`, `LobbyPolicy`, `SignalOutcome`, `ValidateOutcome`,
  …). Authoritative on JSON shapes; what's documented there is the
  contract.
- **The `mijnspel_sdk::guest::reference` module** — the
  canonical export-glue you copy into your `runtime`/`migration`
  crates (alloc/free, ptr/len packing, JSON encode/decode,
  `#[no_mangle] extern "C"` wrappers).
- **The local container's logs** (`docker logs <container-id>`) —
  every WASM trap, ABI mismatch, and JSON decode error surfaces here
  with the export name and the failure cause.
- **The local container's MiniStack** — for inspecting raw rows.
  Bind `-p 4566:4566` and use `awslocal` (or `aws --endpoint-url=http://localhost:4566`)
  to query DDB tables directly. Useful when you want to confirm how
  your WASM persisted a row.
- **The rest of these docs** — `docs/sdk/` covers each export
  in depth, schema versioning, lobbies + signals, and submission.

## What you may *not* do as the agent

- Allocate a real AWS account or call any deployed `mijnspel-backend`
  endpoint directly.
- Try to upload WASMs anywhere except the local container's mounted
  `local-wasm/` directory.
- Try to flip the production app-registry, run admin migrations
  against production, or otherwise reach beyond the local container.
- Modify the SDK crate, the Docker image, or anything in the host
  itself to make your generated game work. The SDK and image are
  fixed inputs; if your game needs them to change, your WASM or your
  frontend is solving the wrong problem.

The system is built so that you can ship a working game by doing only
one thing: writing the two WASMs and your frontend that hits the
fixed REST + WS surface.
