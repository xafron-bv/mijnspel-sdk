---
title: State and versioning
sidebar_position: 6
---

# State and versioning

## What `state` is

Every game row carries an opaque `state` field — JSON the host stores
verbatim and never inspects. Your runtime WASM is the only thing that
reads or writes its inner shape.

The host wraps `state` in an envelope:

```json
{
  "id": "<gameId>",
  "schemaVersion": <envelope-level, host-managed>,
  "state": <opaque per-game JSON, you control>,
  "createdAt": "...",
  "updatedAt": "...",
  "ttl": <optional epoch seconds>
}
```

Two fields share the name `schemaVersion`:

- **Envelope `schemaVersion`** (host-managed). Bumped only when the
  host changes the row's wrapper shape — extremely rare; treated as
  an envelope migration.
- **`state.schemaVersion`** (your version). Bumped when *your* in-state
  shape changes. Stamped by your runtime, walked by your migration.

When this guide says "schema version" without qualifier, it means the
inner one.

## The authoritative current version

The single source of truth for "what version is current" is
`app-registry.<appId>.currentSchemaVersion`. It is written by the
intake script during activation and bumped atomically by the bulk
migration consumer when a job transitions to `succeeded`.

Your runtime exports `current_schema_version` for the intake script
to assert against — and so the runtime knows the version it's
producing — but at request time the host trusts the registry value
for coercion.

## On-read coercion (host-side, automatic)

When a handler reads a row whose `state.schemaVersion` differs from
the registry's `currentSchemaVersion`, the host lazy-loads your
migration WASM and walks the state to the target — `upgrade_from_previous`
if the row is below current, `downgrade_to_previous` if above.

The persisted row is **not** rewritten by the read path. Bulk
persistence is the operator's job via
`POST /admin/migrations/run?appId=<id>` after a registry flip.

The implication for game devs: you can ship a new schema version
without forcing a bulk migration immediately. Reads will coerce
on the fly until the operator runs the bulk job. This is also what
lets a rollback be safe — rows still at the higher version coerce
*down* on read while the operator (and you) decide whether to follow
up with a bulk downgrade.

## Designing for safe migrations

A few rules of thumb:

- **Stamp `state.schemaVersion` in `create_initial_state`** so every
  fresh row is born at the version your runtime is producing.
- **Make upgrades total, not partial.** An upgrade that rejects some
  inputs leaves rows uncoerceable; the host will surface a 502 on
  reads of those rows.
- **Make downgrades work.** Even if you don't plan to roll back, the
  host can downgrade rows at *higher* versions during the rollback
  window. If a field is genuinely non-recoverable, document it and
  decide whether the downgrade fabricates a placeholder or panics.
- **Don't rely on side effects in steppers.** Steppers run in a
  sandbox with no host imports. Treat them as pure functions over
  JSON.
- **Keep stepper input/output JSON-serializable.** Anything that
  doesn't survive a `serde_json` round-trip will be lost.

## Worked example

Initial release at version 1:

```json
{ "board": "...", "turn": 0 }
```

Version 2 adds `difficulty` with default `"medium"`:

```rust
fn v1_to_v2(s: Value) -> Value {
    let mut s = s;
    s["difficulty"] = json!("medium");
    s
}
fn v2_to_v1(s: Value) -> Value {
    let mut s = s;
    s.as_object_mut().map(|o| o.remove("difficulty"));
    s
}
```

After your runtime + migration are uploaded and the registry flipped
to `currentSchemaVersion=2`:

- Existing rows at v1 read as v2 (with `difficulty="medium"` injected
  by the upgrade).
- New writes from your runtime stamp `schemaVersion=2`.
- A subsequent `POST /admin/migrations/run` rewrites all v1 rows on
  disk to v2 so on-read coercion stops being needed for that version
  pair on the hot path.

If you later realize v2 was a mistake, you re-upload v1's WASM pair,
wait the 60 s drain, and run `direction=down, targetVersion=1`. The
host walks rows at v2 down via `v2_to_v1`.
