// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution traces (`debug_traceTransaction`, `debug_traceCall`).
//!
//! Traces run through revm's own EIP-3155 tracer over a Kanari EVM with the
//! full PQC precompile set, so traced execution matches sealed execution
//! opcode-for-opcode. Historical accuracy comes from deterministic replay:
//! `trace_transaction` rebuilds the exact pre-block state in a scratch
//! database (genesis + prior blocks' raw transactions, same block env)
//! instead of tracing against live head.
//!
//! Honest limitation: only sealed history is replayable. There is no
//! pending-block tracing beyond `debug_traceCall` on current state.

use crate::execution::{PreparedTx, transact_raw};
use crate::node::{KanariNode, NodeError};
use crate::precompiles::build_kanari_evm_traced;
use alloy_primitives::{Address, Bytes, U256};
use revm::{
    context::TxEnv, context_interface::result::ExecutionResult, database::InMemoryDB,
    primitives::TxKind as RevmTxKind,
};
use revm_inspector::InspectEvm;
use revm_inspector::inspectors::TracerEip3155;
use std::sync::{Arc, Mutex};

/// Trace options (mirrors geth's struct-log tracer flags).
#[derive(Debug, Clone, Default)]
pub struct TraceOptions {
    /// Omit the per-step stack (`disableStack`).
    pub disable_stack: bool,
    /// Omit per-step memory (`disableMemory`).
    pub disable_memory: bool,
}

impl TraceOptions {
    /// Parse the `{disableStack?, disableMemory?}` options object.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v.as_object().cloned().unwrap_or_default();
        let flag = |key: &str| match obj.get(key) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{key} must be a boolean")),
        };
        Ok(Self {
            disable_stack: flag("disableStack")?,
            disable_memory: flag("disableMemory")?,
        })
    }
}

/// One traced transaction: Geth-style struct-log output.
#[derive(Debug, Clone)]
pub struct TraceOutput {
    /// Gas used by the transaction (matches the receipt).
    pub gas_used: u64,
    /// True when execution reverted or halted.
    pub failed: bool,
    /// Return data / revert reason, `0x`-hex.
    pub return_value: String,
    /// Per-step logs: `{pc, op, gas, gasCost, depth, stack?, memory?}`.
    pub struct_logs: Vec<serde_json::Value>,
}

impl TraceOutput {
    /// Render the `debug_trace*` result object.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "gas": format!("0x{:x}", self.gas_used),
            "failed": self.failed,
            "returnValue": self.return_value,
            "structLogs": self.struct_logs,
        })
    }
}

/// `Write` sink collecting tracer JSON lines into shared memory.
#[derive(Clone, Default)]
struct SharedBuf {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl KanariNode {
    /// Trace a sealed transaction by re-executing it at its exact
    /// historical state (genesis + all prior blocks replayed into scratch
    /// state with their stored block envs). Fails when the hash is unknown.
    pub fn trace_transaction(
        &mut self,
        hash: &alloy_primitives::B256,
        opts: TraceOptions,
    ) -> Result<TraceOutput, NodeError> {
        let receipt = self
            .receipt(hash)
            .ok_or_else(|| NodeError::InvalidTransaction(format!("unknown transaction {hash}")))?;
        let block_number = receipt.block_number;
        let block = self
            .block_by_number(block_number)
            .ok_or_else(|| NodeError::Storage(format!("missing block {block_number}")))?;
        let (_, raw) = self
            .tx_record(hash)
            .ok_or_else(|| NodeError::Storage(format!("missing raw tx {hash}")))?;
        let prepared = PreparedTx::decode(raw, self.spec.chain_id)?;

        // Rebuild the exact pre-block state in scratch space.
        let mut scratch = InMemoryDB::default();
        self.spec.apply_genesis(&mut scratch);
        for prior in self.blocks.iter().filter(|b| b.number < block_number) {
            for prior_raw in &prior.txs {
                let prior_prepared = PreparedTx::decode(prior_raw, self.spec.chain_id)?;
                transact_raw(
                    &mut scratch,
                    &self.spec,
                    &prior_prepared,
                    prior.number,
                    prior.timestamp,
                    prior.base_fee,
                )?;
            }
        }

        trace_in(
            &mut scratch,
            &self.spec,
            prepared.tx_env(),
            block_number,
            block.timestamp,
            block.base_fee,
            &opts,
        )
    }

    /// Trace a read-only call against current state (never seals).
    pub fn trace_call(
        &mut self,
        call: crate::execution::CallRequest,
        opts: TraceOptions,
    ) -> Result<TraceOutput, NodeError> {
        let caller = call.from.unwrap_or(Address::ZERO);
        let nonce = match call.nonce {
            Some(n) => n,
            None => self.nonce_of(caller).unwrap_or(0),
        };
        let tx = TxEnv {
            caller,
            gas_limit: call.gas.unwrap_or(crate::node::DEFAULT_CALL_GAS),
            gas_price: call.gas_price.unwrap_or_else(|| self.pending_base_fee()),
            kind: match call.to {
                Some(to) => RevmTxKind::Call(to),
                None => RevmTxKind::Create,
            },
            value: call.value.unwrap_or(U256::ZERO),
            data: call.data.unwrap_or_default(),
            nonce,
            chain_id: Some(self.spec.chain_id),
            gas_priority_fee: None,
            access_list: Default::default(),
            ..Default::default()
        };
        let head_number = self.block_number();
        let now = crate::execution::now_secs();
        let head_fee = self.pending_base_fee() as u64;
        trace_in(
            &mut self.db,
            &self.spec,
            tx,
            head_number,
            now,
            head_fee,
            &opts,
        )
    }
}

/// Run one transaction under the EIP-3155 tracer and map its JSON lines to
/// Geth-style struct logs. `skip_summary` lines (state root/time) are
/// dropped; the pass/fail + gas come from the summary line AND the
/// `ExecutionResult` (the result wins on disagreement).
fn trace_in(
    db: &mut InMemoryDB,
    spec: &crate::chainspec::KanariChainSpec,
    tx: TxEnv,
    number: u64,
    timestamp: u64,
    base_fee: u64,
    opts: &TraceOptions,
) -> Result<TraceOutput, NodeError> {
    use crate::precompiles::KanariEvmParams;

    let buf = SharedBuf::default();
    let tracer = TracerEip3155::new(Box::new(buf.clone()));
    let mut evm = build_kanari_evm_traced(
        db,
        KanariEvmParams {
            chain_id: spec.chain_id,
            spec: spec.spec_id,
            number,
            timestamp,
            basefee: base_fee,
            gas_limit: crate::node::BLOCK_GAS_LIMIT,
            beneficiary: crate::node::BLOCK_BENEFICIARY,
        },
        tracer,
    );
    let out = evm
        .inspect_one_tx(tx)
        .map_err(|e| NodeError::Execution(e.to_string()))?;

    let text = String::from_utf8(
        buf.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .map_err(|e| NodeError::Execution(format!("tracer output is not UTF-8: {e}")))?;

    let mut struct_logs = Vec::new();
    let mut summary: Option<serde_json::Value> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| NodeError::Execution(format!("bad tracer line: {e}")))?;
        if value.get("pc").is_some() {
            struct_logs.push(map_step(value, opts));
        } else {
            summary = Some(value);
        }
    }

    let (failed, gas_used, return_value) = match out {
        ExecutionResult::Success { gas, output, .. } => {
            (false, gas.tx_gas_used(), output.into_data())
        }
        ExecutionResult::Revert { gas, output, .. } => (true, gas.tx_gas_used(), output),
        ExecutionResult::Halt { gas, .. } => (true, gas.tx_gas_used(), Bytes::new()),
    };
    // Prefer the summary's gas when present (identical in practice).
    let gas_used = summary
        .as_ref()
        .and_then(|s| s.get("gasUsed"))
        .and_then(|g| g.as_str())
        .and_then(|g| u64::from_str_radix(g.trim_start_matches("0x"), 16).ok())
        .unwrap_or(gas_used);
    Ok(TraceOutput {
        gas_used,
        failed,
        return_value: kanari_evm_types::hex_prefixed(&return_value),
        struct_logs,
    })
}

/// Map one EIP-3155 step line to a Geth struct-log entry.
fn map_step(mut step: serde_json::Value, opts: &TraceOptions) -> serde_json::Value {
    let obj = step.as_object_mut().expect("step is an object");
    // Geth names: op (string), gasCost (number-as-hex here stays hex).
    // Insert first: this overwrites the numeric `op` with the name.
    if let Some(op) = obj.remove("opName") {
        obj.insert("op".to_string(), op);
    }
    obj.remove("reservoir");
    obj.remove("stateGas");
    obj.remove("refund");
    obj.remove("memSize");
    obj.remove("returnData");
    obj.remove("returnStack");
    obj.remove("storage");
    if opts.disable_stack {
        obj.remove("stack");
    }
    if opts.disable_memory {
        obj.remove("memory");
    }
    serde_json::Value::Object(obj.clone())
}
