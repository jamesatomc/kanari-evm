// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM execution layer: chain spec + genesis, PQC verification
//! precompiles, hand-assembled demo contracts and the instant-seal execution
//! engine (`KanariNode`) built on the `revm` interpreter.
//!
//! The engine itself is split by concern:
//!
//! - [`node`] — node lifecycle, persistence glue and chain accessors
//! - [`execution`] — transaction execution and instant sealing
//! - [`views`] — JSON block / transaction / receipt views
//! - [`faucet`] — dev faucet
//! - [`state`] — state reads and the SMT state commitment

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
    BLOCK_BENEFICIARY, BLOCK_GAS_LIMIT, DEFAULT_BASE_FEE_WEI, FAUCET_GENESIS_ETH, KanariNode,
    MAX_FAUCET_ETH_PER_REQUEST, WEI_IN_ETH,
};
pub use state::SmtProof;
