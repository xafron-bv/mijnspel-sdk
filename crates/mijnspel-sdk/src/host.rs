//! Host-side WASM loader, sandboxed instance helpers, and the JSON-boundary
//! invocation primitives.
//!
//! Sha resolution + S3 fetch + compilation lives here. Each call instantiates
//! a fresh `Store` against the cached `Module` so guest state never leaks
//! across requests; modules expose no host imports, so even a malicious
//! guest cannot reach DDB.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::abi::{
    memory_exports, migration_exports, runtime_exports, unpack_ptr_len, HostJson,
    LeaderboardStatIds, LobbyPolicy, PlayersInRow, PriorStatIds, ProcessUpdateOutput,
    SignalOutcome, ValidateOutcome, HOST_ABI_VERSION,
};
use crate::cache::{CachedModule, ModuleCache};

/// Errors produced by the loader / dispatch layer. The lambda crate maps
/// these to HTTP responses; the loader does not assume a particular error
/// representation.
#[derive(Debug)]
pub enum WasmError {
    Fetch(String),
    Compile(String),
    Instantiate(String),
    AbiMismatch { expected: i64, got: i64 },
    Call(String),
    Decode(String),
    Memory(String),
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmError::Fetch(msg) => write!(f, "wasm fetch failed: {msg}"),
            WasmError::Compile(msg) => write!(f, "wasm compile failed: {msg}"),
            WasmError::Instantiate(msg) => write!(f, "wasm instantiate failed: {msg}"),
            WasmError::AbiMismatch { expected, got } => write!(
                f,
                "wasm abi mismatch: host expected {expected}, guest reported {got}"
            ),
            WasmError::Call(msg) => write!(f, "wasm call failed: {msg}"),
            WasmError::Decode(msg) => write!(f, "wasm decode failed: {msg}"),
            WasmError::Memory(msg) => write!(f, "wasm memory error: {msg}"),
        }
    }
}

impl std::error::Error for WasmError {}

/// Indirection so tests can inject blobs without S3.
#[async_trait::async_trait]
pub trait BinaryFetcher: Send + Sync {
    async fn fetch(&self, app_id: &str, kind: BinaryKind, sha: &str) -> Result<Vec<u8>, WasmError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryKind {
    Runtime,
    Migration,
}

impl BinaryKind {
    pub fn segment(&self) -> &'static str {
        match self {
            BinaryKind::Runtime => "runtime",
            BinaryKind::Migration => "migration",
        }
    }
}

/// Default S3-backed fetcher. Reads the bucket name from
/// `WASM_BINARIES_BUCKET`. Keys are content-addressed:
/// `<appId>/<runtime|migration>/<sha>.wasm`.
pub struct S3BinaryFetcher {
    s3: aws_sdk_s3::Client,
    bucket: String,
}

impl S3BinaryFetcher {
    pub fn new(s3: aws_sdk_s3::Client, bucket: impl Into<String>) -> Self {
        Self {
            s3,
            bucket: bucket.into(),
        }
    }

    pub fn from_env(s3: aws_sdk_s3::Client) -> Self {
        let bucket = std::env::var("WASM_BINARIES_BUCKET")
            .unwrap_or_else(|_| "mijnspel-backend-dev-binaries".to_string());
        Self::new(s3, bucket)
    }
}

#[async_trait::async_trait]
impl BinaryFetcher for S3BinaryFetcher {
    async fn fetch(&self, app_id: &str, kind: BinaryKind, sha: &str) -> Result<Vec<u8>, WasmError> {
        let key = format!("{}/{}/{}.wasm", app_id, kind.segment(), sha);
        let resp = self
            .s3
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| WasmError::Fetch(format!("s3 get {}: {e:?}", key)))?;
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| WasmError::Fetch(format!("s3 read {}: {e:?}", key)))?;
        let bytes = body.into_bytes().to_vec();
        verify_sha(&bytes, sha)?;
        Ok(bytes)
    }
}

/// Disk-backed fetcher used by `local-backend` so devs can drop binaries
/// into `./local-wasm/<appId>/{runtime,migration}.wasm` without standing
/// up LocalStack S3. Sha verification still runs — `dev-up.sh` computes
/// the expected sha when seeding `app-registry`, so a stale on-disk blob
/// surfaces the same error as a stale S3 object.
pub struct LocalDiskBinaryFetcher {
    root: std::path::PathBuf,
}

impl LocalDiskBinaryFetcher {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait::async_trait]
impl BinaryFetcher for LocalDiskBinaryFetcher {
    async fn fetch(&self, app_id: &str, kind: BinaryKind, sha: &str) -> Result<Vec<u8>, WasmError> {
        let path = self
            .root
            .join(app_id)
            .join(format!("{}.wasm", kind.segment()));
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            WasmError::Fetch(format!("local read {}: {e}", path.display()))
        })?;
        verify_sha(&bytes, sha)?;
        Ok(bytes)
    }
}

fn verify_sha(bytes: &[u8], expected: &str) -> Result<(), WasmError> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(WasmError::Fetch(format!(
            "sha mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

/// Top-level loader. Resolves runtime/migration binaries via a
/// `BinaryFetcher`, compiles them with wasmtime, and caches the compiled
/// module per `(appId, sha)`.
pub struct WasmLoader {
    engine: Engine,
    cache: Arc<ModuleCache>,
    fetcher: Arc<dyn BinaryFetcher>,
}

impl WasmLoader {
    pub fn new(fetcher: Arc<dyn BinaryFetcher>) -> Result<Self, WasmError> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(false);
        config.epoch_interruption(false);
        config.wasm_bulk_memory(true);
        config.wasm_reference_types(true);
        config.wasm_multi_value(true);
        config.wasm_simd(true);
        config.wasm_tail_call(true);
        let engine = Engine::new(&config)
            .map_err(|e| WasmError::Compile(format!("wasmtime engine: {e:?}")))?;
        Ok(Self {
            engine,
            cache: Arc::new(ModuleCache::new()),
            fetcher,
        })
    }

    pub fn cache(&self) -> Arc<ModuleCache> {
        self.cache.clone()
    }

    pub async fn load(
        &self,
        app_id: &str,
        kind: BinaryKind,
        sha: &str,
    ) -> Result<CachedModule, WasmError> {
        if let Some(cached) = self.cache.get(&loaded_key(app_id, kind), sha) {
            return Ok(cached);
        }
        let bytes = self.fetcher.fetch(app_id, kind, sha).await?;
        let module = Module::from_binary(&self.engine, &bytes)
            .map_err(|e| WasmError::Compile(format!("compile {kind:?}/{sha}: {e:?}")))?;
        let entry = CachedModule {
            engine: self.engine.clone(),
            module,
        };
        self.cache
            .insert(&loaded_key(app_id, kind), sha, entry.clone());
        Ok(entry)
    }
}

fn loaded_key(app_id: &str, kind: BinaryKind) -> String {
    format!("{}#{}", app_id, kind.segment())
}

/// Per-call instance built from a cached module. Sandboxed: zero host imports
/// beyond a no-op host function table, so the guest cannot reach DDB even if
/// it tries.
pub struct Instance {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    instance: wasmtime::Instance,
}

impl Instance {
    pub fn new(cached: &CachedModule) -> Result<Self, WasmError> {
        let mut linker: Linker<()> = Linker::new(&cached.engine);

        // Provide a single `host_log` import so the guest can emit a
        // build-id / debug breadcrumb without being able to reach the
        // network. Hot-swap tests rely on this to verify which sha served
        // the request.
        linker
            .func_wrap(
                "env",
                "host_log",
                |mut caller: Caller<'_, ()>, ptr: u32, len: u32| {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let data = mem.data(&caller);
                        if let (Some(start), Some(end)) =
                            ((ptr as usize).into(), (ptr as usize + len as usize).into())
                        {
                            if let Some(slice) = data.get(start..end) {
                                if let Ok(text) = std::str::from_utf8(slice) {
                                    println!("wasm-guest-log: {text}");
                                }
                            }
                        }
                    }
                },
            )
            .ok();

        let mut store = Store::new(&cached.engine, ());
        let instance = linker
            .instantiate(&mut store, &cached.module)
            .map_err(|e| WasmError::Instantiate(format!("{e:?}")))?;

        let memory = instance
            .get_memory(&mut store, memory_exports::MEMORY)
            .ok_or_else(|| WasmError::Memory("guest exports no `memory`".to_string()))?;
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut store, memory_exports::ALLOC)
            .map_err(|e| WasmError::Instantiate(format!("__alloc missing: {e:?}")))?;
        let free = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, memory_exports::FREE)
            .map_err(|e| WasmError::Instantiate(format!("__free missing: {e:?}")))?;

        Ok(Self {
            store,
            memory,
            alloc,
            free,
            instance,
        })
    }

    /// Invoke an export named `name` with a JSON-shaped `HostJson { data: input }`
    /// envelope. Decodes the response envelope's `data` field as `T`.
    pub fn call_json<T: DeserializeOwned + Default>(
        &mut self,
        name: &str,
        input: Value,
    ) -> Result<T, WasmError> {
        let envelope = HostJson::new(input);
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|e| WasmError::Call(format!("encode {name}: {e:?}")))?;
        let in_len = bytes.len() as u32;
        let in_ptr = self
            .alloc
            .call(&mut self.store, in_len)
            .map_err(|e| WasmError::Call(format!("__alloc({in_len}) for {name}: {e:?}")))?;
        self.memory
            .write(&mut self.store, in_ptr as usize, &bytes)
            .map_err(|e| WasmError::Memory(format!("write {name} input: {e:?}")))?;

        let func = self
            .instance
            .get_typed_func::<(u32, u32), u64>(&mut self.store, name)
            .map_err(|e| WasmError::Call(format!("export {name} missing: {e:?}")))?;
        let packed = func
            .call(&mut self.store, (in_ptr, in_len))
            .map_err(|e| WasmError::Call(format!("call {name}: {e:?}")))?;
        // Free the input buffer the host allocated above.
        let _ = self.free.call(&mut self.store, (in_ptr, in_len));

        let (out_ptr, out_len) = unpack_ptr_len(packed);
        let mut out = vec![0u8; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut out)
            .map_err(|e| WasmError::Memory(format!("read {name} output: {e:?}")))?;
        let _ = self.free.call(&mut self.store, (out_ptr, out_len));

        let envelope: HostJson = serde_json::from_slice(&out)
            .map_err(|e| WasmError::Decode(format!("envelope {name}: {e:?}")))?;
        let value: T = serde_json::from_value(envelope.data)
            .map_err(|e| WasmError::Decode(format!("payload {name}: {e:?}")))?;
        Ok(value)
    }

    /// Returns `Ok(true)` if the named export exists with the expected
    /// `(u32, u32) -> u64` JSON-boundary signature. Used for optional
    /// exports like `process_signal` so the host can return 404 instead of
    /// failing when a guest does not implement signaling.
    pub fn has_json_export(&mut self, name: &str) -> bool {
        self.instance
            .get_typed_func::<(u32, u32), u64>(&mut self.store, name)
            .is_ok()
    }

    /// Invoke an `i64`-returning scalar export (e.g. `abi_version`,
    /// `current_schema_version`).
    pub fn call_i64(&mut self, name: &str) -> Result<i64, WasmError> {
        let func = self
            .instance
            .get_typed_func::<(), i64>(&mut self.store, name)
            .map_err(|e| WasmError::Call(format!("export {name} missing: {e:?}")))?;
        func.call(&mut self.store, ())
            .map_err(|e| WasmError::Call(format!("call {name}: {e:?}")))
    }
}

/// High-level wrapper around a runtime instance. Asserts ABI compatibility
/// on first use; callers that care about whether a request hit a particular
/// sha just look at `current_schema_version` per call.
pub struct RuntimeInstance {
    inner: Instance,
}

impl RuntimeInstance {
    pub fn new(cached: &CachedModule) -> Result<Self, WasmError> {
        let mut inner = Instance::new(cached)?;
        let abi = inner.call_i64(runtime_exports::ABI_VERSION)?;
        if abi != HOST_ABI_VERSION {
            return Err(WasmError::AbiMismatch {
                expected: HOST_ABI_VERSION,
                got: abi,
            });
        }
        Ok(Self { inner })
    }

    pub fn current_schema_version(&mut self) -> Result<i64, WasmError> {
        self.inner.call_i64(runtime_exports::CURRENT_SCHEMA_VERSION)
    }

    pub fn validate_create_request(&mut self, input: Value) -> Result<ValidateOutcome, WasmError> {
        self.inner
            .call_json(runtime_exports::VALIDATE_CREATE_REQUEST, input)
    }

    pub fn validate_update_request(&mut self, input: Value) -> Result<ValidateOutcome, WasmError> {
        self.inner
            .call_json(runtime_exports::VALIDATE_UPDATE_REQUEST, input)
    }

    pub fn validate_lobby_create(
        &mut self,
        input: Value,
    ) -> Result<ValidateOutcome, WasmError> {
        self.inner
            .call_json(runtime_exports::VALIDATE_LOBBY_CREATE, input)
    }

    pub fn lobby_policy(&mut self) -> Result<LobbyPolicy, WasmError> {
        self.inner
            .call_json(runtime_exports::LOBBY_POLICY, Value::Null)
    }

    pub fn create_initial_state(&mut self, input: Value) -> Result<Value, WasmError> {
        self.inner
            .call_json(runtime_exports::CREATE_INITIAL_STATE, input)
    }

    pub fn state_for_lobby_start(&mut self, input: Value) -> Result<Value, WasmError> {
        self.inner
            .call_json(runtime_exports::STATE_FOR_LOBBY_START, input)
    }

    pub fn prior_stat_ids(&mut self) -> Result<PriorStatIds, WasmError> {
        self.inner
            .call_json(runtime_exports::PRIOR_STAT_IDS, Value::Null)
    }

    pub fn process_update(&mut self, input: Value) -> Result<ProcessUpdateOutput, WasmError> {
        self.inner.call_json(runtime_exports::PROCESS_UPDATE, input)
    }

    pub fn leaderboard_stat_ids(&mut self) -> Result<LeaderboardStatIds, WasmError> {
        self.inner
            .call_json(runtime_exports::LEADERBOARD_STAT_IDS, Value::Null)
    }

    /// Extract the list of player ids embedded in a game row's per-game
    /// fields (e.g. gunzi's `blackSub` / `whiteSub`).
    pub fn players_in_row(&mut self, row: Value) -> Result<PlayersInRow, WasmError> {
        self.inner
            .call_json(runtime_exports::PLAYERS_IN_ROW, row)
    }

    /// Optional. Returns `Ok(None)` if the loaded guest does not export
    /// `process_signal` (signaling unsupported for this appId — the
    /// `/games/{id}/signal` route turns this into a 404).
    pub fn process_signal(
        &mut self,
        input: Value,
    ) -> Result<Option<SignalOutcome>, WasmError> {
        if !self.inner.has_json_export(runtime_exports::PROCESS_SIGNAL) {
            return Ok(None);
        }
        let outcome = self
            .inner
            .call_json(runtime_exports::PROCESS_SIGNAL, input)?;
        Ok(Some(outcome))
    }
}

/// High-level wrapper around a migration instance.
pub struct MigrationInstance {
    inner: Instance,
}

impl MigrationInstance {
    pub fn new(cached: &CachedModule) -> Result<Self, WasmError> {
        let mut inner = Instance::new(cached)?;
        let abi = inner.call_i64(migration_exports::ABI_VERSION)?;
        if abi != HOST_ABI_VERSION {
            return Err(WasmError::AbiMismatch {
                expected: HOST_ABI_VERSION,
                got: abi,
            });
        }
        Ok(Self { inner })
    }

    pub fn current_schema_version(&mut self) -> Result<i64, WasmError> {
        self.inner
            .call_i64(migration_exports::CURRENT_SCHEMA_VERSION)
    }

    pub fn min_supported_version(&mut self) -> Result<i64, WasmError> {
        self.inner.call_i64(migration_exports::MIN_SUPPORTED_VERSION)
    }

    /// Walk a state from `version - 1` to `version`.
    pub fn upgrade_from_previous(&mut self, version: i64, state: Value) -> Result<Value, WasmError> {
        let payload = serde_json::json!({ "version": version, "state": state });
        self.inner
            .call_json(migration_exports::UPGRADE_FROM_PREVIOUS, payload)
    }

    /// Walk a state from `version` to `version - 1`.
    pub fn downgrade_to_previous(&mut self, version: i64, state: Value) -> Result<Value, WasmError> {
        let payload = serde_json::json!({ "version": version, "state": state });
        self.inner
            .call_json(migration_exports::DOWNGRADE_TO_PREVIOUS, payload)
    }
}
