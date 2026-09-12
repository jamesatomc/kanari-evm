// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Multi-node convergence: N networked validators over real localhost TCP
//! order the same payloads and seal identical blocks.
//!
//! Each validator runs its own Mysticeti DAG core (real signatures, real
//! sockets) plus its own EVM state. After submitting signed transfers
//! through different validators' mempools, every validator must agree on
//! block numbers, block hashes AND state roots — that is the whole point of
//! deterministic commit-driven sealing.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, TxKind, U256, address};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm_consensus::{
    ValidatorNode, ValidatorOpts, committee::COMMITTEE_FILENAME, generate_committee,
};
use kanari_evm_move_execution::{
    DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec,
    KanariNode,
};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000;
const GWEI_WEI: u128 = 1_000_000_000;

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("kanari-evm-multinode-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

async fn signed_transfer(
    signer: &PrivateKeySigner,
    to: alloy_primitives::Address,
    nonce: u64,
) -> Bytes {
    let tx = TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value: U256::from(ONE_ETH_WEI),
        input: Bytes::new(),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    };
    let sig = signer.sign_hash(&tx.signature_hash()).await.expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    Bytes::from(raw)
}

#[tokio::test]
async fn four_validators_converge_on_execution() {
    let dir = fresh_dir("converge");
    // Fixed localhost DAG ports (mesh source ports are listen * 10, so the
    // test range must stay small; RPC is not needed — validators are driven
    // in-process here).
    let base_dag_port = 3700;
    let committee =
        generate_committee(4, IpAddr::from([127, 0, 0, 1]), base_dag_port, &dir).expect("keygen");

    let funder = PrivateKeySigner::random();
    let funder_addr = funder.address();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    // Identical genesis on every validator (convergence precondition).
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(funder_addr, U256::from(DEV_FUNDED_BALANCE))],
    );

    // One EVM node per validator (own data dir, own chain store).
    let mut validators = Vec::new();
    for i in 0..4 {
        let node_dir = dir.join(format!("node{i}"));
        std::fs::create_dir_all(&node_dir).expect("node dir");
        let node = KanariNode::open(spec.clone(), node_dir.join("state.json")).expect("open");
        let shared = Arc::new(Mutex::new(node));
        let validator = ValidatorNode::spawn(
            shared,
            ValidatorOpts {
                committee_path: dir.join(COMMITTEE_FILENAME),
                key_path: dir.join(format!("validator-{}.key", i + 1)),
                dag_wal_dir: None,
                round_timeout: Duration::from_millis(500),
            },
        )
        .await
        .expect("validator joins mesh");
        validators.push(validator);
    }
    assert_eq!(committee.len(), 4);

    // Submit two transfers through two different validators' mempools.
    let raw_a = signed_transfer(&funder, recipient, 0).await;
    let raw_b = signed_transfer(&funder, recipient, 1).await;
    validators[0]
        .node()
        .lock()
        .await
        .send_raw_transaction(raw_a)
        .expect("submit via validator 1");
    validators[2]
        .node()
        .lock()
        .await
        .send_raw_transaction(raw_b)
        .expect("submit via validator 3");

    // Wait for all validators to seal both transactions (60s budget;
    // localhost DAG commits settle in seconds).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut done = true;
        for v in &validators {
            if v.node().lock().await.block_number() < 2 {
                done = false;
                break;
            }
        }
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "validators did not converge in time"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Full convergence: same heads, same hashes, same state roots.
    let heads: Vec<_> = {
        let mut out = Vec::new();
        for v in &validators {
            let node = v.node().lock().await;
            let head = node.block_by_number(2).expect("block 2").clone();
            out.push((head.hash, node.state_root().expect("root")));
        }
        out
    };
    for (hash, root) in &heads[1..] {
        assert_eq!(*hash, heads[0].0, "block hashes must converge");
        assert_eq!(*root, heads[0].1, "state roots must converge");
    }
    // Both transfers executed exactly once, in consensus order.
    for v in &validators {
        let mut node = v.node().lock().await;
        assert_eq!(
            node.balance_of(recipient).expect("balance"),
            U256::from(2 * ONE_ETH_WEI),
            "recipient must hold both transfers"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
