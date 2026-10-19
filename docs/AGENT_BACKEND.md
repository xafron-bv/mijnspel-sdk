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
   host injects whatever you need (`callerSub`, a random `seed`, time if
   relevant) into the JSON input.
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
orchestrator to include your frontend's origin. A REST request from any
other origin, including `Origin: null` from a sandboxed iframe, gets
`403` before the handler runs. If you see CORS errors or `403` in the
browser console, that's the variable to fix at the orchestrator level.

On deployed stages, published games served from `https://*.mijnspel.com`
are CORS-allowed by default (in addition to the official frontend). The
scheme is pinned, so only `https://<slug>.mijnspel.com` is allowed.

## REST surface (highlights)

All routes accept `?appId=<id>`. Many require an `Authorization:
Bearer <id_token>` header from a Cognito sign-in flow.

| Route | Purpose |
|---|---|
| `POST /auth/signup` | Sign up with username/password (auto-confirmed locally). |
| `POST /auth/login` | Returns `{ idToken, accessToken, refreshToken }`. |
| `POST /auth/guest` | Issue a short-lived guest ID token. |
| `POST /auth/refresh` | Refresh an expired ID token. |
| `POST /games` | Create a game. Body `{ gameId?, state: { ... } }`; runs your runtime's `validate_create_request` and `create_initial_state`. Re-using the id of an existing game is allowed only for its members (or its creator), else `409 Game already exists`. |
| `GET /games/{id}` | Read a game. Triggers on-read coercion if the row's `state.schemaVersion` differs from the registry. |
| `PUT /games/{id}` | Update. Runs your runtime's `validate_update_request` and `process_update`. Optional body field `expectedUpdatedAt` (the row's last seen `updatedAt`) → `409` if the game changed since. |
| `POST /games/{id}/abandon` | Abandon. Members (or the creator) only, else `403`. |
| `POST /games/{id}/reset` | Reset to a fresh initial state. Members (or the creator) only, else `403`. |
| `POST /games/{id}/signal` | Per-session signal. **404** if your runtime does not export `process_signal`. |
| `POST /lobbies` | Create a lobby. Runs `validate_lobby_create` and stamps `lobby_policy()` on the row. |
| `POST /lobbies/{id}/join` | Join. May trigger autostart if `auto_start_when_full = true`. |
| `POST /lobbies/by-hash/join` | Resolve a lobby by short hash from body `{ hashKey }` or `{ hash }` and join. |
| `DELETE /lobbies/{id}` | Cancel (owner only). |
| `POST /lobbies/{id}/leave` | Leave (non-owner). |
| `GET /lobbies` | List lobbies you're a member of. |
| `GET /lobbies/owned` | List lobbies you own. |
| `GET /users/{id}/stats` | Read player-stats values. |
| `GET /leaderboard?stat=...` | Read leaderboard for a stat. |
| `POST /push-subscriptions` | Register a WebPush subscription. |
| `DELETE /push-subscriptions` | Unregister. |

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
JSON shapes.

```rust
use mijnspel_sdk::abi::{
    LeaderboardStatIds, LobbyPolicy, PlayersInRow, PriorStatIds, ProcessUpdateOutput,
    ValidateOutcome,
};
use mijnspel_sdk::guest::GuestRuntime;
use serde_json::Value;

pub struct MyGame;

impl GuestRuntime for MyGame {
    fn current_schema_version() -> i64 { 1 }

    fn validate_create_request(_input: Value) -> ValidateOutcome { ValidateOutcome::ok() }

    fn validate_update_request(input: Value) -> ValidateOutcome {
        // pure check on input["priorState"], input["state"], input["callerSub"]
        ValidateOutcome::ok()
    }

    fn validate_lobby_create(_input: Value) -> ValidateOutcome { ValidateOutcome::ok() }

    fn lobby_policy() -> LobbyPolicy {
        LobbyPolicy { min_members: 1, max_members: 1, auto_start_when_full: false, allow_guest: false }
    }

    fn create_initial_state(input: Value) -> Value {
        // input has: appId, gameId, state, callerSub, seed
        serde_json::json!({ /* ... your initial in-state shape ... */ })
    }

    fn state_for_lobby_start(input: Value) -> Value {
        // input has: appId, gameId, lobbyId, ownerSub, members, payload, seed
        serde_json::json!({ /* ... */ })
    }

    fn process_update(input: Value) -> ProcessUpdateOutput {
        // input has: appId, gameId, priorState, state, row, priors, callerSub, seed
        ProcessUpdateOutput { next_state: compute_next(&input), stat_updates: vec![] }
    }

    fn prior_stat_ids() -> PriorStatIds { PriorStatIds::default() }
    fn players_in_row(_row: Value) -> PlayersInRow { PlayersInRow::default() }
    fn leaderboard_stat_ids() -> LeaderboardStatIds { LeaderboardStatIds::default() }
}

// Emits __alloc/__free, abi_version, current_schema_version and every JSON
// export. Add hooks after the type: process_signal, state_for_viewer.
mijnspel_sdk::export_runtime!(MyGame);
```

Do not hand-write `#[no_mangle] extern "C"` glue; the macro expands in
your crate and emits it for `wasm32` only.

## Seats and hidden information

The host is the authority; the client never chooses a seat or sees
another player's hand.

- **Seats.** Assign `seats: { <sub>: 1..N }` from `ownerSub` and
  `members` in `state_for_lobby_start`.
- **Moves.** Clients `PUT /games/{id}` with `{ state: { move } }`.
  `validate_update_request` rejects unless `priorState.seats[callerSub]`
  is the seat whose turn it is; `process_update` computes the next
  state from `priorState`.
- **Randomness.** Shuffle and deal from the host `seed` (16 lowercase
  hex characters, fresh per request). Your WASM source may be public, so
  never derive secrets from the game id or a constant.
- **Views.** Export `state_for_viewer` (`export_runtime!(MyGame,
  state_for_viewer)`): input `{ appId, gameId, viewerSub, state, row }`,
  output the JSON that viewer receives in place of `row.state` (own
  hand, opponent counts, no deck). A non-member gets the public view.
  The host then redacts every outgoing row: REST responses, lobby join,
  WS fan-out and `/events` replay.

## Authoring `migration.wasm`

While your stored shape has never changed, the whole crate is one line:

```rust
mijnspel_sdk::export_migration!(mijnspel_sdk::guest::IdentityMigration<1>);
```

Once it changes, implement `GuestMigration` with pairwise steppers. The
host calls them one version at a time:

```rust
use mijnspel_sdk::guest::GuestMigration;
use serde_json::Value;

pub struct MyMigration;

impl GuestMigration for MyMigration {
    fn current_schema_version() -> i64 { 2 }
    fn min_supported_version() -> i64 { 1 }

    fn upgrade_from_previous(target: i64, state: Value) -> Value {
        match target {
            2 => v1_to_v2(state),
            _ => panic!("no upgrade defined for {target}"),
        }
    }

    fn downgrade_to_previous(target: i64, state: Value) -> Value {
        match target {
            2 => v2_to_v1(state),
            _ => panic!("no downgrade defined for {target}"),
        }
    }
}

mijnspel_sdk::export_migration!(MyMigration);
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

## Dev WebPush keys

WebPush (`POST /push-subscriptions` + signal-driven sends) needs a VAPID
keypair. Production reads it from SSM; the local dev-server has no SSM, so
an operator mints a **dedicated DEV keypair once** with a generation script.
The private key is written to a keyfile on the mounted `local-wasm` volume
(`/app/local-wasm/.vapid-dev.env`); the public key is printed for you to
paste into the **mijnspel-creator `.env`** as `DEV_VAPID_PUBLIC_KEY`.

There is **no auto-generation at boot and no public-key HTTP endpoint**. The
dev-server **refuses to start** until the keyfile exists (chicken-and-egg:
generate it into the same volume first, without a full boot).

1. Generate the keypair into the mounted volume (pick one):

   ```
   # mijnspel-creator compose (the dev-server service is defined there):
   docker compose run --rm --entrypoint /app/scripts/dev/gen-dev-vapid.sh dev-server

   # portable (no compose) — <volume> is whatever is mounted at /app/local-wasm:
   docker run --rm -v <volume>:/app/local-wasm \
     --entrypoint /app/scripts/dev/gen-dev-vapid.sh \
     ghcr.io/xafron-bv/mijnspel-dev-server:<tag>

   # host standalone local-backend — write into dev-up.sh's default keyfile
   # dir (<repo>/local-wasm); run from the repo root so the paths line up. If
   # you start dev-up.sh with a custom WASM_LOCAL_DIR, export the SAME value
   # here so both steps agree.
   cargo build --release -p mijnspel_backend_lambdas --bin vapid_keygen
   WASM_LOCAL_DIR=local-wasm ./scripts/dev/gen-dev-vapid.sh
   ```

2. Copy the printed `DEV_VAPID_PUBLIC_KEY=<public>` into the mijnspel-creator
   `.env`.
3. Start the dev-server normally; it reads `.vapid-dev.env` and serves push.

Re-running the script refuses to overwrite an existing keyfile (rotating it
would invalidate the public key already pasted into the creator `.env`); pass
`--force` to rotate deliberately.

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
- **`mijnspel_sdk::export_runtime!` / `export_migration!`** — the
  macros that emit the export glue (alloc/free, ptr/len packing, JSON
  encode/decode, `#[no_mangle] extern "C"` wrappers) from your trait
  impls.
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
