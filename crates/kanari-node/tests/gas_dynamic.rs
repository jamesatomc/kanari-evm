// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Dynamic base fees: EIP-1559 adjustment from sealed-block fullness.
//!
//! A fat block pushes the next fee up, idle blocks decay it toward the
//! 1-wei floor, underpriced transactions are rejected, and `eth_gasPrice`
//! / `eth_feeHistory` track the live schedule.

mod common;

use alloy_primitives::{Address, Bytes, TxKind, U256, address};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, open_for_signer, quantity_u128 as quantity, rpc, serve, sign_1559};
use kanari_evm_move_execution::{BLOCK_GAS_LIMIT, contracts};
use serde_json::json;

// Runtime: `for (i = 0; i < 700; i++) SSTORE(i, i)`, then STOP.
// Addresses/values are 32-byte words; every slot is cold (~22.1k gas),
// so one call burns ~15.5M gas — just over the 15M (30M/2) target.
// (GT compares top-of-stack first: `[N, i+1]` yields `N > i+1`.)
const LOOP_N: u16 = 700;
fn loop_runtime() -> Vec<u8> {
    vec![
        0x60,
        0x00, // PUSH1 0x00           [i]
        0x5b, // JUMPDEST              loop:
        0x80, // DUP1                  [i,i]
        0x80, // DUP1                  [i,i,i]
        0x55, // SSTORE                [i]
        0x60,
        0x01, // PUSH1 0x01      [i,1]
        0x01, // ADD                   [i+1]
        0x80, // DUP1                  [i+1,i+1]
        0x61,
        (LOOP_N >> 8) as u8,
        (LOOP_N & 0xff) as u8, // PUSH2 N
        0x11,                  // GT                    [N>i+1]
        0x60,
        0x02, // PUSH1 loop      [dest,cond,i+1]
        0x57, // JUMPI                 [i+1]
        0x50, // POP                   []
        0x00, // STOP
    ]
}

#[tokio::test]
async fn fees_rise_fall_and_gate_transactions() {
    let funder = PrivateKeySigner::random();
    let funder_addr = funder.address();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let (mut node, dir, _) = open_for_signer("gasdyn", &funder);
    assert_eq!(node.pending_base_fee(), GWEI_WEI, "genesis fee is 1 gwei");

    // Block 1: deploy the loop contract (cheap, executes at 1 gwei).
    let init = contracts::deploy_init(&loop_runtime());
    let deploy_hash = node
        .send_raw_transaction(
            sign_1559(
                &funder,
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
    let looper: Address = node
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");

    // Block 2: the fat call (~15.5M gas > 15M target) executes AT 1 gwei...
    let fat_raw = sign_1559(
        &funder,
        TxKind::Call(looper),
        Bytes::new(),
        1,
        16_500_000,
        GWEI_WEI,
        U256::ZERO,
    )
    .await;
    let fat_hash = node.send_raw_transaction(fat_raw).expect("fat call seals");
    let fat_receipt = node.receipt(&fat_hash).expect("receipt");
    assert!(fat_receipt.success);
    assert!(
        fat_receipt.gas_used as u128 > BLOCK_GAS_LIMIT as u128 / 2,
        "fat block must exceed target, used {}",
        fat_receipt.gas_used
    );
    let block2 = node.block_by_number(2).expect("block 2");
    let fee2 = block2.base_fee as u128;
    assert!(
        fee2 < GWEI_WEI,
        "block 2 executes at the post-deploy decayed fee, got {fee2}"
    );

    // ...so the fee AFTER the fat block rose above block 2's fee.
    let risen = node.pending_base_fee();
    assert!(
        risen > fee2,
        "over-target block must raise fee, got {risen}"
    );

    // A 1-wei-max transaction is underpriced at any live fee and rejected.
    let cheap = sign_1559(
        &funder,
        TxKind::Call(recipient),
        Bytes::new(),
        2,
        21_000,
        1,
        U256::ZERO,
    )
    .await;
    assert!(
        node.send_raw_transaction(cheap).is_err(),
        "underpriced tx must be rejected at fee {risen}"
    );

    // A 2-gwei-max transaction seals; its receipt prices at the risen base.
    let rich = sign_1559(
        &funder,
        TxKind::Call(recipient),
        Bytes::new(),
        2,
        100_000,
        2 * GWEI_WEI,
        U256::ZERO,
    )
    .await;
    let rich_hash = node.send_raw_transaction(rich).expect("funded tx seals");
    let rich_receipt = node.receipt(&rich_hash).expect("receipt");
    assert!(rich_receipt.success);
    let view = node.tx_view(&rich_hash).expect("tx view");
    let base3 = node.block_by_number(3).expect("block 3").base_fee as u128;
    assert_eq!(
        view["gasPrice"],
        json!(format!("0x{:x}", base3 + GWEI_WEI)),
        "effective price is base+priority below the max cap"
    );

    // Idle blocks decay the fee back toward the floor.
    for _ in 0..5 {
        let filler = sign_1559(
            &funder,
            TxKind::Call(recipient),
            Bytes::new(),
            node.nonce_of(funder_addr).expect("nonce"),
            21_000,
            1_000 * GWEI_WEI,
            U256::ZERO,
        )
        .await;
        node.send_raw_transaction(filler).expect("filler seals");
    }
    let decayed = node.pending_base_fee();
    assert!(decayed < risen, "idle must decay fee ({decayed} < {risen})");
    assert!(decayed >= 1, "fee never hits zero");

    // RPC tracks the live schedule.
    let (url, server) = serve(node).await;
    let client = reqwest::Client::new();
    let gas_price = rpc(&client, &url, "eth_gasPrice", json!([])).await;
    assert_eq!(
        quantity(&gas_price),
        decayed,
        "gasPrice must equal pending fee"
    );

    let history = rpc(
        &client,
        &url,
        "eth_feeHistory",
        json!(["0x4", "latest", []]),
    )
    .await;
    let fees = history["baseFeePerGas"].as_array().expect("fees array");
    assert_eq!(fees.len(), 5, "count+1 entries");
    assert!(
        fees.iter()
            .all(|f| f.as_str().expect("hex").starts_with("0x")),
        "all entries are quantities"
    );
    let ratios = history["gasUsedRatio"].as_array().expect("ratios");
    assert_eq!(ratios.len(), 4);
    assert!(
        ratios.iter().all(|r| r.as_f64().expect("f64") <= 1.0),
        "ratios are fractions"
    );

    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}
