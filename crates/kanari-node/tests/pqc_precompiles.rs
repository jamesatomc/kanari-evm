// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the Kanari PQC verification precompiles (`0x100`
//! Falcon-512, `0x101` Dilithium3): real kanari-crypto keys sign a message,
//! the node executes a CALL to the precompile, and the 32-byte boolean output
//! is asserted for both valid and tampered signatures.

use kanari_crypto::keys::{CurveType, generate_keypair};
use kanari_evm_move_execution::{
    CallRequest, KanariChainSpec, KanariNode,
    precompiles::{DILITHIUM3_VERIFY_ADDRESS, FALCON512_VERIFY_ADDRESS},
};

fn build_input(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + pubkey.len() + msg.len() + sig.len());
    out.extend_from_slice(&(pubkey.len() as u16).to_be_bytes());
    out.extend_from_slice(pubkey);
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg);
    out.extend_from_slice(sig);
    out
}

fn fresh_node(tag: &str) -> (KanariNode, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("kanari-evm-pqc-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("state.json");
    let node = KanariNode::open(KanariChainSpec::devnet(), &file).expect("open node");
    (node, dir)
}

fn call_precompile(
    node: &mut KanariNode,
    to: alloy_primitives::Address,
    input: Vec<u8>,
) -> Vec<u8> {
    // eth_call charges gas against state, so call from the funded genesis
    // account (calls are not signed; no key needed).
    let out = node
        .call(CallRequest {
            from: Some(kanari_evm_move_execution::DEV_FUNDED_ACCOUNT),
            to: Some(to),
            data: Some(input.into()),
            gas: Some(5_000_000),
            ..Default::default()
        })
        .expect("eth_call executes");
    assert!(out.success, "precompile call must not revert");
    assert_eq!(out.output.len(), 32, "output is a 32-byte word");
    out.output.to_vec()
}

#[test]
fn falcon512_precompile_verifies() {
    let kp = generate_keypair(CurveType::Falcon512).expect("keygen");
    let msg = b"kanari pqc precompile";
    let secret = kp.export_private_key_secure();
    let sig =
        kanari_crypto::signatures::falcon::sign_message_falcon512(&secret, msg).expect("sign");
    let pubkey = alloy_primitives::hex::decode(&kp.public_key).expect("pubkey hex");

    let (mut node, dir) = fresh_node("falcon");
    let yes = call_precompile(
        &mut node,
        FALCON512_VERIFY_ADDRESS,
        build_input(&pubkey, msg, &sig),
    );
    assert_eq!(yes[31], 1, "valid Falcon-512 signature must verify");

    let mut bad_sig = sig.clone();
    bad_sig[0] ^= 0xff;
    let no = call_precompile(
        &mut node,
        FALCON512_VERIFY_ADDRESS,
        build_input(&pubkey, msg, &bad_sig),
    );
    assert_eq!(no[31], 0, "tampered signature must fail closed");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dilithium3_precompile_verifies() {
    let kp = generate_keypair(CurveType::Dilithium3).expect("keygen");
    let msg = b"kanari pqc precompile";
    let secret = kp.export_private_key_secure();
    let sig =
        kanari_crypto::signatures::dilithium3::sign_message_dilithium3(&secret, msg).expect("sign");
    let pubkey = alloy_primitives::hex::decode(&kp.public_key).expect("pubkey hex");

    let (mut node, dir) = fresh_node("dilithium");
    let yes = call_precompile(
        &mut node,
        DILITHIUM3_VERIFY_ADDRESS,
        build_input(&pubkey, msg, &sig),
    );
    assert_eq!(yes[31], 1, "valid Dilithium3 signature must verify");

    let mut bad_sig = sig.clone();
    bad_sig[10] ^= 0x01;
    let no = call_precompile(
        &mut node,
        DILITHIUM3_VERIFY_ADDRESS,
        build_input(&pubkey, msg, &bad_sig),
    );
    assert_eq!(no[31], 0, "tampered signature must fail closed");

    std::fs::remove_dir_all(&dir).ok();
}
