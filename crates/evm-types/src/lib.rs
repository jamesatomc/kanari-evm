// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared leaf primitives for the Kanari EVM workspace: hex and Ethereum
//! QUANTITY formatting in one place, so every crate renders hashes,
//! quantities and storage words identically.
//!
//! Leaf by design: only `alloy-primitives` + `thiserror`, so every crate
//! can depend on this without cycles.

pub mod gas;

use alloy_primitives::U256;

/// Hex codec errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HexError {
    /// Odd number of hex digits.
    #[error("odd hex length (want pairs of digits)")]
    OddLength,
    /// Non-hex character.
    #[error("non-hex character in input")]
    InvalidChar,
    /// Wrong byte length for a fixed-size decode.
    #[error("want {want} bytes, got {got}")]
    InvalidLength { want: usize, got: usize },
}

/// Lowercase hex without `0x` prefix.
pub fn hex_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(CHARS[(b >> 4) as usize] as char);
        out.push(CHARS[(b & 0x0f) as usize] as char);
    }
    out
}

/// Lowercase hex with `0x` prefix (payloads, log data, code).
pub fn hex_prefixed(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    out.push_str(&hex_encode(bytes));
    out
}

/// Decode hex with or without a `0x` prefix (surrounding whitespace
/// tolerated).
pub fn hex_decode(hex: &str) -> Result<Vec<u8>, HexError> {
    let hex = hex.trim().trim_start_matches("0x");
    if !hex.len().is_multiple_of(2) {
        return Err(HexError::OddLength);
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).map_err(|_| HexError::InvalidChar)?;
        let byte = u8::from_str_radix(pair, 16).map_err(|_| HexError::InvalidChar)?;
        out.push(byte);
    }
    Ok(out)
}

/// Decode exactly 32 bytes of hex (keys, hashes, slots).
pub fn hex_decode32(hex: &str) -> Result<[u8; 32], HexError> {
    let bytes = hex_decode(hex)?;
    let got = bytes.len();
    bytes
        .try_into()
        .map_err(|_| HexError::InvalidLength { want: 32, got })
}

/// Ethereum QUANTITY: minimal `0x` hex (`0x0`, `0x5208`, …).
pub fn quantity_u64(v: u64) -> String {
    format!("0x{v:x}")
}

/// Ethereum QUANTITY for 256-bit values.
pub fn quantity_u256(v: U256) -> String {
    format!("0x{v:x}")
}

/// Fixed 32-byte DATA word: `0x` + 64 hex chars (storage values, topics).
pub fn bytes32_hex(v: U256) -> String {
    format!("0x{v:064x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_encode(&[0xab, 0x01, 0xff]), "ab01ff");
        assert_eq!(hex_prefixed(&[0xab]), "0xab");
        assert_eq!(hex_decode("0xab01").expect("decode"), vec![0xab, 0x01]);
        assert_eq!(hex_decode("ab01").expect("decode"), vec![0xab, 0x01]);
        assert_eq!(hex_decode("  0xAB  ").expect("decode"), vec![0xab]);
        assert!(matches!(hex_decode("0x0"), Err(HexError::OddLength)));
        assert!(matches!(hex_decode("0xzz"), Err(HexError::InvalidChar)));
    }

    #[test]
    fn decode32_strict() {
        let full = "ab".repeat(32);
        assert_eq!(hex_decode32(&full).expect("32B").len(), 32);
        assert!(hex_decode32("ab").is_err());
        assert!(hex_decode32(&"ab".repeat(33)).is_err());
    }

    #[test]
    fn quantities() {
        assert_eq!(quantity_u64(0), "0x0");
        assert_eq!(quantity_u64(21000), "0x5208");
        assert_eq!(quantity_u256(U256::ZERO), "0x0");
        assert_eq!(
            bytes32_hex(U256::from(0xaa)),
            "0x00000000000000000000000000000000000000000000000000000000000000aa"
        );
    }
}
