// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Live state reads and the SMT state-commitment layer.
//!
//! Point reads (`balance_of`, `nonce_of`, `code_of`, `storage_of`) serve
//! JSON-RPC directly; every state transition rebuilds the sparse Merkle tree
//! commitment from the full revm cache (dev-scale full rebuilds stay correct
//! under every transition, including selfdestructs, without dirty tracking).

use super::node::{KanariNode, NodeError};
use alloy_primitives::{Address, B256, Bytes, U256};
use revm::database_interface::Database;

impl KanariNode {
    /// Balance of an account in the current state.
    pub fn balance_of(&mut self, address: Address) -> Result<U256, NodeError> {
        Ok(self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
            .map(|info| info.balance)
            .unwrap_or(U256::ZERO))
    }

    /// Transaction count (nonce) of an account in the current state.
    pub fn nonce_of(&mut self, address: Address) -> Result<u64, NodeError> {
        Ok(self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
            .map(|info| info.nonce)
            .unwrap_or(0))
    }

    /// Deployed bytecode of an account, empty for EOAs and absent accounts.
    pub fn code_of(&mut self, address: Address) -> Result<Bytes, NodeError> {
        let Some(info) = self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
        else {
            return Ok(Bytes::new());
        };
        if info.code_hash == revm::primitives::KECCAK_EMPTY {
            return Ok(Bytes::new());
        }
        if let Some(code) = info.code {
            return Ok(code.original_bytes());
        }
        Ok(self
            .db
            .cache
            .contracts
            .get(&info.code_hash)
            .map(|code| code.original_bytes())
            .unwrap_or_default())
    }

    /// Storage value of an account slot (`U256::ZERO` when absent).
    /// Serves `eth_getStorageAt`.
    pub fn storage_of(&mut self, address: Address, slot: U256) -> Result<U256, NodeError> {
        self.db
            .storage(address, slot)
            .map_err(|e| NodeError::Storage(e.to_string()))
    }

    /// Current SMT state root.
    pub fn state_root(&self) -> Result<B256, NodeError> {
        let root = self
            .smt
            .root_hash()
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(B256::from_slice(&root))
    }

    /// Prove an account (or one of its storage slots) against the current
    /// state root. Returns `None` only on storage errors, never on absence:
    /// absent keys yield non-membership proofs (`exists: false`).
    pub fn smt_proof(&self, address: Address, slot: Option<U256>) -> Result<SmtProof, NodeError> {
        let key = match slot {
            Some(s) => smt_storage_key(&address, &s),
            None => smt_account_key(&address),
        };
        let value = self
            .smt
            .get(&key)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        let (exists, leaf, siblings) = self
            .smt
            .proof(&key)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(SmtProof {
            root: self.state_root()?,
            key: B256::from_slice(&key),
            value,
            exists,
            leaf: B256::from_slice(&leaf),
            siblings: siblings.into_iter().map(|s| B256::from_slice(&s)).collect(),
        })
    }

    /// Rebuild the SMT state commitment from the full live revm state.
    /// Dev-scale full rebuilds stay correct under every state transition
    /// (including selfdestructs) without dirty-set tracking.
    pub(crate) fn rebuild_smt(&self) -> Result<B256, NodeError> {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (address, account) in self.db.cache.accounts.iter() {
            let info = &account.info;
            if info.balance.is_zero()
                && info.nonce == 0
                && info.code_hash == revm::primitives::KECCAK_EMPTY
            {
                continue;
            }
            entries.push((smt_account_key(address).to_vec(), encode_account(info)));
            for (slot, value) in account.storage.iter() {
                entries.push((
                    smt_storage_key(address, slot).to_vec(),
                    value.to_be_bytes::<32>().to_vec(),
                ));
            }
        }
        self.smt
            .rebuild(&entries)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        let root = self
            .smt
            .root_hash()
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(B256::from_slice(&root))
    }
}

/// Domain tag for the Kanari EVM state commitment key derivation.
const SMT_DOMAIN: &[u8] = b"kanari-evm-smt-v1";

/// SMT key for an account: `BLAKE3(domain || 0x00 || address)`.
fn smt_account_key(address: &Address) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(SMT_DOMAIN.len() + 1 + 20);
    preimage.extend_from_slice(SMT_DOMAIN);
    preimage.push(0x00);
    preimage.extend_from_slice(address.as_slice());
    kanari_crypto::hash_data_blake3_array(&preimage)
}

/// SMT key for a storage slot: `BLAKE3(domain || 0x01 || address || slot)`.
fn smt_storage_key(address: &Address, slot: &U256) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(SMT_DOMAIN.len() + 1 + 20 + 32);
    preimage.extend_from_slice(SMT_DOMAIN);
    preimage.push(0x01);
    preimage.extend_from_slice(address.as_slice());
    preimage.extend_from_slice(&slot.to_be_bytes::<32>());
    kanari_crypto::hash_data_blake3_array(&preimage)
}

/// SMT value for an account: `balance_be32 || nonce_be8 || code_hash`.
fn encode_account(info: &revm::state::AccountInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(72);
    out.extend_from_slice(&info.balance.to_be_bytes::<32>());
    out.extend_from_slice(&info.nonce.to_be_bytes());
    out.extend_from_slice(info.code_hash.as_slice());
    out
}

/// An SMT inclusion/non-inclusion proof over the current state.
#[derive(Debug, Clone)]
pub struct SmtProof {
    /// Current state root the proof is valid against.
    pub root: B256,
    /// SMT key that was proven.
    pub key: B256,
    /// Committed value, or `None` for non-membership proofs.
    pub value: Option<Vec<u8>>,
    /// True for membership proofs.
    pub exists: bool,
    /// Leaf hash covered by the proof (feed to `smt::verify_proof`).
    pub leaf: B256,
    /// Sibling hashes, bottom-up.
    pub siblings: Vec<B256>,
}
