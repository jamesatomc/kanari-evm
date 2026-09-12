// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! State commitment tests: deploy a contract that SSTOREs a value, then
//! prove storage/account inclusion against the sealed block's state root
//! using `smt::verify_proof` (independent verifier path).

mod common;

use alloy_consensus::TxEip1559;
use alloy_primitives::{Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, sign_tx};
use kanari_evm_move_execution::{
    CallRequest, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode,
};

/// Init code: `PUSH1 0x2a, PUSH1 0x00, SSTORE, PUSH1 0x00, PUSH1 0x00, RETURN`
/// Stores 0x2a at slot 0, returns empty runtime code.
const SSTORE_INIT: &[u8] = &[
    0x60, 0x2a, // PUSH1 0x2a
    0x60, 0x00, // PUSH1 0x00
    0x55, // SSTORE
    0x60, 0x00, // PUSH1 0x00 (return offset)
    0x60, 0x00, // PUSH1 0x00 (return size)
    0xf3, // RETURN
];

/// Runtime code returning the constant 42 as a 32-byte word:
/// `PUSH1 0x2a, PUSH1 0x00, MSTORE, PUSH1 0x20, PUSH1 0x00, RETURN`.
const RETURN42_RUNTIME: &[u8] = &[
    0x60, 0x2a, // PUSH1 0x2a
    0x60, 0x00, // PUSH1 0x00
    0x52, // MSTORE
    0x60, 0x20, // PUSH1 0x20
    0x60, 0x00, // PUSH1 0x00
    0xf3, // RETURN
];

/// Init code deploying [`RETURN42_RUNTIME`] via CODECOPY (prefix is 13
/// bytes, runtime is 10 bytes at offset 13).
const RETURN42_INIT: &[u8] = &[
    0x60, 0x0a, // PUSH1 10 (len)
    0x80, // DUP1
    0x60, 0x0d, // PUSH1 13 (code offset of embedded runtime)
    0x60, 0x00, // PUSH1 0x00 (memory dest)
    0x39, // CODECOPY
    0x60, 0x0a, // PUSH1 10 (len)
    0x60, 0x00, // PUSH1 0x00 (offset)
    0xf3, // RETURN
    // --- embedded runtime (10 bytes) ---
    0x60, 0x2a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
];

fn base_tx() -> TxEip1559 {
    TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce: 0,
        gas_limit: 1_000_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: Bytes::copy_from_slice(SSTORE_INIT),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    }
}

#[tokio::test]
async fn deploy_proves_storage_and_account_inclusion() {
    let deployer = PrivateKeySigner::random();
    let deployer_addr = deployer.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(deployer_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = common::temp_dir("smt");

    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");
    let raw = sign_tx(&deployer, base_tx()).await;
    let tx_hash = node.send_raw_transaction(raw).expect("deploy seals");

    let receipt = node.receipt(&tx_hash).expect("receipt");
    assert!(receipt.success, "deploy must succeed");
    let contract = receipt.contract_address.expect("create yields address");

    // Storage slot 0 holds 0x2a: prove inclusion, verify independently.
    let proof = node
        .smt_proof(contract, Some(U256::ZERO))
        .expect("storage proof");
    assert!(proof.exists, "slot must be committed");
    let mut expected_value = [0u8; 32];
    expected_value[31] = 0x2a;
    assert_eq!(proof.value.expect("value"), expected_value);
    assert!(
        smt::verify_proof(
            &proof.root.0,
            &proof.key.0,
            (
                proof.exists,
                proof.leaf.0,
                proof.siblings.iter().map(|s| s.0).collect()
            )
        ),
        "storage proof must verify against the sealed state root"
    );

    // Contract account itself is committed too.
    let account_proof = node.smt_proof(contract, None).expect("account proof");
    assert!(account_proof.exists);
    assert!(
        smt::verify_proof(
            &account_proof.root.0,
            &account_proof.key.0,
            (
                account_proof.exists,
                account_proof.leaf.0,
                account_proof.siblings.iter().map(|s| s.0).collect()
            )
        ),
        "account proof must verify"
    );

    // Block view carries the same root.
    let view = node.block_view(1, false).expect("block 1");
    assert_eq!(
        view["stateRoot"].as_str().expect("root"),
        proof.root.to_string().as_str()
    );

    // Absent keys yield verifiable non-membership proofs.
    let ghost = node
        .smt_proof(
            "0x000000000000000000000000000000000000dEaD"
                .parse()
                .expect("addr"),
            None,
        )
        .expect("ghost proof");
    assert!(!ghost.exists);
    assert!(
        smt::verify_proof(
            &ghost.root.0,
            &ghost.key.0,
            (
                ghost.exists,
                ghost.leaf.0,
                ghost.siblings.iter().map(|s| s.0).collect()
            )
        ),
        "non-membership proof must verify"
    );

    // Reopen: replay restores the same root (deterministic commitment).
    drop(node);
    let reopened = KanariNode::open(
        KanariChainSpec::with_alloc(
            KANARI_EVM_DEV_CHAIN_ID,
            KANARI_EVM_GENESIS_SPEC,
            vec![(deployer_addr, U256::from(DEV_FUNDED_BALANCE))],
        ),
        dir.join("state.json"),
    )
    .expect("reopen");
    assert_eq!(reopened.block_number(), 1);
    let view2 = reopened.block_view(1, false).expect("block 1 after reopen");
    assert_eq!(view2["stateRoot"], view["stateRoot"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// Contracts are first-class: deploy code that returns the constant 42,
/// then prove (a) bytecode is stored on-chain, (b) calling it executes and
/// returns the 32-byte word 42 — i.e. the chain supports smart contracts.
#[tokio::test]
async fn deploy_callable_contract_returns_value() {
    assert_eq!(
        RETURN42_INIT.len(),
        13 + RETURN42_RUNTIME.len(),
        "init prefix must be 13 bytes with runtime at offset 13"
    );
    let deployer = PrivateKeySigner::random();
    let deployer_addr = deployer.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(deployer_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = common::temp_dir("call");

    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");
    let mut tx = base_tx();
    tx.input = Bytes::copy_from_slice(RETURN42_INIT);
    let raw = sign_tx(&deployer, tx).await;
    let tx_hash = node.send_raw_transaction(raw).expect("deploy seals");

    let receipt = node.receipt(&tx_hash).expect("receipt");
    assert!(receipt.success, "deploy must succeed");
    let contract = receipt.contract_address.expect("create yields address");

    // (a) Deployed bytecode is stored exactly.
    let code = node.code_of(contract).expect("code_of");
    assert_eq!(
        code.as_ref(),
        RETURN42_RUNTIME,
        "stored runtime bytecode must match"
    );

    // (b) Calling the contract executes it and returns 32-byte 42.
    let result = node
        .call(CallRequest {
            from: Some(deployer_addr),
            to: Some(contract),
            data: Some(Bytes::new()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call");
    assert!(result.success, "contract call must succeed");
    let mut expected = [0u8; 32];
    expected[31] = 0x2a;
    assert_eq!(
        result.output.as_ref(),
        expected.as_slice(),
        "contract must return the constant 42"
    );

    std::fs::remove_dir_all(&dir).ok();
}
