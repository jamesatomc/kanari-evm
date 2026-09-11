// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM execution layer: chain spec + genesis, PQC verification
//! precompiles, hand-assembled demo contracts and the instant-seal execution
//! engine (`KanariNode`) built on the `revm` interpreter.

pub mod chainspec;
pub mod contracts;
pub mod node;
pub mod precompiles;

pub use chainspec::{
    DEV_FUNDED_ACCOUNT, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KANARI_EVM_MAX_SUPPLY_ETH, KanariChainSpec,
};
pub use node::{
    BLOCK_BENEFICIARY, BLOCK_GAS_LIMIT, CallRequest, DEFAULT_BASE_FEE_WEI, FAUCET_GENESIS_ETH,
    KanariNode, MAX_FAUCET_ETH_PER_REQUEST, SmtProof, WEI_IN_ETH, generate_faucet_key,
};
