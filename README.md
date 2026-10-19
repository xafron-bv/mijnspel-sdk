# mijnspel-sdk

The Rust SDK and developer guide for building games on the **mijnspel**
backend. This repository is the public mirror of the SDK, regenerated
from the upstream source at each release.

## What's here

- **`crates/mijnspel-sdk/`** — the Rust SDK crate. Per-game
  `runtime.wasm` + `migration.wasm` modules are authored against this.
- **`docs/sdk/`** — page-per-topic developer guide (quickstart, ABI
  reference, runtime exports, migration exports, schema versioning,
  lobbies + signals, local dev, intake submission, troubleshooting).
- **`docs/AGENT_BACKEND.md`** — condensed LLM-friendly reference for
  AI agents that need to wire a generated game frontend up to a local
  backend container.

## Installing the SDK

Add the crate as a Git dependency in your game's `Cargo.toml`. Pin to
a tag — every release of this repo carries one — so your build is
reproducible:

```toml
[dependencies]
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", tag = "v0.3.0" }
```

Then use it the usual way:

```rust
use mijnspel_sdk::abi::{HOST_ABI_VERSION, ProcessUpdateOutput, ValidateOutcome};
```

For a full per-game project layout (`backend/{core,runtime,migration}`)
and the export-glue boilerplate, see [`docs/sdk/quickstart.md`](docs/sdk/quickstart.md).

### Pinning by branch or revision

Tags are the recommended pin. Other options Cargo supports:

```toml
# Track main (not recommended for releases — moves underneath you):
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", branch = "main" }

# Pin to an exact commit (immutable; equivalent to a tag):
mijnspel-sdk = { git = "https://github.com/xafron-bv/mijnspel-sdk", rev = "<sha>" }
```

## Local development container

The companion Docker image runs the full backend offline so you can
exercise your WASM end-to-end without any cloud setup:

```
docker run --rm -p 3000:3000 \
    -v $PWD/local-wasm:/app/local-wasm \
    ghcr.io/xafron-bv/mijnspel-dev-server:latest
```

See [`docs/sdk/local-development.md`](docs/sdk/local-development.md)
for the drop-in WASM workflow and [`docs/sdk/quickstart.md`](docs/sdk/quickstart.md)
for the end-to-end project setup.

## Releases

Every tag (`v<semver>`) on this repo is a snapshot of the SDK source +
docs at the corresponding host release. Game devs pin to whichever
tag matches the host version they are building against.

## License

MIT — see [`LICENSE`](LICENSE).
