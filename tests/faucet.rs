// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Faucet tests: node-funded drips seal like normal transactions, the
//! per-request cap is enforced, and missing keys fail closed.

use kanari_evm::{
    KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec, KanariNode, WEI_IN_ETH,
    generate_faucet_key,
};

fn eth(n: u128) -> alloy_primitives::U256 {
    alloy_primitives::U256::from(n * WEI_IN_ETH)
}

fn fresh_state(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kanari-evm-faucet-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("state.json")
}

#[test]
fn faucet_drips_seal_and_cap_enforced() {
    let (faucet_addr, faucet_secret) = generate_faucet_key();
    let user = alloy_primitives::address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(faucet_addr, eth(1_000_000))],
    );
    let state = fresh_state("drip");
    let mut node = KanariNode::open(spec, &state).expect("open");
    node.set_faucet_key(faucet_secret).expect("set key");
    assert_eq!(
        node.faucet_address().expect("addr").expect("some"),
        faucet_addr
    );

    // Drip 5 ETH: seals, receipt succeeds, balances move exactly.
    let hash = node.faucet(user, eth(5)).expect("drip");
    let receipt = node.receipt(&hash).expect("receipt");
    assert!(receipt.success);
    assert_eq!(receipt.from, faucet_addr);
    assert_eq!(node.balance_of(user).expect("balance"), eth(5));
    assert_eq!(
        node.balance_of(faucet_addr).expect("balance"),
        eth(1_000_000 - 5) - eth_gas()
    );
    assert_eq!(node.block_number(), 1);

    // Second drip advances the faucet nonce.
    let hash2 = node.faucet(user, eth(1)).expect("drip 2");
    assert_ne!(hash, hash2);
    assert_eq!(node.balance_of(user).expect("balance"), eth(6));

    // Cap enforced (10_000 + 1 ETH rejected).
    let over = node.faucet(user, eth(10_001));
    assert!(over.is_err(), "over-cap drip must fail");

    // Persistence: sidecar reload keeps the faucet working after reopen.
    drop(node);
    let spec2 = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(faucet_addr, eth(1_000_000))],
    );
    let mut reopened = KanariNode::open(spec2, &state).expect("reopen");
    assert!(reopened.load_faucet_key().expect("load"));
    let hash3 = reopened.faucet(user, eth(2)).expect("drip after reopen");
    assert_ne!(hash3, hash2);

    std::fs::remove_dir_all(state.parent().expect("parent")).ok();
}

#[test]
fn faucet_without_key_fails_closed() {
    let user = alloy_primitives::address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let state = fresh_state("nokey");
    let mut node = KanariNode::open(KanariChainSpec::devnet(), &state).expect("open");
    assert!(!node.load_faucet_key().expect("load"));
    assert!(node.faucet(user, eth(1)).is_err());
    std::fs::remove_dir_all(state.parent().expect("parent")).ok();
}

fn eth_gas() -> alloy_primitives::U256 {
    // 21000 gas * 1 gwei base fee.
    eth(0) + alloy_primitives::U256::from(21_000u128 * 1_000_000_000u128)
}
