---
title: Runtime exports
sidebar_position: 4
---

# Runtime exports

The host calls into your runtime WASM through a fixed set of JSON-typed
exports. This page covers what each export is for, what's in the
input, and what you should return.

For the wire format and boilerplate, see
[ABI reference](./abi-reference.md). Implement
`mijnspel_sdk::guest::GuestRuntime` and end the crate with
`mijnspel_sdk::export_runtime!(MyGame)` to emit every export below; name
the optional hooks after the type to export them too, for example
`export_runtime!(MyGame, process_signal, state_for_viewer)`.

Two host-supplied fields recur below:

- `callerSub` — the authenticated user making the request. Use it to
  check that the caller holds the seat that may move.
- `seed` — fresh randomness for this request: 64 bits as 16 lowercase
  hex characters. The guest has no entropy of its own and its source may
  be public, so shuffle decks and roll dice from this seed.

## `validate_create_request`

**Called from:** `POST /games?appId=<id>`.

Pure validator. Decide whether the requested initial state is legal
for your game. Return `Ok` or `Reject { message }`. The host turns
`Reject` into `400` with the message you provide.

Input is `{ appId, gameId, state, callerSub, seed }`.

```rust
pub fn validate_create_request(input: serde_json::Value) -> ValidateOutcome {
    let state = input.get("state").cloned().unwrap_or_default();
    if !is_legal_initial(&state) {
        return ValidateOutcome::reject("invalid initial state");
    }
    ValidateOutcome::ok()
}
```

## `validate_update_request`

**Called from:** `PUT /games/{id}?appId=<id>` and the autostart
promotion path.

Input is `{ appId, gameId, priorState, state, callerSub }`. Use it to
enforce the per-game rules you'd normally guard at the boundary (turn
order, move legality, content sanity). For a game with seats, reject a
move unless `priorState` seats `callerSub` at the seat whose turn it is.

## `validate_lobby_create`

**Called from:** `POST /lobbies?appId=<id>`.

Input is `{ appId, lobbyId, ownerSub, members, payload }`. Decide
whether the proposed members and opaque `payload` are legal for your
game. Return `Reject` to fail the lobby creation.

## `lobby_policy`

**Called from:** `POST /lobbies?appId=<id>` (right after validate).

Return the per-game lobby policy:

```rust
LobbyPolicy {
    min_members: 2,
    max_members: 4,
    auto_start_when_full: false,
    allow_guest: true,
}
```

The host stamps these onto the lobby row at creation and enforces the
state machine generically. `auto_start_when_full = true` means a join
that brings `members.len() == max_members` triggers the host to
atomically promote the lobby to `started` and create a game from it.

A 1-on-1 invite UX is reproduced by `LobbyPolicy { 2, 2, true, false }`.

## `create_initial_state`

**Called from:** `POST /games?appId=<id>` (after `validate_create_request`)
and `POST /games/{id}/reset?appId=<id>`.

Return the initial in-state JSON for a fresh game. The host stores
this verbatim under `state` on the row.

Input is `{ appId, gameId, state, callerSub, seed }`.

```rust
pub fn create_initial_state(input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": current_schema_version(),
        "board": empty_board(),
        "turn": 0,
    })
}
```

If you stamp `schemaVersion` here (recommended), the host will use it
for on-read coercion later.

## `state_for_lobby_start`

**Called from:** the autostart path during the final join on a lobby
where `auto_start_when_full = true`.

Input is `{ appId, gameId, lobbyId, ownerSub, members, payload, seed }`.
Return the initial game state for the new game atomically created from
the lobby. This is where a seated game assigns seats (for example
`seats: { <sub>: 1..N }` in `members` order) and deals from `seed`. Optional row-extension fields can be lifted from the result
by host convention. The generated schema is in
`docs/api/wasm-abi.schema.json`.

## `prior_stat_ids`

**Called from:** every `process_update` to learn which stats the host
should pre-fetch as `priors`.

Return the stat ids whose stored values your runtime needs. Example:

```rust
pub fn prior_stat_ids() -> PriorStatIds {
    PriorStatIds { stat_ids: vec!["pvpRating".into()] }
}
```

## `players_in_row`

**Called from:** every `process_update`.

Extract the player ids from the row. The host uses your answer to
decide whose stats from `prior_stat_ids()` to fetch. Example for a
2-player game:

```rust
pub fn players_in_row(row: serde_json::Value) -> PlayersInRow {
    let s = row.get("state").cloned().unwrap_or_default();
    let players = vec![
        s["whiteSub"].as_str().unwrap_or_default().to_string(),
        s["blackSub"].as_str().unwrap_or_default().to_string(),
    ];
    PlayersInRow { player_ids: players }
}
```

## `process_update`

**Called from:** `PUT /games/{id}?appId=<id>`.

The main dispatch. Input is `{ appId, gameId, priorState, state, row,
priors, callerSub, seed }`. Given the current row, the requested next
state, and the fetched `priors[player][statId]`, compute the next state
and any stat writes. In a hidden-information game the client sends only
its move (`state: { move }`) and you compute the next state from
`priorState`. Return:

```rust
ProcessUpdateOutput {
    next_state: <new state JSON>,
    stat_updates: vec![
        StatUpdate {
            player_id: "alice".into(),
            stat_id: "pvpRating".into(),
            value: 1234.0,
            mode: "set".into(), // or "add", "max"
        },
        // ...
    ],
}
```

The host writes `next_state` under `(appId, gameId)` and applies each
`StatUpdate` against the generic `player-stats` store. The host has no
opinion about what your stats *mean*.

## `leaderboard_stat_ids`

**Called from:** the `stats_stream` DDB stream consumer.

Return the stat ids your game treats as leaderboard-relevant. The
stream consumer fans WS events out only for these:

```rust
pub fn leaderboard_stat_ids() -> LeaderboardStatIds {
    LeaderboardStatIds { stat_ids: vec!["pvpRating".into()] }
}
```

Subscribers receive `<appId>:leaderboard:<statId>` events when any
player's value for that stat changes.

## `process_signal` (optional)

**Called from:** `POST /games/{id}/signal?appId=<id>`.

If your game needs a per-session signal channel — pings, "knock",
"undo request", "pause" — export `process_signal`. The host validates
the JWT and the appId/game scope, then calls you with `{ appId, gameId,
state, signalType, callerSub, payload, row, nowMs }`. Return:

```rust
SignalOutcome::Ok {
    new_state: <state to persist; can equal the input>,
    targets: vec!["recipient-sub".into()],
    webpush: true,
    push_title: "Knock!".into(),
    push_body: "alice is asking to play.".into(),
}
```

or `SignalOutcome::Reject { message }` (turned into 403 by the host).

If you don't export `process_signal`, the host returns 404 for that
route on your appId.

## `state_for_viewer` (optional)

**Called for:** every game row the host sends to a user — the responses
of create, get, list, update and reset, the `game` in a lobby join
response, the WebSocket `gameUpdated` / `gameReset` fan-out (once per
subscriber), and `GET /events` replay.

Export it when players must not see the whole state, such as a hidden
hand or a shuffled deck. Input is `{ appId, gameId, viewerSub, state,
row }`, where `state` is the stored `row.state` (the same shape as
`priorState`). Return the JSON the host sends to `viewerSub` in place of
`row.state`. A viewer without a seat should get the public view.

```rust
fn state_for_viewer(input: serde_json::Value) -> serde_json::Value {
    let state = &input["state"]["state"];
    let seat = state["seats"][input["viewerSub"].as_str().unwrap_or("")].clone();
    serde_json::json!({
        "schemaVersion": input["state"]["schemaVersion"],
        "state": {
            "turn": state["turn"],
            "mySeat": seat,
            "hand": state["hands"][seat.to_string()],
            "deckCount": state["deck"].as_array().map_or(0, Vec::len),
        },
    })
}
```

The host detects the hook by export name. With it, every row the host
writes carries `viewerHook: true`, set in the same write as the state,
and only flagged rows are redacted: a row without the flag is sent as
stored, without loading your runtime. A failing call on a flagged row
never falls back to the full state: a write (`POST`, `PUT`, reset)
returns `502` and stores nothing, a `GET` returns `502`, the row is left
out of a `GET /games` page and of `/events`, and that WebSocket fan-out
is skipped (subscribers catch up on their next GET). The events table
itself keeps the full row.

If you later ship a runtime without `state_for_viewer`, flagged rows are
sent as stored (with a warning in the host log), and the next write
clears their flag.

Replayed events keep the schema they were recorded with, so the host
migrates their `state` to the current schema before calling you, as GET
does.
