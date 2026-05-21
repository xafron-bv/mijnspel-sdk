---
title: Runtime exports
sidebar_position: 4
---

# Runtime exports

The host calls into your runtime WASM through a fixed set of JSON-typed
exports. This page covers what each export is for, what's in the
input, and what you should return.

For the wire format and boilerplate, see
[ABI reference](./abi-reference.md).

## `validate_create_request`

**Called from:** `POST /games?appId=<id>`.

Pure validator. Decide whether the requested initial state is legal
for your game. Return `Ok` or `Reject { message }`. The host turns
`Reject` into `400` with the message you provide.

Input is `{ appId, gameId, state }`.

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

Input is `{ appId, gameId, priorState, state }`. Use it to enforce the
per-game rules you'd normally guard at the boundary (turn order, move
legality, content sanity).

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

**Called from:** `POST /games?appId=<id>` (after `validate_create_request`).

Return the initial in-state JSON for a fresh game. The host stores
this verbatim under `state` on the row.

Input is `{ appId, gameId, state }`.

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

Input is `{ appId, gameId, lobbyId, ownerSub, members, payload }`.
Return the initial game state for the new game atomically created from
the lobby. Optional row-extension fields can be lifted from the result
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
priors }`. Given the current row, the requested next state, and the
fetched `priors[player][statId]`, compute the next state and any stat
writes. Return:

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
