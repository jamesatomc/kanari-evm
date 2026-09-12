// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint fork: import remote chain state into genesis.
//!
//! Unlike Anvil's live fork (every unknown slot fetched on demand through a
//! custom database), this snapshots EXPLICITLY listed accounts: balances,
//! deployed code and chosen storage slots at one remote block. The result
//! bakes into `genesis_alloc` / `genesis_code`, so from block 1 the chain
//! is fully local and deterministic.
//!
//! Honest scope: contracts whose storage you did NOT list start empty.
//! List every slot your flow touches, or fork with the accounts whose code
//! paths you actually exercise.

use alloy_primitives::{Address, Bytes, U256};
use serde_json::{Value, json};

/// One storage slot to import: `0xaddress:0xslot`.
#[derive(Debug, Clone)]
pub struct SlotRef {
    pub address: Address,
    pub slot: U256,
}

/// Parse `0xaddress:0xslot` (whitespace tolerated).
pub fn parse_slot(s: &str) -> Result<SlotRef, String> {
    let (addr, slot) = s
        .split_once(':')
        .ok_or_else(|| format!("bad --slot '{s}': want 0xaddress:0xslot"))?;
    let address: Address = addr
        .trim()
        .parse()
        .map_err(|_| format!("bad --slot address '{addr}'"))?;
    let slot = U256::from_str_radix(slot.trim().trim_start_matches("0x"), 16)
        .map_err(|_| format!("bad --slot slot '{slot}'"))?;
    Ok(SlotRef { address, slot })
}

/// State imported from the remote endpoint.
pub struct ForkedGenesis {
    pub chain_id: u64,
    pub alloc: Vec<(Address, U256)>,
    pub code: Vec<(Address, Bytes)>,
    /// `(address, slot, value)` in request order.
    pub storage: Vec<(Address, U256, U256)>,
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{url} unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{url} returned non-JSON: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("remote {method} failed: {err}"));
    }
    resp.get("result")
        .cloned()
        .ok_or_else(|| format!("remote {method} returned no result"))
}

fn parse_hex_u64(v: &Value) -> Result<u64, String> {
    v.as_str()
        .ok_or_else(|| "want 0x quantity".to_string())
        .and_then(|s| {
            u64::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(|_| "invalid 0x quantity".to_string())
        })
}

fn parse_hex_u256(v: &Value) -> Result<U256, String> {
    v.as_str()
        .ok_or_else(|| "want 0x quantity".to_string())
        .and_then(|s| {
            U256::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(|_| "invalid 0x quantity".to_string())
        })
}

/// Fetch balances, code and storage slots at `block` (`"latest"` or
/// `0x`-number) into genesis shapes.
pub async fn fetch_fork_genesis(
    client: &reqwest::Client,
    rpc_url: &str,
    block: &str,
    accounts: &[Address],
    slots: &[SlotRef],
) -> Result<ForkedGenesis, String> {
    if block != "latest"
        && !(block.starts_with("0x") && parse_hex_u64(&Value::String(block.to_string())).is_ok())
    {
        return Err(format!(
            "bad --block '{block}': want 'latest' or a 0x block number"
        ));
    }
    let chain_id = {
        let v = rpc_call(client, rpc_url, "eth_chainId", json!([])).await?;
        parse_hex_u64(&v)?
    };
    let mut alloc = Vec::with_capacity(accounts.len());
    let mut code = Vec::new();
    for addr in accounts {
        let balance = rpc_call(
            client,
            rpc_url,
            "eth_getBalance",
            json!([addr.to_string(), block]),
        )
        .await
        .and_then(|v| parse_hex_u256(&v))?;
        alloc.push((*addr, balance));
        let code_hex: String = rpc_call(
            client,
            rpc_url,
            "eth_getCode",
            json!([addr.to_string(), block]),
        )
        .await
        .and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "eth_getCode must return 0x hex".to_string())
        })?;
        let bytes = code_hex
            .parse::<Bytes>()
            .map_err(|_| format!("bad code hex for {addr}"))?;
        if !bytes.is_empty() {
            code.push((*addr, bytes));
        }
    }
    let mut storage = Vec::with_capacity(slots.len());
    for slot in slots {
        let value = rpc_call(
            client,
            rpc_url,
            "eth_getStorageAt",
            json!([
                slot.address.to_string(),
                format!("0x{:x}", slot.slot),
                block
            ]),
        )
        .await
        .and_then(|v| parse_hex_u256(&v))?;
        storage.push((slot.address, slot.slot, value));
    }
    Ok(ForkedGenesis {
        chain_id,
        alloc,
        code,
        storage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slot_refs() {
        let slot = parse_slot("0x70997970C51812dc3A010C7d01b50e0d17dc79C8:0x0").expect("slot");
        let want: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            .parse()
            .expect("addr");
        assert_eq!(slot.address, want);
        assert_eq!(slot.slot, U256::ZERO);
        assert!(parse_slot("no-colon-here").is_err());
        assert!(parse_slot("0xZZZ:0x0").is_err());
        assert!(parse_slot("0x70997970C51812dc3A010C7d01b50e0d17dc79C8:xyz").is_err());
    }
}
