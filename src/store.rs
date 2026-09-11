// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Durable chain storage on `kanari-db-common` (RocksDB).
//!
//! Layout (single RocksDB, default column family, prefixed keys so the SMT
//! layer — which owns `n:` / `d:` prefixes plus its root keys — never
//! collides with chain data):
//!
//! ```text
//! m:chain_id      -> u64 LE — chain this database belongs to
//! m:genesis       -> JSON Vec<(Address, U256)> — genesis allocations
//! m:height        -> u64 LE — latest sealed block number
//! m:faucet_key    -> 32 raw bytes — dev faucet secret (DEV ONLY)
//! b:{n:016x}      -> JSON SealedBlock — fixed-width hex keeps byte order
//! r:{txhash_hex}  -> JSON StoredReceipt
//! t:{txhash_hex}  -> u64 LE block number holding the transaction
//! ```
//!
//! A block seal (block + receipts + tx index + height) commits atomically in
//! one [`rocksdb::WriteBatch`]. Values are JSON for debuggability; this is a
//! dev chain, not a high-throughput validator store.

use crate::node::{SealedBlock, StoredReceipt};
use alloy_primitives::{Address, B256, U256};
use rocksdb::WriteBatch;
use std::{path::Path, sync::Arc};
/// Precompile-free key helpers live here so the layout stays in one place.
mod keys {
    use alloy_primitives::B256;

    pub const META_CHAIN_ID: &[u8] = b"m:chain_id";
    pub const META_GENESIS: &[u8] = b"m:genesis";
    pub const META_HEIGHT: &[u8] = b"m:height";
    pub const META_FAUCET_KEY: &[u8] = b"m:faucet_key";

    pub fn block_key(number: u64) -> Vec<u8> {
        format!("b:{number:016x}").into_bytes()
    }

    pub fn receipt_key(hash: &B256) -> Vec<u8> {
        let mut key = Vec::with_capacity(2 + 64);
        key.extend_from_slice(b"r:");
        key.extend_from_slice(hex_of(&hash.0).as_bytes());
        key
    }

    pub fn tx_index_key(hash: &B256) -> Vec<u8> {
        let mut key = Vec::with_capacity(2 + 64);
        key.extend_from_slice(b"t:");
        key.extend_from_slice(hex_of(&hash.0).as_bytes());
        key
    }

    fn hex_of(bytes: &[u8]) -> String {
        const CHARS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(CHARS[(b >> 4) as usize] as char);
            out.push(CHARS[(b & 0x0f) as usize] as char);
        }
        out
    }
}

/// Durable chain storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Backend(String),
    #[error("chain id mismatch: database holds {0}, node wants {1}")]
    ChainMismatch(u64, u64),
}

type Result<T> = std::result::Result<T, StoreError>;

/// Chain data store. Clonable handle over a shared RocksDB (`Arc`).
#[derive(Debug, Clone)]
pub struct ChainStore {
    db: Arc<rocksdb::DB>,
}

impl ChainStore {
    /// Open (or create) the store at `dir` via the shared Kanari opener.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let db = kanari_db_common::open_or_get_db(Some(dir.as_ref().to_path_buf()))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(Self { db })
    }

    /// Raw handle for sharing with the SMT layer (same DB, disjoint keys).
    pub fn shared_db(&self) -> Arc<rocksdb::DB> {
        self.db.clone()
    }

    /// True when no block has ever been sealed here.
    pub fn is_fresh(&self) -> Result<bool> {
        Ok(self.get_u64(keys::META_HEIGHT)?.is_none())
    }

    /// Seal a block atomically: block + receipts + tx index + height.
    pub fn save_block(
        &self,
        block: &SealedBlock,
        receipts: &[(&B256, &StoredReceipt)],
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        let block_bytes =
            serde_json::to_vec(block).map_err(|e| StoreError::Backend(e.to_string()))?;
        batch.put(keys::block_key(block.number), block_bytes);
        for (hash, receipt) in receipts {
            let receipt_bytes =
                serde_json::to_vec(receipt).map_err(|e| StoreError::Backend(e.to_string()))?;
            batch.put(keys::receipt_key(hash), receipt_bytes);
            batch.put(keys::tx_index_key(hash), block.number.to_le_bytes());
        }
        batch.put(keys::META_HEIGHT, block.number.to_le_bytes());
        self.db
            .write(batch)
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    /// All sealed blocks in number order (dev-scale full scan for replay).
    pub fn load_blocks(&self) -> Result<Vec<SealedBlock>> {
        let height = self.get_u64(keys::META_HEIGHT)?.unwrap_or(0);
        let mut out = Vec::new();
        for number in 1..=height {
            if let Some(block) = self.load_block(number)? {
                out.push(block);
            }
        }
        Ok(out)
    }

    /// Load one sealed block by number.
    pub fn load_block(&self, number: u64) -> Result<Option<SealedBlock>> {
        let raw = self
            .db
            .get(keys::block_key(number))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| StoreError::Backend(e.to_string())),
        }
    }

    /// Load a receipt by transaction hash.
    pub fn load_receipt(&self, hash: &B256) -> Result<Option<StoredReceipt>> {
        let raw = self
            .db
            .get(keys::receipt_key(hash))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| StoreError::Backend(e.to_string())),
        }
    }

    /// Block number holding a transaction, if any.
    pub fn block_number_for_tx(&self, hash: &B256) -> Result<Option<u64>> {
        let raw = self
            .db
            .get(keys::tx_index_key(hash))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) if bytes.len() == 8 => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(Some(u64::from_le_bytes(arr)))
            }
            Some(_) => Err(StoreError::Backend("corrupt tx index entry".to_string())),
        }
    }

    /// Latest sealed block number, 0 when empty.
    pub fn height(&self) -> Result<u64> {
        Ok(self.get_u64(keys::META_HEIGHT)?.unwrap_or(0))
    }

    /// Read a meta u64 value.
    fn get_u64(&self, key: &[u8]) -> Result<Option<u64>> {
        let raw = self
            .db
            .get(key)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) if bytes.len() == 8 => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(Some(u64::from_le_bytes(arr)))
            }
            Some(_) => Err(StoreError::Backend("corrupt meta entry".to_string())),
        }
    }

    /// Initialize chain metadata once (chain id + genesis alloc).
    pub fn init_meta(&self, chain_id: u64, genesis_alloc: &[(Address, U256)]) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.put(keys::META_CHAIN_ID, chain_id.to_le_bytes());
        let alloc_bytes =
            serde_json::to_vec(genesis_alloc).map_err(|e| StoreError::Backend(e.to_string()))?;
        batch.put(keys::META_GENESIS, alloc_bytes);
        self.db
            .write(batch)
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    /// Stored chain id, if initialized.
    pub fn stored_chain_id(&self) -> Result<Option<u64>> {
        self.get_u64(keys::META_CHAIN_ID)
    }

    /// Stored genesis allocations.
    pub fn stored_genesis_alloc(&self) -> Result<Vec<(Address, U256)>> {
        let raw = self
            .db
            .get(keys::META_GENESIS)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(Vec::new()),
            Some(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| StoreError::Backend(e.to_string()))
            }
        }
    }

    /// Store the dev faucet secret (DEV ONLY).
    pub fn set_faucet_key(&self, secret: &B256) -> Result<()> {
        self.db
            .put(keys::META_FAUCET_KEY, secret.as_slice())
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    /// Load the dev faucet secret, if one was stored.
    pub fn load_faucet_key(&self) -> Result<Option<B256>> {
        let raw = self
            .db
            .get(keys::META_FAUCET_KEY)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Ok(Some(B256::from_slice(&arr)))
            }
            Some(_) => Err(StoreError::Backend("corrupt faucet key".to_string())),
        }
    }
}
