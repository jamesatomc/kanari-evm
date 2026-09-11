// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Minimal Ethereum JSON-RPC over HTTP for the Kanari dev chain.
//!
//! Covers what wallets (e.g. MetaMask) and scripts need: chain id, balances,
//! nonces, gas price, storage slots, sending raw transactions
//! (instant-sealed), receipts, blocks and read-only calls. `eth_getLogs`
//! returns the empty set (receipts carry no logs on this dev chain) so that
//! dApp libraries finish loading instead of failing. Anything else returns
//! `-32601 Method not found`. Block hashes are deterministic placeholders
//! (see `node.rs`).

use crate::evm_execution::{
    execution::CallRequest,
    node::{DEFAULT_BASE_FEE_WEI, KanariNode, NodeError},
};
use alloy_primitives::{Address, B256, Bytes, U256};
use axum::{Json, Router, extract::State, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

pub type SharedNode = Arc<Mutex<KanariNode>>;

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcErrorBody {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcErrorBody>,
}

fn ok(id: Value, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Value, code: i64, message: String) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcErrorBody { code, message }),
    }
}

fn quantity_u64(v: u64) -> Value {
    json!(format!("0x{v:x}"))
}

fn quantity_u256(v: U256) -> Value {
    json!(format!("0x{v:x}"))
}

fn parse_address(v: &Value) -> Result<Address, String> {
    v.as_str()
        .and_then(|s| s.parse::<Address>().ok())
        .ok_or_else(|| "invalid address (want 0x-hex)".to_string())
}

fn parse_hash(v: &Value) -> Result<B256, String> {
    v.as_str()
        .and_then(|s| s.parse::<B256>().ok())
        .ok_or_else(|| "invalid hash (want 0x-hex)".to_string())
}

fn parse_bytes(v: &Value) -> Result<Bytes, String> {
    v.as_str()
        .and_then(|s| s.parse::<Bytes>().ok())
        .ok_or_else(|| "invalid bytes (want 0x-hex)".to_string())
}

fn parse_u64(v: &Value) -> Result<u64, String> {
    let s = v.as_str().ok_or("want 0x quantity")?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|_| "invalid 0x quantity".to_string())
}

fn parse_u256(v: &Value) -> Result<U256, String> {
    let s = v.as_str().ok_or("want 0x quantity")?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|_| "invalid 0x quantity".to_string())
}

fn take_parsed<T>(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse(v).map(Some),
    }
}

impl CallRequest {
    fn from_json(obj: &serde_json::Map<String, Value>) -> Result<Self, String> {
        // `input` is accepted as an alias of `data` (some wallets send it).
        let data = match (obj.get("data"), obj.get("input")) {
            (Some(v), _) | (None, Some(v)) => Some(parse_bytes(v)?),
            (None, None) => None,
        };
        Ok(Self {
            from: take_parsed(obj, "from", parse_address)?,
            to: take_parsed(obj, "to", parse_address)?,
            data,
            value: take_parsed(obj, "value", parse_u256)?,
            gas: take_parsed(obj, "gas", parse_u64)?,
            gas_price: take_parsed(obj, "gasPrice", |v| parse_u256(v).map(|u| u.to::<u128>()))?,
            nonce: take_parsed(obj, "nonce", parse_u64)?,
        })
    }
}

fn node_error(id: Value, e: NodeError) -> RpcResponse {
    err(id, -32000, e.to_string())
}

/// Build the axum router: JSON-RPC at `POST /`, block explorer at `GET /`.
pub fn router(node: SharedNode) -> Router {
    Router::new()
        .route(
            "/",
            post(handle_rpc).options(handle_options).get(explorer_page),
        )
        .with_state(node)
}

/// Static single-file explorer UI (talks to this same origin, so no CORS
/// issues even in browsers).
async fn explorer_page() -> impl axum::response::IntoResponse {
    (
        [("Content-Type", "text/html; charset=utf-8")],
        include_str!("explorer.html"),
    )
}

/// Permissive CORS for browser wallets hitting a LAN dev node.
/// DEV ONLY: echoes any origin, allows POST + Content-Type.
async fn handle_options() -> impl axum::response::IntoResponse {
    (
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Access-Control-Allow-Methods", "POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "Content-Type"),
            ("Access-Control-Max-Age", "86400"),
        ],
        "",
    )
}

fn with_cors(json: Json<Value>) -> impl axum::response::IntoResponse {
    ([("Access-Control-Allow-Origin", "*")], json)
}

/// Accept single requests and batch arrays (wallets routinely batch
/// `eth_chainId` + `eth_blockNumber` + `eth_getBalance` on load).
async fn handle_rpc(
    State(node): State<SharedNode>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    with_cors(inner_handle_rpc(node, body).await)
}

async fn inner_handle_rpc(node: SharedNode, body: Value) -> Json<Value> {
    // One stderr line per call so wallet traffic is observable on dev nodes.
    // Params are truncated; responses log only ok/err + code.
    fn summarize(v: &Value) -> String {
        let s = serde_json::to_string(v).unwrap_or_default();
        const MAX: usize = 160;
        if s.len() > MAX {
            format!("{}…({}B)", &s[..MAX], s.len())
        } else {
            s
        }
    }
    match &body {
        Value::Array(batch) => {
            let names: Vec<&str> = batch
                .iter()
                .map(|item| item.get("method").and_then(|m| m.as_str()).unwrap_or("?"))
                .collect();
            eprintln!("rpc batch len={} methods={names:?}", batch.len());
        }
        Value::Object(_) => {
            eprintln!(
                "rpc {} params={}",
                body.get("method").and_then(|m| m.as_str()).unwrap_or("?"),
                summarize(body.get("params").unwrap_or(&Value::Null)),
            );
        }
        _ => eprintln!("rpc <non-object>"),
    }
    match body {
        Value::Array(batch) => {
            if batch.is_empty() {
                return Json(json!([{
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32600, "message": "empty batch" },
                }]));
            }
            let mut out = Vec::with_capacity(batch.len());
            for item in batch {
                match serde_json::from_value::<RpcRequest>(item) {
                    Ok(req) => {
                        let resp = dispatch(&node, req).await;
                        out.push(serde_json::to_value(resp).unwrap_or(Value::Null));
                    }
                    Err(_) => out.push(json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": "parse error" },
                    })),
                }
            }
            Json(Value::Array(out))
        }
        Value::Object(_) => {
            let req: RpcRequest = match serde_json::from_value(body) {
                Ok(req) => req,
                Err(_) => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": "parse error" },
                    }));
                }
            };
            let resp = dispatch(&node, req).await;
            Json(serde_json::to_value(resp).unwrap_or(Value::Null))
        }
        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32600, "message": "invalid request" },
        })),
    }
}

async fn dispatch(node: &SharedNode, req: RpcRequest) -> RpcResponse {
    let resp = dispatch_inner(node, req).await;
    if let Some(err) = &resp.error {
        eprintln!("rpc -> err {} {}", err.code, err.message);
    }
    resp
}

async fn dispatch_inner(node: &SharedNode, req: RpcRequest) -> RpcResponse {
    let id = req.id.clone();
    let params = req.params.clone();
    match req.method.as_str() {
        "web3_clientVersion" => ok(id, json!("kanari-evm-node/0.2.8")),
        "net_version" => {
            let node = node.lock().await;
            ok(id, json!(node.chain_id().to_string()))
        }
        "eth_chainId" => {
            let node = node.lock().await;
            ok(id, quantity_u64(node.chain_id()))
        }
        "eth_blockNumber" => {
            let node = node.lock().await;
            ok(id, quantity_u64(node.block_number()))
        }
        "eth_syncing" => ok(id, Value::Bool(false)),
        "net_listening" => ok(id, Value::Bool(true)),
        "eth_getCode" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(addr_v) = arr.first() else {
                return err(id, -32602, "want [address, block]".to_string());
            };
            match parse_address(addr_v) {
                Ok(addr) => {
                    let mut node = node.lock().await;
                    match node.code_of(addr) {
                        Ok(code) => ok(id, json!(format!("0x{}", hex::encode(&code)))),
                        Err(e) => node_error(id, e),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_feeHistory" => {
            // Minimal static response: constant base fee, empty blocks. One
            // reward entry per requested percentile (strict clients validate
            // the shape).
            let arr = params.as_array().cloned().unwrap_or_default();
            let count = arr
                .first()
                .and_then(|v| parse_u64(v).ok())
                .unwrap_or(4)
                .clamp(1, 32) as usize;
            let n_rewards = arr
                .get(2)
                .and_then(|v| v.as_array())
                .map(|a| a.len().max(1))
                .unwrap_or(1);
            let node = node.lock().await;
            let latest = node.block_number();
            let oldest = latest.saturating_sub(count as u64 - 1);
            let base = quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI));
            ok(
                id,
                json!({
                    "oldestBlock": quantity_u64(oldest),
                    "baseFeePerGas": vec![base.clone(); count + 1],
                    "gasUsedRatio": vec![0.0; count],
                    "reward": vec![vec!["0x0"; n_rewards]; count],
                }),
            )
        }
        "eth_gasPrice" => ok(id, quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI))),
        "eth_maxPriorityFeePerGas" => ok(id, quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI))),
        "eth_getBalance" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(addr_v) = arr.first() else {
                return err(id, -32602, "want [address, block]".to_string());
            };
            match parse_address(addr_v) {
                Ok(addr) => {
                    let mut node = node.lock().await;
                    match node.balance_of(addr) {
                        Ok(b) => ok(id, quantity_u256(b)),
                        Err(e) => node_error(id, e),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_getTransactionCount" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(addr_v) = arr.first() else {
                return err(id, -32602, "want [address, block]".to_string());
            };
            match parse_address(addr_v) {
                Ok(addr) => {
                    let mut node = node.lock().await;
                    match node.nonce_of(addr) {
                        Ok(n) => ok(id, quantity_u64(n)),
                        Err(e) => node_error(id, e),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_getStorageAt" => {
            // params: [address, slot, block?]. Block is accepted and ignored
            // (only the current state is queryable on this dev chain).
            let arr = params.as_array().cloned().unwrap_or_default();
            let (Some(addr_v), Some(slot_v)) = (arr.first(), arr.get(1)) else {
                return err(id, -32602, "want [address, slot, block?]".to_string());
            };
            let (addr, slot) = match (parse_address(addr_v), parse_u256(slot_v)) {
                (Ok(a), Ok(s)) => (a, s),
                (Err(e), _) | (_, Err(e)) => return err(id, -32602, e),
            };
            let mut node = node.lock().await;
            match node.storage_of(addr, slot) {
                // DATA, 32 bytes: zero-padded hex, not minimal QUANTITY.
                Ok(v) => ok(id, json!(format!("0x{v:064x}"))),
                Err(e) => node_error(id, e),
            }
        }
        "eth_sendRawTransaction" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(raw_v) = arr.first() else {
                return err(id, -32602, "want [rawTx]".to_string());
            };
            match parse_bytes(raw_v) {
                Ok(raw) => {
                    let mut node = node.lock().await;
                    match node.send_raw_transaction(raw) {
                        Ok(hash) => ok(id, json!(hash.to_string())),
                        Err(e) => node_error(id, e),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_getTransactionByHash" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(hash_v) = arr.first() else {
                return err(id, -32602, "want [hash]".to_string());
            };
            match parse_hash(hash_v) {
                Ok(hash) => {
                    let node = node.lock().await;
                    match node.tx_view(&hash) {
                        Some(view) => ok(id, view),
                        None => ok(id, Value::Null),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_getTransactionReceipt" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(hash_v) = arr.first() else {
                return err(id, -32602, "want [hash]".to_string());
            };
            match parse_hash(hash_v) {
                Ok(hash) => {
                    let node = node.lock().await;
                    match node.receipt_view(&hash) {
                        Some(view) => ok(id, view),
                        None => ok(id, Value::Null),
                    }
                }
                Err(e) => err(id, -32602, e),
            }
        }
        "eth_getLogs" => {
            // Receipts carry no logs on this dev chain, so any filter
            // matches the empty set. Returning `[]` (instead of "method not
            // found") keeps wallets and dApp libraries loading.
            ok(id, json!([]))
        }
        "eth_getBlockByHash" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                return err(id, -32602, "want [blockHash, fullTxs]".to_string());
            }
            let full_txs = arr.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let hash = match parse_hash(&arr[0]) {
                Ok(h) => h,
                Err(e) => return err(id, -32602, e),
            };
            let node = node.lock().await;
            match node.block_view_by_hash(&hash, full_txs) {
                Some(view) => ok(id, view),
                None => ok(id, Value::Null),
            }
        }
        "eth_getBlockByNumber" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                return err(id, -32602, "want [blockNumber, fullTxs]".to_string());
            }
            let full_txs = arr.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let number: Option<u64> = match &arr[0] {
                Value::String(s) if s == "latest" || s == "pending" => None,
                Value::String(s) if s == "earliest" => Some(0),
                v => match parse_u64(v) {
                    Ok(n) => Some(n),
                    Err(e) => return err(id, -32602, e),
                },
            };
            let node = node.lock().await;
            let number = number.unwrap_or_else(|| node.block_number());
            match node.block_view(number, full_txs) {
                Some(view) => ok(id, view),
                None => ok(id, Value::Null),
            }
        }
        "eth_call" | "eth_estimateGas" => {
            let arr = params.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                return err(id, -32602, "want [{...}, block]".to_string());
            }
            let obj = arr[0].as_object().cloned().unwrap_or_default();
            let call = match CallRequest::from_json(&obj) {
                Ok(c) => c,
                Err(e) => return err(id, -32602, e),
            };
            let mut node = node.lock().await;
            match node.call(call) {
                Ok(out) => {
                    if req.method.as_str() == "eth_estimateGas" {
                        ok(id, quantity_u64(out.gas_used))
                    } else {
                        ok(id, json!(format!("0x{}", hex::encode(&out.output))))
                    }
                }
                Err(e) => node_error(id, e),
            }
        }
        "kanari_faucet" => {
            // params: [address, ethAmount?]. DEV ONLY, no auth. Sends from
            // the node faucet account and seals immediately.
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(addr_v) = arr.first() else {
                return err(id, -32602, "want [address, ethAmount?]".to_string());
            };
            let address = match parse_address(addr_v) {
                Ok(a) => a,
                Err(e) => return err(id, -32602, e),
            };
            let eth: u128 = match arr.get(1) {
                None | Some(Value::Null) => 100,
                Some(v) => match v
                    .as_str()
                    .and_then(|s| s.parse::<u128>().ok())
                    .or_else(|| v.as_u64().map(|n| n as u128))
                {
                    Some(n) => n,
                    None => {
                        return err(
                            id,
                            -32602,
                            "ethAmount must be a whole-ETH number or 0x quantity".to_string(),
                        );
                    }
                },
            };
            let amount = match eth.checked_mul(1_000_000_000_000_000_000) {
                Some(w) => w,
                None => return err(id, -32602, "ethAmount too large".to_string()),
            };
            let mut node = node.lock().await;
            match node.faucet(address, U256::from(amount)) {
                Ok(hash) => ok(id, json!(hash.to_string())),
                Err(e) => node_error(id, e),
            }
        }
        "kanari_supply" => {
            // No params. Circulating supply is the genesis-allocation sum
            // (no post-genesis minting); max supply is the protocol cap.
            let node = node.lock().await;
            let (total, max) = node.supply();
            ok(
                id,
                json!({
                    "totalSupply": quantity_u256(total),
                    "maxSupply": quantity_u256(max),
                }),
            )
        }
        "kanari_getSmtProof" => {
            // params: [address, slot?] — account proof when slot is absent,
            // storage-slot proof otherwise. All values 0x-hex.
            let arr = params.as_array().cloned().unwrap_or_default();
            let Some(addr_v) = arr.first() else {
                return err(id, -32602, "want [address, slot?]".to_string());
            };
            let address = match parse_address(addr_v) {
                Ok(a) => a,
                Err(e) => return err(id, -32602, e),
            };
            let slot = match arr.get(1) {
                None | Some(Value::Null) => None,
                Some(v) => match parse_u256(v) {
                    Ok(u) => Some(u),
                    Err(e) => return err(id, -32602, e),
                },
            };
            let node = node.lock().await;
            match node.smt_proof(address, slot) {
                Ok(p) => ok(
                    id,
                    json!({
                        "root": p.root.to_string(),
                        "key": p.key.to_string(),
                        "value": p.value.map(|v| format!("0x{}", hex::encode(&v))),
                        "exists": p.exists,
                        "leaf": p.leaf.to_string(),
                        "proof": p.siblings.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    }),
                ),
                Err(e) => node_error(id, e),
            }
        }
        other => err(id, -32601, format!("Method not found: {other}")),
    }
}

/// Hex-encode helper (avoids pulling another hex dependency).
mod hex {
    const CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(CHARS[(b >> 4) as usize] as char);
            out.push(CHARS[(b & 0x0f) as usize] as char);
        }
        out
    }
}
