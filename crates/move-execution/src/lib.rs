// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM execution layer.
//!
//! A custom Kanari chain (chain spec + genesis + PQC verification precompiles
//! à la EIP-8052/8053) built on the `revm` interpreter. See the workspace
//! `Cargo.toml` for why the full `reth-node-builder` dependency is deferred
//! to Linux targets.
//!
//! The engine itself is split by concern:
//!
//! - [`node`] — node lifecycle, persistence glue and chain accessors
//! - [`execution`] — transaction execution and instant sealing
//! - [`views`] — JSON block / transaction / receipt views
//! - [`faucet`] — dev faucet
//! - [`state`] — state reads and the SMT state commitment

pub use revm;

pub mod chainspec;
pub mod contracts;
pub mod execution;
pub mod faucet;
pub mod node;
pub mod precompiles;
pub mod state;
pub mod views;

pub use chainspec::{
    DEV_FUNDED_ACCOUNT, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KANARI_EVM_MAX_SUPPLY_ETH, KanariChainSpec,
};
pub use execution::CallRequest;
pub use faucet::generate_faucet_key;
pub use node::{
    BLOCK_BENEFICIARY, BLOCK_GAS_LIMIT, DEFAULT_BASE_FEE_WEI, KanariNode,
    MAX_FAUCET_ETH_PER_REQUEST, NodeMetrics, SharedNode, WEI_IN_ETH,
};
pub use state::SmtProof;
pub use views::{SealedLog, render_sealed_log};

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        Context,
        context::TxEnv,
        database::InMemoryDB,
        database_interface::Database,
        handler::{ExecuteCommitEvm, MainBuilder, MainContext},
        primitives::{TxKind, U256, address},
    };

    const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn dev_chain_id_is_nonzero() {
        assert_ne!(KANARI_EVM_DEV_CHAIN_ID, 0);
    }

    #[test]
    fn genesis_funds_dev_account() {
        let spec = KanariChainSpec::devnet();
        let mut db = InMemoryDB::default();
        spec.apply_genesis(&mut db);

        let info = db
            .basic(DEV_FUNDED_ACCOUNT)
            .expect("db read")
            .expect("dev account funded at genesis");
        assert_eq!(info.balance, U256::from(DEV_FUNDED_BALANCE));
        assert_eq!(info.nonce, 0);
    }

    #[test]
    fn executes_value_transfer_on_kanari_chain() {
        let spec = KanariChainSpec::devnet();
        let mut db = InMemoryDB::default();
        spec.apply_genesis(&mut db);

        let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

        let mut evm = Context::mainnet()
            .with_db(&mut db)
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = spec.chain_id;
                cfg.spec = spec.spec_id;
            })
            .build_mainnet();

        let tx = TxEnv {
            caller: DEV_FUNDED_ACCOUNT,
            gas_limit: 21_000,
            gas_price: 1,
            kind: TxKind::Call(recipient),
            value: U256::from(ONE_ETH_WEI),
            data: Default::default(),
            nonce: 0,
            chain_id: Some(spec.chain_id),
            access_list: Default::default(),
            gas_priority_fee: None,
            ..Default::default()
        };
        let result = evm.transact_commit(tx).expect("transfer must execute");
        assert!(result.is_success(), "transfer must succeed");

        let sender = db
            .basic(DEV_FUNDED_ACCOUNT)
            .expect("db read")
            .expect("sender exists");
        let paid = db
            .basic(recipient)
            .expect("db read")
            .expect("recipient exists");
        assert_eq!(paid.balance, U256::from(ONE_ETH_WEI));
        assert_eq!(sender.nonce, 1);
        assert_eq!(
            sender.balance,
            U256::from(DEV_FUNDED_BALANCE) - U256::from(ONE_ETH_WEI) - U256::from(21_000u64)
        );
    }
}
