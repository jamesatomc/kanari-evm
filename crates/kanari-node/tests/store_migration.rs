// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Legacy JSON journal migration: a pre-RocksDB state file is replayed into
//! RocksDB once (chain + alloc + faucet sidecar), then renamed aside so it
//! is never migrated twice.

use kanari_evm_move_execution::{
    KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec, KanariNode,
};

fn temp_base(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kanari-evm-mig-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("state.json")
}

#[test]
fn migrates_legacy_json_journal_once() {
    let base = temp_base("basic");
    let funder = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let legacy = serde_json::json!({
        "chain_id": KANARI_EVM_DEV_CHAIN_ID,
        "genesis_alloc": [[funder, "0x56bc75e2d63100000"]],
        "blocks": [],
    });
    std::fs::write(&base, serde_json::to_vec(&legacy).expect("json")).expect("write");
    // Legacy faucet sidecar alongside it.
    let sidecar = format!("{}.faucet.key", base.display());
    std::fs::write(
        &sidecar,
        "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
    )
    .expect("sidecar");

    let spec =
        KanariChainSpec::with_alloc(KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, vec![]);
    let mut node = KanariNode::open(spec, &base).expect("migrate");
    assert_eq!(node.block_number(), 0);
    let balance = node
        .balance_of(funder.parse().expect("addr"))
        .expect("balance");
    assert_eq!(
        balance,
        "0x56bc75e2d63100000"
            .parse::<alloy_primitives::U256>()
            .expect("u256")
    );
    // Faucet sidecar migrated into the store.
    assert!(node.load_faucet_key().expect("load"));
    assert!(node.faucet_address().expect("addr").is_some());
    // Legacy artifacts renamed/removed, never re-migrated.
    assert!(!base.exists(), "legacy journal renamed aside");
    assert!(
        base.with_extension("migrated.json").exists(),
        "renamed journal present"
    );
    assert!(!std::path::Path::new(&sidecar).exists(), "sidecar consumed");
    drop(node);

    // Reopen hits RocksDB directly (no legacy file left to migrate).
    let spec2 =
        KanariChainSpec::with_alloc(KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, vec![]);
    let mut reopened = KanariNode::open(spec2, &base).expect("reopen");
    assert_eq!(reopened.block_number(), 0);
    assert_eq!(
        reopened
            .balance_of(funder.parse().expect("addr"))
            .expect("balance"),
        "0x56bc75e2d63100000"
            .parse::<alloy_primitives::U256>()
            .expect("u256")
    );
    assert!(reopened.load_faucet_key().expect("load"));

    std::fs::remove_dir_all(base.parent().expect("parent")).ok();
}

#[test]
fn rejects_chain_mismatched_legacy_journal() {
    let base = temp_base("mismatch");
    let legacy = serde_json::json!({
        "chain_id": 1u64,
        "genesis_alloc": [],
        "blocks": [],
    });
    std::fs::write(&base, serde_json::to_vec(&legacy).expect("json")).expect("write");
    let spec =
        KanariChainSpec::with_alloc(KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, vec![]);
    assert!(KanariNode::open(spec, &base).is_err());
    std::fs::remove_dir_all(base.parent().expect("parent")).ok();
}
