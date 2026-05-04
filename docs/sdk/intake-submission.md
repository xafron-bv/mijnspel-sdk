---
title: Submitting your WASM
sidebar_position: 9
---

# Submitting your WASM for production registration

Game devs do not have direct access to the production host.
Production registration is **manual, out-of-band, and admin-driven**.
You submit the artifacts; an operator runs the intake pipeline.

This page is what you, the game dev, need to know about that
handoff.

## What to submit

Two files:

- `runtime.wasm`
- `migration.wasm`

Both built `--release`, both targeting `wasm32-unknown-unknown`, both
authored against the same SDK version (the operator's verifier will
reject a mismatched pair).

## What the operator's verifier checks

Before any S3 or registry write, the verifier asserts:

- Both files load as valid WASM modules.
- The runtime exports the full required ABI surface (`abi_version`,
  `current_schema_version`, the `validate_*` family,
  `lobby_policy`, `create_initial_state`, `state_for_lobby_start`,
  `prior_stat_ids`, `players_in_row`, `process_update`,
  `leaderboard_stat_ids`). `process_signal` is optional.
- The migration exports `abi_version`, `current_schema_version`,
  `min_supported_version`, `upgrade_from_previous`,
  `downgrade_to_previous`.
- `runtime.abi_version() == migration.abi_version() ==
  HOST_ABI_VERSION`. A mismatch tells you which SDK version the host
  expects.
- `runtime.current_schema_version() ==
  migration.current_schema_version()`.

If any of those fail, the operator will tell you which one — fix
locally, rebuild, resend.

## Smoke-test before submitting

Run your artifacts through the full local stack first:

```
mkdir -p local-wasm/<your-app-id>
cp ...runtime.wasm   local-wasm/<your-app-id>/runtime.wasm
cp ...migration.wasm local-wasm/<your-app-id>/migration.wasm
docker run --rm -p 3000:3000 \
    -v $PWD/local-wasm:/app/local-wasm \
    ghcr.io/xafron-bv/mijnspel-dev-server:latest
```

Exercise the routes your game uses (`POST /games`, `PUT /games/{id}`,
the lobby flow if relevant, signals if you implemented them). Verify
the WS events fire. Verify a few rounds of update + read.

If you bumped `current_schema_version` since the previous submission,
also test that the migration walks an older state forward via the
local container's drop-in workflow (drop a row at the older shape into
the local container's DDB emulator with `awslocal`, then read it back
through the host).

## What the operator does

The operator runs an internal intake script with your two artifacts.
You don't see it directly. The script:

1. Verifies (offline).
2. Uploads each artifact under the content-addressed S3 key
   (`<appId>/<runtime|migration>/<sha>.wasm`); same-sha re-uploads are
   no-ops.
3. Reads back from S3 and re-verifies sha.
4. Atomically flips `app-registry.<appId>` to point at the new sha
   pair.
5. Writes an audit-trail entry on the operator side.

Within ≤ 60 s of the registry flip, every host instance is serving
requests through your new WASM.

## When you bumped `currentSchemaVersion`

For a release that changes `current_schema_version` (forward or
backward), the operator follows up with a bulk migration **after a
60 s drain**:

1. Activation flips the registry pointer.
2. The operator waits ≥ 60 s.
3. The operator triggers the bulk migration with `direction=up|down`
   and `targetVersion=<your new current>`.

The drain ensures warm host instances have refreshed against the new
pointer before the bulk job starts. Without it, warm-path writes can
race the batch.

You usually don't need to do anything during this — the on-read
coercion bridges any mixed-version reads while the batch runs. Just
be aware that if your migration code panics on real production rows,
the operator will surface that to you and the bulk migration is
rolled back.

## What you can *not* do

- **Push WASM artifacts directly to the production host.** No game
  dev has the credentials.
- **Flip the production registry pointer.** Same.
- **Trigger admin migrations against production.** Only the operator
  can.
- **Generate a WASM that needs network access or persistent storage
  outside `state`.** The sandbox has no host imports.

## Communication channel

The operator will tell you the email or chat channel where they
expect the artifacts. Out-of-band by design — this is how the system
authenticates "this binary is allowed to run for this appId".
