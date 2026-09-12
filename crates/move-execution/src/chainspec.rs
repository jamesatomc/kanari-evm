// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM chain specification and genesis.
//!
//! A fresh Kanari devnet activates the newest hardfork at genesis (no fork
//! history to replay). PQC verification precompiles (Falcon/Dilithium, à la
//! EIP-8052/8053) will be registered as custom precompiles on top of this
//! spec in a follow-up phase.

use revm::{
    bytecode::Bytecode,
    database::InMemoryDB,
    primitives::{Address, Bytes, U256, address, hardfork::SpecId},
    state::AccountInfo,
};

/// Kanari EVM devnet chain id.
pub const KANARI_EVM_DEV_CHAIN_ID: u64 = 19088;

/// Hardfork active at genesis on a fresh Kanari chain.
pub const KANARI_EVM_GENESIS_SPEC: SpecId = SpecId::AMSTERDAM;

/// Well-known funded dev account (Anvil default #0).
pub const DEV_FUNDED_ACCOUNT: Address = address!("0xc88c539aa6f67daedaea7aff75fe1f8848d6cec2");

/// Genesis balance of the funded dev account: 11_000_000 ETH.
pub const DEV_FUNDED_BALANCE: u128 = 11_000_000_000_000_000_000_000_000;

/// Protocol max supply: 11_000_000 ETH. No minting exists after genesis
/// (block fees only move value to the block beneficiary),
/// so circulating supply is always the genesis-allocation sum below.
pub const KANARI_EVM_MAX_SUPPLY_ETH: u128 = 11_000_000;

/// Genesis base fee (wei): 1 gwei, the flat fee every chain starts from
/// before EIP-1559 dynamics take over. Single source in `evm-types`.
pub use kanari_evm_types::gas::GENESIS_BASE_FEE_WEI;

/// Minimal chain spec: id + active hardfork + genesis allocations.
#[derive(Debug, Clone)]
pub struct KanariChainSpec {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Hardfork active from genesis.
    pub spec_id: SpecId,
    /// Genesis allocations as (address, balance in wei).
    pub genesis_alloc: Vec<(Address, U256)>,
    /// Deployed contracts as (address, runtime bytecode). Applied after
    /// balances (used by fork-mode imports; empty on fresh chains).
    pub genesis_code: Vec<(Address, Bytes)>,
    /// Storage slots as (address, slot, value). Applied after code (used
    /// by fork-mode imports; empty on fresh chains).
    pub genesis_storage: Vec<(Address, U256, U256)>,
    /// Base fee (wei) of the genesis block. Block 1+ adjust from here by
    /// EIP-1559 dynamics; tune per environment (LAN devnets can start lower).
    pub base_fee_wei: u128,
}

impl KanariChainSpec {
    /// Fresh devnet: newest hardfork active, one funded account.
    pub fn devnet() -> Self {
        Self {
            chain_id: KANARI_EVM_DEV_CHAIN_ID,
            spec_id: KANARI_EVM_GENESIS_SPEC,
            genesis_alloc: vec![(DEV_FUNDED_ACCOUNT, U256::from(DEV_FUNDED_BALANCE))],
            genesis_code: Vec::new(),
            genesis_storage: Vec::new(),
            base_fee_wei: GENESIS_BASE_FEE_WEI,
        }
    }

    /// Custom chain with explicit genesis allocations (tooling / tests).
    pub fn with_alloc(chain_id: u64, spec_id: SpecId, genesis_alloc: Vec<(Address, U256)>) -> Self {
        Self {
            chain_id,
            spec_id,
            genesis_alloc,
            genesis_code: Vec::new(),
            genesis_storage: Vec::new(),
            base_fee_wei: GENESIS_BASE_FEE_WEI,
        }
    }

    /// Override the genesis base fee (builder style).
    pub fn with_base_fee(mut self, base_fee_wei: u128) -> Self {
        self.base_fee_wei = base_fee_wei.max(1);
        self
    }

    /// Writes genesis allocations into an in-memory database.
    pub fn apply_genesis(&self, db: &mut InMemoryDB) {
        for (address, balance) in &self.genesis_alloc {
            let code = self
                .genesis_code
                .iter()
                .find(|(a, _)| a == address)
                .map(|(_, c)| Bytecode::new_raw(c.clone()))
                .filter(|bc| !bc.is_empty());
            let (code_hash, code) = match code {
                Some(bc) => (bc.hash_slow(), Some(bc)),
                None => (revm::primitives::KECCAK_EMPTY, None),
            };
            db.insert_account_info(
                *address,
                AccountInfo {
                    balance: *balance,
                    nonce: 0,
                    code_hash,
                    account_id: None,
                    code,
                },
            );
        }
        // Contracts with no balance entry (zero-balance code accounts).
        for (address, code) in &self.genesis_code {
            if code.is_empty() || self.genesis_alloc.iter().any(|(a, _)| a == address) {
                continue;
            }
            let bytecode = Bytecode::new_raw(code.clone());
            db.insert_account_info(
                *address,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: bytecode.hash_slow(),
                    account_id: None,
                    code: Some(bytecode),
                },
            );
        }
        for (address, slot, value) in &self.genesis_storage {
            // Infallible in practice: the exterior database is empty and the
            // account entry is created on demand. `expect` keeps `apply_genesis`
            // total (genesis building has no partial-failure semantics).
            db.insert_account_storage(*address, *slot, *value)
                .expect("in-memory genesis storage insert");
        }
    }
}
