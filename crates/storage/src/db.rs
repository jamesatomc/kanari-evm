// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Local RocksDB opener: one shared instance per normalized path.
//!
//! Split out from the `kanari-db-common` dependency (whose full opener
//! carries high-throughput tuning knobs this dev chain does not need).
//! Only correctness-critical behavior is preserved:
//!
//! - relative paths resolve against the process working directory,
//! - repeated opens of one path reuse the same `Arc<DB>` (RocksDB takes a
//!   directory lock — opening the same path twice without sharing fails),
//! - parent directories are created on demand.
//!
//! If benchmarks ever demand it, re-add block-cache/bloom tuning here.

use crate::store::StoreError;
use rocksdb::{DB, Options};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
};

type Result<T> = std::result::Result<T, StoreError>;

static OPEN_DATABASES: OnceLock<Mutex<HashMap<std::path::PathBuf, Weak<DB>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<std::path::PathBuf, Weak<DB>>> {
    OPEN_DATABASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open (or reuse) the RocksDB at `dir` for chain data.
pub fn open_or_get_db(dir: impl AsRef<Path>) -> Result<Arc<DB>> {
    let mut path = dir.as_ref().to_path_buf();
    if path.is_relative() {
        path = std::env::current_dir()
            .map_err(|e| StoreError::Backend(format!("failed to resolve cwd for RocksDB: {e}")))
            .map(|cwd| cwd.join(&path))?;
    }

    {
        let mut registry = registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|_, db| db.strong_count() > 0);
        if let Some(db) = registry.get(&path).and_then(Weak::upgrade) {
            return Ok(db);
        }
    }

    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|e| StoreError::Backend(format!("failed to create RocksDB parent directory: {e}")))?;

    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db = DB::open(&opts, &path)
        .map_err(|e| StoreError::Backend(format!("Failed to open RocksDB for kanari: {e}")))?;
    let db = Arc::new(db);

    {
        let mut registry = registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.insert(path, Arc::downgrade(&db));
    }
    Ok(db)
}
