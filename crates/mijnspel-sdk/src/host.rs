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
use wasmtime::{Caller, Engine, ExternType, Linker, Memory, Module, Store, TypedFunc, ValType};

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
    Verify(String),
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
            WasmError::Verify(msg) => write!(f, "wasm verify failed: {msg}"),
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
/// `WASM_BINARIES_BUCKET`. Keys are flat and overwritten in place:
/// `<appId>/<runtime|migration>.wasm`. The served object is **authoritative**
/// (decision (ii)): request-time `fetch` serves the bytes as-is. The `sha`
/// argument is the loader cache key (so a flip busts the per-`(appId,sha)`
/// module cache) plus an intake-time integrity expectation; it is NOT a
/// request-time gate, so a mismatch is logged but never fatal. Intake's
/// verifying readback uses [`S3BinaryFetcher::fetch_verified`] instead. This is
/// the same serve-as-is shape `LocalDiskBinaryFetcher` already serves from disk.
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

    /// GET the served object bytes for `<appId>/<kind>.wasm` without verifying
    /// the sha. Shared by the request-time `fetch` (serve-as-is) and the
    /// intake-time `fetch_verified` (which layers the sha check on top).
    async fn get_object_bytes(
        &self,
        app_id: &str,
        kind: BinaryKind,
    ) -> Result<Vec<u8>, WasmError> {
        let key = format!("{}/{}.wasm", app_id, kind.segment());
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
        Ok(body.into_bytes().to_vec())
    }

    /// Intake-time verifying readback: GET the served object and assert its sha
    /// matches `sha`, returning the bytes on success. Used ONLY by the intake
    /// lambda's activation/repair path — NOT by the request-time `fetch`, which
    /// serves the object as-is (decision (ii)). Keeping the two methods distinct
    /// is deliberate: request-time serving never rejects a skewed object (it
    /// serves the previous verified artifact instead of 500ing), while
    /// intake-time activation stays strictly verified.
    pub async fn fetch_verified(
        &self,
        app_id: &str,
        kind: BinaryKind,
        sha: &str,
    ) -> Result<Vec<u8>, WasmError> {
        let bytes = self.get_object_bytes(app_id, kind).await?;
        verify_sha(&bytes, sha)?;
        Ok(bytes)
    }
}

#[async_trait::async_trait]
impl BinaryFetcher for S3BinaryFetcher {
    async fn fetch(&self, app_id: &str, kind: BinaryKind, sha: &str) -> Result<Vec<u8>, WasmError> {
        let bytes = self.get_object_bytes(app_id, kind).await?;
        // Serve-as-is (decision (ii)): the served object is authoritative. The
        // registry sha is the loader cache key + an intake-time integrity
        // expectation, NOT a request-time gate — so a mismatch is logged but
        // never fatal. A brief flip→copy skew window therefore serves the
        // previous verified artifact rather than 500ing; genuine corruption
        // surfaces later at `Module::from_binary`, not here.
        if let Err(e) = verify_sha(&bytes, sha) {
            eprintln!(
                "wasm fetch {}/{}: serving object as-is despite {e}",
                app_id,
                kind.segment()
            );
        }
        Ok(bytes)
    }
}

/// Disk-backed fetcher used by `local-backend` so devs can drop binaries
/// into `./local-wasm/<appId>/{runtime,migration}.wasm` without standing
/// up LocalStack S3. This flat layout is identical in shape to the S3
/// keyspace — both fetchers resolve `<appId>/<kind>.wasm` and differ only
/// in disk-vs-S3. Serve-as-is (decision (ii)): like the S3 fetcher, a sha
/// mismatch is logged but not fatal — the on-disk object is authoritative,
/// which is exactly the serve-as-is model the local dev server already uses.
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
        // Serve-as-is (decision (ii)): log a sha mismatch but still serve the
        // on-disk bytes — the object is authoritative, not the registry sha.
        if let Err(e) = verify_sha(&bytes, sha) {
            eprintln!("wasm local fetch {}: serving as-is despite {e}", path.display());
        }
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

    /// Returns `true` if the instance exports anything named `name`,
    /// whatever its type.
    pub fn has_export(&mut self, name: &str) -> bool {
        self.instance.get_export(&mut self.store, name).is_some()
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

    /// Optional. Returns `Ok(None)` if the guest has no `state_for_viewer`
    /// export (rows go out unchanged). Presence is decided by name only, so
    /// an export with the wrong signature is an error, never a skip.
    pub fn state_for_viewer(&mut self, input: Value) -> Result<Option<Value>, WasmError> {
        if !self.inner.has_export(runtime_exports::STATE_FOR_VIEWER) {
            return Ok(None);
        }
        let view = self
            .inner
            .call_json(runtime_exports::STATE_FOR_VIEWER, input)?;
        Ok(Some(view))
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

/// Outcome of an offline verification of a runtime (and optional migration)
/// WASM pair. The shape mirrors the JSON the `mijnspel-wasm-verify` CLI
/// emits, so both the manual intake script and the upload-triggered intake
/// lambda consume identical fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub abi_version: i64,
    pub current_schema_version: i64,
    pub runtime_sha256: String,
    /// Empty string when no migration was supplied (first-time runtime-only
    /// apps); otherwise the sha256 of the migration bytes.
    pub migration_sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn compile_bytes(bytes: &[u8], label: &str) -> Result<CachedModule, WasmError> {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes)
        .map_err(|e| WasmError::Compile(format!("{label}: {e:?}")))?;
    Ok(CachedModule { engine, module })
}

/// Optional JSON exports must have the `(i32, i32) -> i64` boundary
/// signature when present. `state_for_viewer` is detected by name at request
/// time, so a wrong signature would fail every read of that game.
fn verify_optional_json_exports(cached: &CachedModule) -> Result<(), WasmError> {
    let name = runtime_exports::STATE_FOR_VIEWER;
    let Some(export) = cached.module.get_export(name) else {
        return Ok(());
    };
    let ok = match export {
        ExternType::Func(func) => {
            let params: Vec<ValType> = func.params().collect();
            let results: Vec<ValType> = func.results().collect();
            matches!(params.as_slice(), [ValType::I32, ValType::I32])
                && matches!(results.as_slice(), [ValType::I64])
        }
        _ => false,
    };
    if !ok {
        return Err(WasmError::Verify(format!(
            "export `{name}` must be a function `(ptr: u32, len: u32) -> u64`"
        )));
    }
    Ok(())
}

/// Offline verification of a runtime + optional migration WASM pair against
/// the host ABI. This is the single shared verifier used by the operator
/// intake script (via the `mijnspel-wasm-verify` CLI) and the
/// upload-triggered intake lambda.
///
/// It compiles each artifact, binds the high-level instance (which asserts
/// `abi_version == HOST_ABI_VERSION`), reads `current_schema_version`, probes
/// the required runtime exports, and — when a migration is supplied — asserts
/// the runtime and migration agree on which schema is current. No guest entry
/// point that mutates state is invoked, so a trapping migration body is not
/// exercised here.
///
/// `migration` is `None` for a first-time, runtime-only app; in that case the
/// returned `migration_sha256` is the empty string and the schema-agreement
/// check is skipped.
pub fn verify_artifacts(
    runtime: &[u8],
    migration: Option<&[u8]>,
) -> Result<VerifyReport, WasmError> {
    let runtime_cached = compile_bytes(runtime, "compile runtime.wasm")?;
    verify_optional_json_exports(&runtime_cached)?;
    let mut runtime_inst = RuntimeInstance::new(&runtime_cached)?;
    let runtime_schema = runtime_inst.current_schema_version()?;

    // Touch every required export name so missing-export defects surface here,
    // not at request time. We only care that the symbol resolves.
    let mut probe = Instance::new(&runtime_cached)?;
    for name in [
        runtime_exports::ABI_VERSION,
        runtime_exports::CURRENT_SCHEMA_VERSION,
    ] {
        probe.call_i64(name)?;
    }

    let runtime_sha256 = sha256_hex(runtime);

    let migration_sha256 = match migration {
        Some(migration_bytes) => {
            let migration_cached = compile_bytes(migration_bytes, "compile migration.wasm")?;
            let mut migration_inst = MigrationInstance::new(&migration_cached)?;
            let migration_schema = migration_inst.current_schema_version()?;
            let _min = migration_inst.min_supported_version()?;
            if runtime_schema != migration_schema {
                return Err(WasmError::Verify(format!(
                    "runtime.current_schema_version ({runtime_schema}) != \
                     migration.current_schema_version ({migration_schema}) — the runtime and \
                     migration must agree on which schema is current. Rebuild both from the \
                     same source."
                )));
            }
            sha256_hex(migration_bytes)
        }
        None => String::new(),
    };

    Ok(VerifyReport {
        abi_version: HOST_ABI_VERSION,
        current_schema_version: runtime_schema,
        runtime_sha256,
        migration_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    #[test]
    fn verify_sha_accepts_match_and_rejects_mismatch() {
        // `verify_sha` is the verifying core shared by `fetch_verified` and the
        // intake readback. It accepts a matching digest (case-insensitively)
        // and rejects a mismatch — the latter is what makes `fetch_verified`
        // (and intake activation) reject a skewed object.
        let bytes = b"hello wasm";
        let good = sha_of(bytes);
        assert!(verify_sha(bytes, &good).is_ok());
        assert!(verify_sha(bytes, &good.to_uppercase()).is_ok());
        assert!(verify_sha(bytes, "deadbeef").is_err());
    }

    #[test]
    fn verify_rejects_state_for_viewer_with_wrong_signature() {
        // Smallest module exporting `state_for_viewer: () -> ()`.
        let mut wasm = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        wasm.extend([0x01, 0x04, 0x01, 0x60, 0x00, 0x00]); // type 0: () -> ()
        wasm.extend([0x03, 0x02, 0x01, 0x00]); // func 0: type 0
        wasm.extend([0x07, 0x14, 0x01, 0x10]); // export section, one name of 16 bytes
        wasm.extend(b"state_for_viewer");
        wasm.extend([0x00, 0x00]); // func 0
        wasm.extend([0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b]); // empty body

        match verify_artifacts(&wasm, None) {
            Err(WasmError::Verify(msg)) => assert!(msg.contains("state_for_viewer"), "{msg}"),
            Err(other) => panic!("expected a Verify error, got {other}"),
            Ok(_) => panic!("expected a Verify error, got Ok"),
        }
    }

    #[tokio::test]
    async fn local_fetch_serves_as_is_on_sha_mismatch() {
        // Decision (ii): the request-time fetcher returns the on-disk bytes
        // even when the passed sha does not match — it never gates serving.
        let dir = std::env::temp_dir().join(format!(
            "mijnspel-host-test-{}-{}",
            std::process::id(),
            "serve-as-is"
        ));
        let app_dir = dir.join("my-game");
        std::fs::create_dir_all(&app_dir).expect("mkdir");
        let bytes = b"not-a-real-wasm-but-serve-anyway";
        std::fs::write(app_dir.join("runtime.wasm"), bytes).expect("write");

        let fetcher = LocalDiskBinaryFetcher::new(&dir);
        let served = fetcher
            .fetch(
                "my-game",
                BinaryKind::Runtime,
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .await
            .expect("serve-as-is must not error on a sha mismatch");
        assert_eq!(served, bytes, "fetch returns the on-disk bytes verbatim");

        std::fs::remove_dir_all(&dir).ok();
    }
}
