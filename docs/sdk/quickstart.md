---
title: Quickstart
sidebar_position: 2
---

# Quickstart

This page gets you from zero to a runnable WASM that the local backend
exercises. You'll need:

- A recent stable Rust toolchain with the `wasm32-unknown-unknown`
  target installed (`rustup target add wasm32-unknown-unknown`).
- Docker, to run `ghcr.io/xafron-bv/mijnspel-dev-server:latest`.

## Project layout

A per-game backend repo follows this canonical layout (the rest of
this guide assumes it):

```
your-game/
├── (your game's frontend, if any)
└── backend/
    ├── core/                 # game-agnostic logic shared by both crates
    ├── runtime/Cargo.toml    # builds runtime.wasm
    ├── migration/Cargo.toml  # builds migration.wasm
    └── build.sh              # cargo build --target wasm32-unknown-unknown for both
```

Both `runtime/` and `migration/` are `cdylib` crates depending on the
SDK and on a shared `core/` crate.

## Minimal `runtime/Cargo.toml`

```toml
[package]
name = "your-game-runtime"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", tag = "v0.1.0" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

`migration/Cargo.toml` is structurally identical.

The `mijnspel-sdk` crate is distributed via Git, not crates.io. Pin
to a release tag (`v<semver>`) so your build is reproducible — every
release of the host has a matching tag on the public repo.

Other Cargo-supported pinning modes:

```toml
# Track main (moves underneath you; not recommended for releases):
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", branch = "main" }

# Pin to an exact commit (immutable; equivalent to a tag):
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", rev = "<sha>" }
```

## Minimal runtime (opaque key/value game)

A runtime that just round-trips state — useful as a smoke test before
you add real game logic:

```rust
// backend/runtime/src/lib.rs
use mijnspel_sdk::abi::{ProcessUpdateOutput, ValidateOutcome};

// (See docs/sdk/abi-reference.md for the export-glue boilerplate. The
// guest macro in mijnspel_sdk::guest::reference shows the exact
// no_mangle extern "C" wrappers your crate needs to expose. They are
// thin: alloc, free, abi_version, current_schema_version, and one wrapper
// per JSON-typed export below.)

pub fn abi_version() -> i64 {
    mijnspel_sdk::abi::HOST_ABI_VERSION
}

pub fn current_schema_version() -> i64 { 1 }

pub fn validate_create_request(_input: serde_json::Value) -> ValidateOutcome {
    ValidateOutcome::ok()
}

pub fn create_initial_state(input: serde_json::Value) -> serde_json::Value {
    input.get("state").cloned().unwrap_or(serde_json::Value::Null)
}

pub fn process_update(input: serde_json::Value) -> ProcessUpdateOutput {
    let next = input.get("request")
        .and_then(|r| r.get("state"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    ProcessUpdateOutput { next_state: next, stat_updates: vec![] }
}
```

For the export-glue boilerplate (alloc/free, ptr/len packing, JSON
in/out per export), copy from the SDK's `guest::reference` module —
view it with `cargo doc --open -p mijnspel-sdk` and navigate
to `mijnspel_sdk::guest::reference`. The reference is
documented inline as the canonical shape; the `#[no_mangle] extern "C"`
blocks must live in your leaf crate (not in the SDK) so the linker
picks them up when building `cdylib`.

## Build

```
cd backend
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path runtime/Cargo.toml
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path migration/Cargo.toml
```

Output lands at:

```
target/wasm32-unknown-unknown/release/your_game_runtime.wasm
target/wasm32-unknown-unknown/release/your_game_migration.wasm
```

## Run against the local backend

```
mkdir -p local-wasm/your-game
cp target/wasm32-unknown-unknown/release/your_game_runtime.wasm \
   local-wasm/your-game/runtime.wasm
cp target/wasm32-unknown-unknown/release/your_game_migration.wasm \
   local-wasm/your-game/migration.wasm

docker run --rm -p 3000:3000 \
    -v $PWD/local-wasm:/app/local-wasm \
    ghcr.io/xafron-bv/mijnspel-dev-server:latest
```

The container starts MiniStack, runs the bootstrap, sees your
`local-wasm/your-game/` directory, sha256s the artifacts, uploads them
under the production content-addressed S3 keys to the in-container
MiniStack, and seeds `app-registry.your-game`. A subsequent
`POST /games?appId=your-game` exercises your WASM.

You can `SIGHUP` the container after rebuilding to pick up new bytes
without restarting it (it re-scans the mount).

## What's next

- Wire in real validation rules — see
  [Runtime exports](./runtime-exports.md).
- Decide whether your game needs lobbies — see
  [Lobbies and signals](./lobbies-and-signals.md).
- Once your WASM works locally, [submit the artifacts](./intake-submission.md)
  to the host operator for production registration.
