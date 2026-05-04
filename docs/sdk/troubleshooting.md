---
title: Troubleshooting
sidebar_position: 10
---

# Troubleshooting

## "wasm abi mismatch: host expected N, guest reported M"

Your runtime or migration was built against a different SDK version
than the host expects. Update the `mijnspel-sdk` dependency
and rebuild both crates:

```
cargo update -p mijnspel-sdk
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path runtime/Cargo.toml \
    --manifest-path migration/Cargo.toml
```

The intake script's offline verifier catches this before any S3
write, so on production you will see the error from the operator
before the registry is touched. Locally, you'll see it in the
container logs the next time the host loads your WASM.

## "400 unregistered app id"

The host doesn't know your `appId` yet. Two cases:

- **In the local container.** Your WASMs aren't under
  `local-wasm/<appId>/`, or you haven't restarted (or `SIGHUP`-ed)
  the container since adding them. The `app-registry` row is seeded
  at scan time only.
- **In production.** The intake script hasn't run yet — your appId
  is not in `app-registry` until the operator activates it.

## A request returns 502 with no clear cause

Most 502s from the host are WASM-side failures: a panic, a `WasmError`,
or a JSON shape your runtime returned that the host couldn't decode.
Check the container logs (`docker logs <container>`) — every WASM
trap surfaces as a structured log line including the export name and
the error.

Common causes:

- A serialization mismatch (e.g. you returned a custom enum without
  `#[serde(tag = "kind", rename_all = "camelCase")]`).
- A panic deep inside your `process_update` (off-by-one, JSON pointer
  miss).
- An unhandled match arm in `upgrade_from_previous` /
  `downgrade_to_previous`.

The host doesn't recover from WASM panics. Make your steppers and
`process_update` total over the inputs you accept.

## My runtime updates aren't picked up

If you rebuild but the container keeps serving old behavior:

- The container computes the sha at scan time. A rebuild that
  produces *the same bytes* (deterministic build, no changes) will
  produce the same sha and the registry pointer won't move.
- If the bytes did change, the container needs to re-scan. Either
  send `SIGHUP` to the container or restart it. The bootstrap is
  idempotent — restarts are cheap.

In production, every intake produces a fresh `<sha>.wasm` key by
content-addressing. The registry flip happens unconditionally when
the script runs to completion.

## A row appears at the wrong schema version after a rollback

Symptom: rows you migrated `direction=down` appear at the higher
version on read.

Likely cause: the operator skipped or shortened the drain window
between the registry flip and the migration trigger. Warm host
processes were still resolving the higher `currentSchemaVersion` and
their writes raced the batch. The drain is the operator's
responsibility — flag this to them and they'll re-run the migration
correctly.

## The on-read coercion isn't running

For coercion to fire, the registry must have a `currentSchemaVersion`
*and* your row's `state.schemaVersion` must differ from it. Common
reasons it doesn't fire:

- The registry has no `currentSchemaVersion` (e.g. an opaque appId
  with no WASM uploaded yet). The host passes the row through
  unchanged.
- The row's `state.schemaVersion` is missing entirely. The host
  treats it as "born current" and does not migrate.
- Your migration WASM isn't registered. Without
  `currentMigrationSha256` the host gives up on coercion and returns
  the row uncoerced.

## How do I see what the registry says about my appId?

In the local container:

```
docker exec <container> aws --endpoint-url=http://localhost:4566 \
    dynamodb get-item --table-name mijnspel-backend-app-registry \
    --key '{"appId":{"S":"<your-app-id>"}}'
```

In production: ask the operator. There's no read-side admin API for
`app-registry` exposed to game devs.
