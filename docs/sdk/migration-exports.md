---
title: Migration exports
sidebar_position: 5
---

# Migration exports

The migration WASM is loaded lazily — only when the host needs to
walk a row's state version (on-read coercion or bulk migration). It
exports two pairwise steppers and a couple of versioning constants.

## Required exports

| Export | Returns | Used for |
|---|---|---|
| `abi_version` | `i64` | Must equal the host's `HOST_ABI_VERSION`. |
| `current_schema_version` | `i64` | Must equal the runtime WASM's `current_schema_version`. |
| `min_supported_version` | `i64` | The lowest version `downgrade_to_previous` is willing to walk to. |
| `upgrade_from_previous` | new state at `version` | Pairwise upgrade from `version - 1`. |
| `downgrade_to_previous` | new state at `version - 1` | Pairwise downgrade. |

## Pairwise stepper shape

The host calls one stepper per version step:

```
upgrade(2, state@v1) → state@v2
upgrade(3, state@v2) → state@v3
downgrade(3, state@v3) → state@v2
downgrade(2, state@v2) → state@v1
```

Steppers receive the target version (the version they're producing)
and the input state at the previous version. Implement them as a
single match on the target:

```rust
pub fn upgrade_from_previous(target: i64, state: serde_json::Value)
    -> serde_json::Value
{
    match target {
        2 => v1_to_v2(state),
        3 => v2_to_v3(state),
        _ => panic!("no upgrade defined for version {target}"),
    }
}
```

A panic in the WASM surfaces as a `502` from the host with a
diagnostic log line.

## Reversibility

Rollbacks call `downgrade_to_previous`, so each upgrade you ship
should have a corresponding downgrade. If a field is genuinely
non-recoverable (a generated id, a derived score), document it: the
downgrade can either fabricate a placeholder or return a rejection
(panic) to make the rollback fail loudly.

`min_supported_version` lets you cap how far down a downgrade chain
the host is allowed to walk. A request whose persisted state is
below `min_supported_version` after coercion is treated as a fatal
error.

## Schema-version stamping

The host writes `state.schemaVersion` according to the registry's
`currentSchemaVersion` after a migration job's final batch. On the
read path, the on-read coercion **rewraps** the result so callers
always see `{ schemaVersion, state: <inner> }` regardless of the
persisted shape. Your steppers should preserve `schemaVersion` if they
see it in the input — the cleanest pattern is to never read it; the
host knows the version it wants, and your stepper produces an inner
state shape that matches that version.

## Migration WASM lifecycle

- **Loaded:** lazily on the first read where
  `state.schemaVersion ≠ registry.currentSchemaVersion`, or at the
  start of every batch processed by `migrations_batch`.
- **Cached:** per `(appId, sha)` in the same compiled-module cache as
  the runtime WASM.
- **Replaced:** atomically with a runtime upgrade — a single intake
  pass flips the active runtime + migration sha pair together. You
  never ship a runtime without a matching migration.

## Writing migration tests

Migrations are pure functions over JSON. Unit-test them with
`serde_json` literals:

```rust
#[test]
fn upgrade_v1_to_v2_adds_default_difficulty() {
    let v1 = serde_json::json!({ "board": "...", "turn": 0 });
    let v2 = upgrade_from_previous(2, v1);
    assert_eq!(v2["difficulty"], serde_json::json!("medium"));
}
```

The operator's offline verifier can optionally replay a fixture
corpus through both steppers and assert no panics. Including a small
representative corpus alongside your submitted artifacts is good
practice for any release that bumps `current_schema_version`.
