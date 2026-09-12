// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Dynamic base fees: EIP-1559 adjustment from sealed-block fullness.
//!
//! A fat block pushes the next fee up, idle blocks decay it toward the
//! 1-wei floor, underpriced transactions are rejected, and `eth_gasPrice`
//! / `eth_feeHistory` track the live schedule.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256, address};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm_move_execution::{
    BLOCK_GAS_LIMIT, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode, contracts,
};
use kanari_evm_rpc::rpc;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

const GWEI_WEI: u128 = 1_000_000_000;

// Runtime: `for (i = 0; i < 700; i++) SSTORE(i, i)`, then STOP.
// Addresses/values are 32-byte words; every slot is cold (~22.1k gas),
// so one call burns ~15.5M gas — just over the 15M (30M/2) target.
// (GT compares top-of-stack first: `[N, i+1]` yields `N > i+1`.)
const LOOP_N: u16 = 700;
fn loop_runtime() -> Vec<u8> {
    vec![
        0x60, 0x00, // PUSH1 0x00           [i]
        0x5b, // JUMPDEST              loop:
        0x80, // DUP1                  [i,i]
        0x80, // DUP1                  [i,i,i]
        0x55, // SSTORE                [i]
        0x60, 0x01, // PUSH1 0x01      [i,1]
        0x01, // ADD                   [i+1]
        0x80, // DUP1                  [i+1,i+1]
        0x61, (LOOP_N >> 8) as u8, (LOOP_N & 0xff) as u8, // PUSH2 N
        0x11, // GT                    [N>i+1]
        0x60, 0x02, // PUSH1 loop      [dest,cond,i+1]
        0x57, // JUMPI                 [i+1]
        0x50, // POP                   []
        0x00, // STOP
    ]
}

async fn sign_1559(
    signer: &PrivateKeySigner,
    to: TxKind,
    input: Bytes,
    nonce: u64,
    gas_limit: u64,
    max_fee: u128,
) -> Bytes {
    let tx = TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce,
        gas_limit,
        to,
        value: U256::ZERO,
        input,
        access_list: Default::default(),
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: GWEI_WEI,
    };
    let sig = signer.sign_hash(&tx.signature_hash()).await.expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    Bytes::from(raw)
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("rpc reachable")
        .json()
        .await
        .expect("valid json");
    assert!(
        resp.get("error").is_none(),
        "rpc {method} errored: {}",
        resp["error"]
    );
    resp["result"].clone()
}

fn quantity(v: &Value) -> u128 {
    let s = v.as_str().expect("0x quantity");
    u128::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex")
}

#[tokio::test]
async fn fees_rise_fall_and_gate_transactions() {
    let funder = PrivateKeySigner::random();
    let funder_addr = funder.address();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(funder_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = std::env::temp_dir().join(format!("kanari-evm-gasdyn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");
    assert_eq!(node.pending_base_fee(), GWEI_WEI, "genesis fee is 1 gwei");

    // Block 1: deploy the loop contract (cheap, executes at 1 gwei).
    let init = contracts::deploy_init(&loop_runtime());
    let deploy_hash = node
        .send_raw_transaction(sign_1559(&funder, TxKind::Create, init.into(), 0, 1_000_000, GWEI_WEI).await)
        .expect("deploy seals");
    let looper: Address = node
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");

    // Block 2: the fat call (~15.5M gas > 15M target) executes AT 1 gwei...
    let fat_raw = sign_1559(&funder, TxKind::Call(looper), Bytes::new(), 1, 16_500_000, GWEI_WEI).await;
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
    assert!(risen > fee2, "over-target block must raise fee, got {risen}");

    // A 1-wei-max transaction is underpriced at any live fee and rejected.
    let cheap = sign_1559(&funder, TxKind::Call(recipient), Bytes::new(), 2, 21_000, 1).await;
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
        )
        .await;
        node.send_raw_transaction(filler).expect("filler seals");
    }
    let decayed = node.pending_base_fee();
    assert!(decayed < risen, "idle must decay fee ({decayed} < {risen})");
    assert!(decayed >= 1, "fee never hits zero");

    // RPC tracks the live schedule.
    let app = rpc::router(Arc::new(Mutex::new(node)));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let client = reqwest::Client::new();
    let gas_price = rpc(&client, &url, "eth_gasPrice", json!([])).await;
    assert_eq!(quantity(&gas_price), decayed, "gasPrice must equal pending fee");

    let history = rpc(&client, &url, "eth_feeHistory", json!(["0x4", "latest", []])).await;
    let fees = history["baseFeePerGas"].as_array().expect("fees array");
    assert_eq!(fees.len(), 5, "count+1 entries");
    assert!(
        fees.iter().all(|f| f.as_str().expect("hex").starts_with("0x")),
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
