# mijnspel-sdk

Shared SDK consumed by:

- per-game WASM modules (default features) — the ABI types, the
  `GuestRuntime` / `GuestMigration` traits, and the `export_runtime!` /
  `export_migration!` macros that emit every WASM export from an impl:

  ```rust
  pub struct MyGame;
  impl mijnspel_sdk::guest::GuestRuntime for MyGame { /* … */ }
  // Optional hooks after the type: process_signal, state_for_viewer.
  mijnspel_sdk::export_runtime!(MyGame, state_for_viewer);
  ```

  ```rust
  // migration crate, for a game whose stored shape never changed
  mijnspel_sdk::export_migration!(mijnspel_sdk::guest::IdentityMigration<1>);
  ```

- the mijnspel-backend host loader (`host` feature) — wasmtime-backed S3
  fetch + per-`(appId, sha)` compiled-module cache.

The crate is mirrored from the `xafron-bv/mijnspel-backend` repo to the
public `xafron-bv/mijnspel-sdk` repo by `tools/sdk-mirror/mirror.sh`, which
tags `v<crate-version>`. ABI compatibility is signalled by the
`HOST_ABI_VERSION` constant — the host rejects guests whose `abi_version()`
does not match.

See `docs/sdk/` for the per-export reference.
