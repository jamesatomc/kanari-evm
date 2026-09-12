// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint fork end to end, without any external chain: spin node A,
//! deploy + mutate a contract, then fork-import its balance, code and one
//! storage slot into node B's genesis and prove they match.

mod common;

use alloy_primitives::{TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, open_for_signer, serve, sign_1559};
use kanari_evm_move_execution::{
    DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec,
    KanariNode, contracts,
};
use kanari_evm_node::fork::{SlotRef, fetch_fork_genesis};
use serde_json::json;

#[tokio::test]
async fn fork_imports_balance_code_and_storage() {
    let deployer = PrivateKeySigner::random();
    let (mut node_a, dir_a, _) = open_for_signer("fork-src", &deployer);

    // Deploy SimpleStorage and set(42) on node A.
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_hash = node_a
        .send_raw_transaction(
            sign_1559(
                &deployer,
                TxKind::Create,
                init.into(),
                0,
                1_000_000,
                GWEI_WEI,
                U256::ZERO,
            )
            .await,
        )
        .expect("deploy seals");
    let contract = node_a
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");
    let set_hash = node_a
        .send_raw_transaction(
            sign_1559(
                &deployer,
                TxKind::Call(contract),
                contracts::encode_set(42).into(),
                1,
                500_000,
                GWEI_WEI,
                U256::ZERO,
            )
            .await,
        )
        .expect("set seals");
    assert!(node_a.receipt(&set_hash).expect("receipt").success);
    let expected_balance = node_a.balance_of(contract).expect("balance");

    // Import balance + code + slot 0 into a fresh spec via node A's RPC.
    let (url, server) = serve(node_a).await;
    let client = reqwest::Client::new();
    let forked = fetch_fork_genesis(
        &client,
        url.trim_end_matches('/'),
        "latest",
        &[contract],
        &[SlotRef {
            address: contract,
            slot: U256::ZERO,
        }],
    )
    .await
    .expect("fork fetch works");
    assert_eq!(forked.chain_id, KANARI_EVM_DEV_CHAIN_ID);
    assert_eq!(forked.alloc, vec![(contract, expected_balance)]);
    assert_eq!(forked.code.len(), 1);
    assert_eq!(forked.code[0].0, contract);
    assert_eq!(forked.code[0].1.as_ref(), contracts::SIMPLE_STORAGE_RUNTIME);
    assert_eq!(forked.storage.len(), 1);
    let (_, _, slot_value) = forked.storage[0];
    assert_eq!(
        slot_value,
        U256::from(42),
        "imported slot 0 must hold set(42)"
    );

    // Node B boots from the import: same code, same slot, same balance.
    // (Plus a funded caller — the fork only imported the contract.)
    let mut alloc = forked.alloc;
    alloc.push((deployer.address(), U256::from(DEV_FUNDED_BALANCE)));
    let mut spec = KanariChainSpec::with_alloc(forked.chain_id, KANARI_EVM_GENESIS_SPEC, alloc);
    spec.genesis_code = forked.code;
    spec.genesis_storage = forked.storage;
    let dir_b = common::temp_dir("fork-dst");
    let mut node_b = KanariNode::open(spec, dir_b.join("state.json")).expect("open forked");
    assert_eq!(
        node_b.code_of(contract).expect("code").as_ref(),
        contracts::SIMPLE_STORAGE_RUNTIME
    );
    assert_eq!(
        node_b.storage_of(contract, U256::ZERO).expect("storage"),
        U256::from(42)
    );
    assert_eq!(
        node_b.balance_of(contract).expect("balance"),
        expected_balance
    );
    // ...and the imported contract actually executes.
    let out = node_b
        .call(kanari_evm_move_execution::CallRequest {
            from: Some(deployer.address()),
            to: Some(contract),
            data: Some(contracts::encode_get().into()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call");
    assert!(out.success);
    let mut word = [0u8; 32];
    word[31] = 42;
    assert_eq!(out.output.as_ref(), &word);

    // eth_getLogs-style RPC works against the forked node too.
    let balance = common::rpc(
        &client,
        &url,
        "eth_getBalance",
        json!([contract.to_string(), "latest"]),
    )
    .await;
    assert_eq!(
        common::quantity_u128(&balance),
        expected_balance.to::<u128>()
    );

    server.abort();
    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();
}
