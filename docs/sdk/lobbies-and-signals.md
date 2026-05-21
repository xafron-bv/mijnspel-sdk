---
title: Lobbies and signals
sidebar_position: 7
---

# Lobbies and signals

The host's multiplayer primitive is a **lobby** — an N-member group
with a `pending → started | cancelled` state machine. Per-game shape
is declared by your WASM via `lobby_policy` and `validate_lobby_create`.

## When you need lobbies

You need lobbies if your game has any of:

- Multiple human players coming together before play starts.
- A "wait until everyone joined" phase.
- Asynchronous invites (member B accepts an invite from member A).
- An autostart-when-full flow.

If your game is single-player or starts immediately on `POST /games`,
you can skip lobbies — your `validate_lobby_create` and
`state_for_lobby_start` can be unimplemented (the host will simply
never call them).

## Declaring policy

```rust
pub fn lobby_policy() -> LobbyPolicy {
    LobbyPolicy {
        min_members: 2,
        max_members: 4,
        auto_start_when_full: false,
        allow_guest: true,
    }
}
```

The host stamps these onto the lobby row at creation and enforces the
state machine generically.

A 1-on-1 invite UX is `LobbyPolicy { 2, 2, true, false }`. The host
has no built-in 2-player special-casing — every flow goes through
the same lobby endpoints.

## `validate_lobby_create`

Called on `POST /lobbies?appId=<id>`. Input is `{ appId, lobbyId,
ownerSub, members, payload }`. Reject if the proposed members and
opaque per-game `payload` are illegal:

```rust
pub fn validate_lobby_create(input: serde_json::Value) -> ValidateOutcome {
    let members = input["members"].as_array().cloned().unwrap_or_default();
    if members.len() < 2 {
        return ValidateOutcome::reject("need at least two members");
    }
    ValidateOutcome::ok()
}
```

`payload` is whatever per-game data your frontend wants to round-trip
(mode, difficulty, AI opponent profile). The host stores it verbatim
and hands it back in `state_for_lobby_start`.

## `state_for_lobby_start`

Called on the autostart-triggering join when
`auto_start_when_full = true`. Input is `{ appId, gameId, lobbyId,
ownerSub, members, payload }`. Return the initial game state:

```rust
pub fn state_for_lobby_start(input: serde_json::Value) -> serde_json::Value {
    let members = input["members"].as_array().cloned().unwrap_or_default();
    serde_json::json!({
        "schemaVersion": current_schema_version(),
        "members": members,
        "turn": 0,
        "board": empty_board(),
    })
}
```

The promotion is atomic: the host writes the new game row, stamps
`gameId` on the lobby, and flips the lobby `pending → started` in a
single transaction. Your runtime is called once during the
transaction; if it returns an error, the transaction rolls back.

## Per-session signals

The host exposes an **optional** per-session signal channel:

```
POST /games/{id}/signal?appId=<id>
```

Use it for in-game pings, "knock to wake the opponent", "request
undo", "pause request", or any other ephemeral message that:

- Targets a small set of subs (typically the other player(s) in the
  game).
- May want a WebPush payload too (for sleeping recipients).
- May or may not write a small piece of state on the row (cooldown
  bookkeeping, "knock count").

Implement it by exporting `process_signal`:

```rust
pub fn process_signal(input: serde_json::Value) -> SignalOutcome {
    let row = &input["row"];
    let signal_type = input["signalType"].as_str().unwrap_or("");
    let caller_sub = input["callerSub"].as_str().unwrap_or("");

    match signal_type {
        "knock" => {
            // basic cooldown enforcement: at most one knock per minute
            if recent_knock(row, caller_sub) {
                return SignalOutcome::Reject {
                    message: "knock cooldown".into(),
                };
            }
            SignalOutcome::Ok {
                new_state: stamp_knock_time(row, caller_sub),
                targets: opponents_of(row, caller_sub),
                webpush: true,
                push_title: format!("{} is knocking", display_name(caller_sub)),
                push_body: "Tap to return".into(),
            }
        }
        _ => SignalOutcome::Reject { message: "unknown signal".into() },
    }
}
```

The host writes `new_state` back to the game row, dispatches a WS
event of kind `signal` to each `targets` sub, and (if `webpush=true`)
sends the WebPush payload.

If you don't export `process_signal`, the host returns `404` for
that route — no need to declare anything to opt out.

## Lobby state machine recap

The host enforces the lobby state machine; you enforce per-game
content rules via the validators.

| Transition | Trigger | Notes |
|---|---|---|
| `pending → pending` | join below maxMembers | `validate_lobby_create` is **not** re-run on join |
| `pending → started` | join that fills the lobby with `auto_start_when_full = true` | atomic with game creation via `state_for_lobby_start` |
| `pending → cancelled` | owner cancel, or last non-owner leaves | terminal |
