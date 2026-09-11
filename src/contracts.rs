// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Hand-assembled demo contracts for the Kanari EVM dev chain.
//!
//! No Solidity toolchain is required: the bytecode below is written out
//! opcode-by-opcode with destination offsets verified by unit test, so the
//! repo can deploy and exercise real contracts without `solc`/`forge`.

/// `SimpleStorage.set(uint256)`: `60fe47b1`.
pub const SIMPLE_STORAGE_SET_SELECTOR: [u8; 4] = [0x60, 0xfe, 0x47, 0xb1];
/// `SimpleStorage.get()`: `6d4ce63c`.
pub const SIMPLE_STORAGE_GET_SELECTOR: [u8; 4] = [0x6d, 0x4c, 0xe6, 0x3c];

/// Runtime bytecode of `SimpleStorage` (63 bytes):
/// dispatcher on `set(uint256)`/`get()`, single `uint256` at storage slot 0.
/// Jump destinations: `set` at `0x2a`, `get` at `0x33`.
pub const SIMPLE_STORAGE_RUNTIME: &[u8] = &[
    0x60, 0x00, // 0x00: PUSH1 0x00
    0x35, // 0x02: CALLDATALOAD
    0x60, 0xe0, // 0x03: PUSH1 0xe0
    0x1c, // 0x05: SHR (selector)
    0x80, // 0x06: DUP1
    0x63, 0x60, 0xfe, 0x47, 0xb1, // 0x07: PUSH4 set selector
    0x14, // 0x0c: EQ
    0x60, 0x2a, // 0x0d: PUSH1 set_dest
    0x57, // 0x0f: JUMPI
    0x63, 0x6d, 0x4c, 0xe6, 0x3c, // 0x10: PUSH4 get selector
    0x14, // 0x15: EQ
    0x60, 0x33, // 0x16: PUSH1 get_dest
    0x57, // 0x18: JUMPI
    0x60, 0x00, // 0x19: PUSH1 0x00
    0x80, // 0x1b: DUP1
    0xfd, // 0x1c: REVERT (unknown selector)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x1d-0x29: padding
    0x5b, // 0x2a: JUMPDEST (set)
    0x60, 0x04, // 0x2b: PUSH1 0x04
    0x35, // 0x2d: CALLDATALOAD (uint arg)
    0x60, 0x00, // 0x2e: PUSH1 0x00
    0x55, // 0x30: SSTORE
    0x00, // 0x31: STOP
    0x00, // 0x32: padding
    0x5b, // 0x33: JUMPDEST (get)
    0x60, 0x00, // 0x34: PUSH1 0x00
    0x54, // 0x36: SLOAD
    0x60, 0x00, // 0x37: PUSH1 0x00
    0x52, // 0x39: MSTORE
    0x60, 0x20, // 0x3a: PUSH1 0x20
    0x60, 0x00, // 0x3c: PUSH1 0x00
    0xf3, // 0x3e: RETURN
];

/// Build standard init code deploying `runtime` via CODECOPY.
/// Prefix is 13 bytes, so `runtime` must fit in `13 + len <= 255`.
pub fn deploy_init(runtime: &[u8]) -> Vec<u8> {
    assert!(
        !runtime.is_empty() && runtime.len() <= 242,
        "runtime must be 1..=242 bytes"
    );
    let (len, off) = (runtime.len() as u8, 13u8);
    let mut init = vec![
        0x60, len, // PUSH1 len
        0x80, // DUP1
        0x60, off, // PUSH1 13 (code offset of embedded runtime)
        0x60, 0x00, // PUSH1 0x00 (memory dest)
        0x39, // CODECOPY
        0x60, len, // PUSH1 len
        0x60, 0x00, // PUSH1 0x00 (offset)
        0xf3, // RETURN
    ];
    init.extend_from_slice(runtime);
    init
}

/// ABI-encode `set(value)`: selector + 32-byte big-endian argument.
pub fn encode_set(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&SIMPLE_STORAGE_SET_SELECTOR);
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&word);
    out
}

/// ABI-encode `get()`: selector only.
pub fn encode_get() -> Vec<u8> {
    SIMPLE_STORAGE_GET_SELECTOR.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_storage_layout() {
        // Runtime length and jump destinations must match the dispatcher.
        assert_eq!(SIMPLE_STORAGE_RUNTIME.len(), 63);
        assert_eq!(SIMPLE_STORAGE_RUNTIME[0x2a], 0x5b, "set JUMPDEST");
        assert_eq!(SIMPLE_STORAGE_RUNTIME[0x33], 0x5b, "get JUMPDEST");
        // Selectors embedded at the documented offsets.
        assert_eq!(&SIMPLE_STORAGE_RUNTIME[0x08..0x0c], SIMPLE_STORAGE_SET_SELECTOR);
        assert_eq!(&SIMPLE_STORAGE_RUNTIME[0x11..0x15], SIMPLE_STORAGE_GET_SELECTOR);
        // Init-code round trip: prefix 13 bytes, runtime at offset 13.
        let init = deploy_init(SIMPLE_STORAGE_RUNTIME);
        assert_eq!(init.len(), 13 + SIMPLE_STORAGE_RUNTIME.len());
        assert_eq!(&init[13..], SIMPLE_STORAGE_RUNTIME);
        // ABI encoding sanity.
        let set = encode_set(0x3039);
        assert_eq!(set.len(), 36);
        assert_eq!(&set[..4], SIMPLE_STORAGE_SET_SELECTOR);
        assert_eq!(set[35], 0x39);
        assert_eq!(encode_get(), SIMPLE_STORAGE_GET_SELECTOR);
    }
}
