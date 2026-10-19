//! Per-(appId, sha) compiled-module cache.
//!
//! Entries are immutable for the lifetime of the lambda — the key already
//! pins the content sha, so an entry can never be stale relative to its key.
//! A registry pointer flip points new requests at a different `(appId, sha)`
//! cache slot, so neither up- nor down-flips need entry expiry.

use std::collections::HashMap;
use std::sync::Mutex;

use wasmtime::{Engine, Module};

#[derive(Clone)]
pub struct CachedModule {
    pub engine: Engine,
    pub module: Module,
}

impl CachedModule {
    /// Whether the compiled module exports anything under `name`, whatever
    /// its type. Optional hooks whose absence would widen what the host
    /// sends (`state_for_viewer`) are detected this way, so an export with
    /// the wrong signature fails the call instead of being skipped.
    pub fn has_export(&self, name: &str) -> bool {
        self.module.get_export(name).is_some()
    }
}

pub struct ModuleCache {
    inner: Mutex<HashMap<(String, String), CachedModule>>,
}

impl ModuleCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, app_id: &str, sha: &str) -> Option<CachedModule> {
        let guard = self.inner.lock().ok()?;
        guard.get(&(app_id.to_string(), sha.to_string())).cloned()
    }

    pub fn insert(&self, app_id: &str, sha: &str, entry: CachedModule) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert((app_id.to_string(), sha.to_string()), entry);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

impl Default for ModuleCache {
    fn default() -> Self {
        Self::new()
    }
}
