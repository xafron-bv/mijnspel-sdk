# mijnspel-sdk

Shared SDK consumed by:

- per-game WASM modules (default features) — exposes the ABI types and the
  guest-side macro that emits the WASM exports from a `GameModule` impl.
- the mijnspel-backend host loader (`host` feature) — wasmtime-backed S3
  fetch + per-`(appId, sha)` compiled-module cache.

The crate is published from the `xafron-bv/mijnspel-backend` repo via the
`Publish mijnspel-sdk` GitHub Actions workflow on `sdk-v<semver>`
tags. ABI compatibility is signalled by the `HOST_ABI_VERSION` constant —
the host rejects guests whose `abi_version()` does not match.

See the `mijnspel-backend` repo's README for the per-game backend repo
template that depends on this crate.
