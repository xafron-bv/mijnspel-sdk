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
    BinaryFetcher, BinaryKind, MigrationInstance, RuntimeInstance, WasmError, WasmLoader,
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
