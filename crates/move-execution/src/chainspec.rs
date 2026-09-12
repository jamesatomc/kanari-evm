// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM chain specification and genesis.
//!
//! A fresh Kanari devnet activates the newest hardfork at genesis (no fork
//! history to replay). PQC verification precompiles (Falcon/Dilithium, à la
//! EIP-8052/8053) will be registered as custom precompiles on top of this
//! spec in a follow-up phase.

use revm::{
    database::InMemoryDB,
    primitives::{Address, U256, address, hardfork::SpecId},
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
            base_fee_wei: GENESIS_BASE_FEE_WEI,
        }
    }

    /// Custom chain with explicit genesis allocations (tooling / tests).
    pub fn with_alloc(chain_id: u64, spec_id: SpecId, genesis_alloc: Vec<(Address, U256)>) -> Self {
        Self {
            chain_id,
            spec_id,
            genesis_alloc,
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
            db.insert_account_info(
                *address,
                AccountInfo {
                    balance: *balance,
                    nonce: 0,
                    code_hash: revm::primitives::KECCAK_EMPTY,
                    account_id: None,
                    code: None,
                },
            );
        }
    }
}
