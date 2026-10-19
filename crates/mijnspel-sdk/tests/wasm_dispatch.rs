//! End-to-end SDK smoke test: loads the runtime + migration fixtures from
//! `integration-tests/fixtures/wasm/` (built by
//! `scripts/build-wasm-fixtures.sh`) and exercises every JSON export through
//! the host loader.
//!
//! Skipped unless the fixtures are present so non-fixture clones still pass
//! `cargo test`.

#![cfg(feature = "host")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mijnspel_sdk::host::{
    verify_artifacts, BinaryFetcher, BinaryKind, MigrationInstance, RuntimeInstance, WasmError,
    WasmLoader,
};
use sha2::Digest;

/// In-memory fetcher keyed by sha so the test can pre-register fixture
/// content and call `WasmLoader::load(sha)` against it. The host's sha-verify
/// runs against the bytes returned, matching production behaviour.
struct InMemoryFetcher {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryFetcher {
    fn new() -> Self {
        Self {
            blobs: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, sha: &str, bytes: Vec<u8>) {
        self.blobs.lock().unwrap().insert(sha.to_string(), bytes);
    }
}

#[async_trait]
impl BinaryFetcher for InMemoryFetcher {
    async fn fetch(
        &self,
        _app_id: &str,
        _kind: BinaryKind,
        sha: &str,
    ) -> Result<Vec<u8>, WasmError> {
        let map = self.blobs.lock().unwrap();
        map.get(sha)
            .cloned()
            .ok_or_else(|| WasmError::Fetch(format!("no fixture for {sha}")))
    }
}

fn fixtures_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("integration-tests/fixtures/wasm");
    if dir.join("runtime-v1.wasm").exists() {
        Some(dir)
    } else {
        None
    }
}

fn read_fixture(name: &str) -> (String, Vec<u8>) {
    let path = fixtures_dir().expect("fixtures present").join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    let sha = hex::encode(hasher.finalize());
    (sha, bytes)
}

fn make_loader(fetcher: Arc<InMemoryFetcher>) -> WasmLoader {
    WasmLoader::new(fetcher).expect("engine")
}

#[tokio::test]
async fn runtime_fixture_round_trip() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let fetcher = Arc::new(InMemoryFetcher::new());
    let (sha, bytes) = read_fixture("runtime-v1.wasm");
    fetcher.register(&sha, bytes);
    let loader = make_loader(fetcher);
    let cached = loader.load("smoke", BinaryKind::Runtime, &sha).await.expect("load");
    let mut inst = RuntimeInstance::new(&cached).expect("instance");
    assert_eq!(inst.current_schema_version().unwrap(), 1);
    let outcome = inst
        .validate_create_request(serde_json::json!({ "appId": "x", "state": {} }))
        .unwrap();
    assert!(matches!(
        outcome,
        mijnspel_sdk::abi::ValidateOutcome::Ok
    ));
    let initial = inst
        .create_initial_state(serde_json::json!({ "state": { "score": 1 } }))
        .unwrap();
    assert_eq!(initial.get("fixtureBuildId").and_then(|v| v.as_i64()), Some(1));
    let processed = inst
        .process_update(serde_json::json!({ "state": { "playerId": "p", "score": 7.0 } }))
        .unwrap();
    assert_eq!(processed.stat_updates.len(), 1);
    assert_eq!(processed.stat_updates[0].stat_id, "fixture.score");
    let leaderboard = inst.leaderboard_stat_ids().unwrap();
    assert_eq!(leaderboard.stat_ids, vec!["fixture.score".to_string()]);
}

#[tokio::test]
async fn migration_fixture_round_trip() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let fetcher = Arc::new(InMemoryFetcher::new());
    let (sha, bytes) = read_fixture("migration-v1.wasm");
    fetcher.register(&sha, bytes);
    let loader = make_loader(fetcher);
    let cached = loader
        .load("smoke", BinaryKind::Migration, &sha)
        .await
        .expect("load");
    let mut inst = MigrationInstance::new(&cached).expect("instance");
    assert_eq!(inst.min_supported_version().unwrap(), 1);
    let up = inst
        .upgrade_from_previous(2, serde_json::json!({ "score": 1 }))
        .unwrap();
    assert!(up
        .get("migrated_up_to_2")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let down = inst.downgrade_to_previous(2, up).unwrap();
    assert!(down
        .get("downgraded_from_2")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
}

#[tokio::test]
async fn bad_abi_fixture_fails_construction() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let fetcher = Arc::new(InMemoryFetcher::new());
    let (sha, bytes) = read_fixture("runtime-bad-abi.wasm");
    fetcher.register(&sha, bytes);
    let loader = make_loader(fetcher);
    let cached = loader
        .load("smoke", BinaryKind::Runtime, &sha)
        .await
        .expect("compile");
    let err = RuntimeInstance::new(&cached).err().expect("abi mismatch");
    match err {
        WasmError::AbiMismatch { expected, got } => {
            assert_eq!(expected, mijnspel_sdk::abi::HOST_ABI_VERSION);
            assert_eq!(got, 99);
        }
        other => panic!("expected AbiMismatch, got {other}"),
    }
}

#[test]
fn verify_artifacts_runtime_and_migration_pair() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let (runtime_sha, runtime_bytes) = read_fixture("runtime-v1.wasm");
    let (migration_sha, migration_bytes) = read_fixture("migration-v1.wasm");

    let report =
        verify_artifacts(&runtime_bytes, Some(&migration_bytes)).expect("verify pair");
    assert_eq!(report.abi_version, mijnspel_sdk::abi::HOST_ABI_VERSION);
    assert_eq!(report.current_schema_version, 1);
    assert_eq!(report.runtime_sha256, runtime_sha);
    assert_eq!(report.migration_sha256, migration_sha);
}

#[test]
fn verify_artifacts_runtime_only_has_empty_migration_sha() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let (runtime_sha, runtime_bytes) = read_fixture("runtime-v1.wasm");

    let report = verify_artifacts(&runtime_bytes, None).expect("verify runtime-only");
    assert_eq!(report.abi_version, mijnspel_sdk::abi::HOST_ABI_VERSION);
    assert_eq!(report.current_schema_version, 1);
    assert_eq!(report.runtime_sha256, runtime_sha);
    assert_eq!(report.migration_sha256, "");
}

#[test]
fn verify_artifacts_rejects_abi_mismatch() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let (_sha, runtime_bytes) = read_fixture("runtime-bad-abi.wasm");

    let err = verify_artifacts(&runtime_bytes, None).expect_err("abi mismatch");
    match err {
        WasmError::AbiMismatch { expected, got } => {
            assert_eq!(expected, mijnspel_sdk::abi::HOST_ABI_VERSION);
            assert_eq!(got, 99);
        }
        other => panic!("expected AbiMismatch, got {other}"),
    }
}

fn has_fixture(name: &str) -> bool {
    fixtures_dir().is_some_and(|dir| dir.join(name).exists())
}

async fn views_instance() -> RuntimeInstance {
    let fetcher = Arc::new(InMemoryFetcher::new());
    let (sha, bytes) = read_fixture("runtime-views.wasm");
    fetcher.register(&sha, bytes);
    let loader = make_loader(fetcher);
    let cached = loader
        .load("views", BinaryKind::Runtime, &sha)
        .await
        .expect("load");
    RuntimeInstance::new(&cached).expect("instance")
}

/// Stored row state for a lobby game between alice (seat 1) and bob (seat 2).
fn dealt_lobby_state(inst: &mut RuntimeInstance) -> serde_json::Value {
    let inner = inst
        .state_for_lobby_start(serde_json::json!({
            "appId": "views",
            "gameId": "g",
            "lobbyId": "l",
            "ownerSub": "alice",
            "members": ["alice", "bob"],
            "payload": null,
            "seed": "00000000000000ff",
        }))
        .unwrap();
    serde_json::json!({ "schemaVersion": 1, "state": inner })
}

#[tokio::test]
async fn runtime_views_fixture_redacts_state_per_viewer() {
    if !has_fixture("runtime-views.wasm") {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let mut inst = views_instance().await;
    let stored = dealt_lobby_state(&mut inst);
    let alice_hand = stored["state"]["hands"]["1"].clone();
    assert_eq!(alice_hand.as_array().map(Vec::len), Some(3));
    assert_eq!(stored["state"]["seedOk"], true);

    let view_for = |inst: &mut RuntimeInstance, viewer: &str| {
        inst.state_for_viewer(serde_json::json!({
            "appId": "views",
            "gameId": "g",
            "viewerSub": viewer,
            "state": stored.clone(),
            "row": { "gameId": "g", "state": stored.clone() },
        }))
        .unwrap()
        .expect("runtime-views exports state_for_viewer")
    };

    let alice = view_for(&mut inst, "alice");
    assert_eq!(alice["schemaVersion"], 1);
    assert_eq!(alice["state"]["hand"], alice_hand);
    assert_eq!(alice["state"]["mySeat"], 1);
    assert_eq!(alice["state"]["handCounts"]["2"], 3);
    assert_eq!(alice["state"]["deckCount"], 14);
    assert!(alice["state"].get("hands").is_none());
    assert!(alice["state"].get("deck").is_none());

    let outsider = view_for(&mut inst, "eve");
    assert_eq!(outsider["state"]["hand"], serde_json::json!([]));
    assert_eq!(outsider["state"]["mySeat"], serde_json::Value::Null);
}

#[tokio::test]
async fn runtime_views_fixture_enforces_the_callers_seat() {
    if !has_fixture("runtime-views.wasm") {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let mut inst = views_instance().await;
    let stored = dealt_lobby_state(&mut inst);
    let alice_card = stored["state"]["hands"]["1"][0].clone();
    let bob_card = stored["state"]["hands"]["2"][0].clone();
    let attempt = |inst: &mut RuntimeInstance, caller: &str, card: &serde_json::Value| {
        inst.validate_update_request(serde_json::json!({
            "appId": "views",
            "gameId": "g",
            "priorState": stored.clone(),
            "state": { "move": { "card": card } },
            "callerSub": caller,
        }))
        .unwrap()
    };
    assert!(
        !attempt(&mut inst, "bob", &bob_card).is_ok(),
        "not bob's turn"
    );
    assert!(
        !attempt(&mut inst, "eve", &alice_card).is_ok(),
        "eve has no seat"
    );
    assert!(
        !attempt(&mut inst, "alice", &bob_card).is_ok(),
        "not alice's card"
    );
    assert!(attempt(&mut inst, "alice", &alice_card).is_ok());

    let next = inst
        .process_update(serde_json::json!({
            "appId": "views",
            "gameId": "g",
            "priorState": stored.clone(),
            "state": { "move": { "card": alice_card } },
            "row": {},
            "priors": {},
            "callerSub": "alice",
            "seed": "0123456789abcdef",
        }))
        .unwrap()
        .next_state;
    assert_eq!(next["turn"], 2);
    assert_eq!(next["seq"], 1);
    assert_eq!(next["lastMover"], "alice");
    assert_eq!(next["hands"]["1"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn runtime_without_viewer_hook_returns_none() {
    if fixtures_dir().is_none() {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let fetcher = Arc::new(InMemoryFetcher::new());
    let (sha, bytes) = read_fixture("runtime-v1.wasm");
    fetcher.register(&sha, bytes);
    let loader = make_loader(fetcher);
    let cached = loader.load("smoke", BinaryKind::Runtime, &sha).await.expect("load");
    assert!(!cached.has_export(mijnspel_sdk::abi::runtime_exports::STATE_FOR_VIEWER));
    let mut inst = RuntimeInstance::new(&cached).expect("instance");
    let view = inst
        .state_for_viewer(serde_json::json!({ "viewerSub": "alice", "state": { "a": 1 } }))
        .unwrap();
    assert!(view.is_none());
}

#[test]
fn export_runtime_macro_emits_exactly_the_runtime_exports() {
    if !has_fixture("runtime-views.wasm") {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let (_sha, bytes) = read_fixture("runtime-views.wasm");
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, &bytes).expect("compile");
    let mut functions: Vec<String> = module
        .exports()
        .filter(|e| matches!(e.ty(), wasmtime::ExternType::Func(_)))
        .map(|e| e.name().to_string())
        .collect();
    functions.sort();
    let mut expected = vec![
        "__alloc",
        "__free",
        "abi_version",
        "current_schema_version",
        "validate_create_request",
        "validate_update_request",
        "validate_lobby_create",
        "lobby_policy",
        "create_initial_state",
        "state_for_lobby_start",
        "prior_stat_ids",
        "process_update",
        "leaderboard_stat_ids",
        "players_in_row",
        "state_for_viewer",
    ];
    expected.sort();
    assert_eq!(functions, expected);
}

#[test]
fn verify_artifacts_accepts_runtime_views() {
    if !has_fixture("runtime-views.wasm") {
        eprintln!("skipping: build with scripts/build-wasm-fixtures.sh first");
        return;
    }
    let (sha, bytes) = read_fixture("runtime-views.wasm");
    let report = verify_artifacts(&bytes, None).expect("verify runtime-views");
    assert_eq!(report.current_schema_version, 1);
    assert_eq!(report.runtime_sha256, sha);
}
