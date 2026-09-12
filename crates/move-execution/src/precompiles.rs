// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari post-quantum verification precompiles.
//!
//! Two custom precompiles backed by `kanari-crypto`:
//!
//! | address | scheme | input layout |
//! |---------|--------|----------------|
//! | `0x100` | Falcon-512 (FN-DSA) verify | `pubkey_len:u16BE \|\| pubkey \|\| msg_len:u16BE \|\| msg \|\| sig` |
//! | `0x101` | Dilithium3 (ML-DSA-65) verify | same layout |
//!
//! Output is a 32-byte big-endian boolean (`0` / `1`). ANY failure —
//! malformed input, wrong lengths, bad signature — returns `0`
//! (fail-closed, ecrecover-style). Gas is charged in full regardless.
//!
//! Gas pricing is a dev placeholder, not mainnet economics:
//! Falcon-512 = 3_000 (EIP-8052 ballpark), Dilithium3 = 5_000.

use revm::precompile::{
    EthPrecompileOutput, EthPrecompileResult, Precompile, PrecompileHalt, PrecompileId,
    eth_precompile_fn, u64_to_address,
};
use revm::primitives::{Address, Bytes};

/// Falcon-512 signature verification precompile address.
pub const FALCON512_VERIFY_ADDRESS: Address = u64_to_address(0x100);
/// Dilithium3 (ML-DSA-65) signature verification precompile address.
pub const DILITHIUM3_VERIFY_ADDRESS: Address = u64_to_address(0x101);

/// Flat gas cost of a Falcon-512 verification call.
pub const FALCON512_VERIFY_GAS: u64 = 3_000;
/// Flat gas cost of a Dilithium3 verification call.
pub const DILITHIUM3_VERIFY_GAS: u64 = 5_000;

/// Expected Falcon-512 public key length (bytes).
pub const FALCON512_PUBLIC_KEY_LEN: usize = 897;
/// Expected Dilithium3 (ML-DSA-65) public key length (bytes).
pub const DILITHIUM3_PUBLIC_KEY_LEN: usize = 1952;
/// Expected Dilithium3 (ML-DSA-65) signature length (bytes).
pub const DILITHIUM3_SIGNATURE_LEN: usize = 3309;

eth_precompile_fn!(falcon512_verify_fn, falcon512_verify_run);
eth_precompile_fn!(dilithium3_verify_fn, dilithium3_verify_run);

/// Falcon-512 verification precompile (`0x100`).
pub const FALCON512_VERIFY_FUN: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("KANARI_FALCON512")),
    FALCON512_VERIFY_ADDRESS,
    falcon512_verify_fn,
);

/// Dilithium3 verification precompile (`0x101`).
pub const DILITHIUM3_VERIFY_FUN: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("KANARI_DILITHIUM3")),
    DILITHIUM3_VERIFY_ADDRESS,
    dilithium3_verify_fn,
);

/// All Kanari custom precompiles, for extending the standard set.
pub fn kanari_custom_precompiles() -> [Precompile; 2] {
    [FALCON512_VERIFY_FUN, DILITHIUM3_VERIFY_FUN]
}

use revm::{
    context::{Context, Evm, FrameStack},
    context_interface::{Cfg as CfgTr, ContextTr},
    database::InMemoryDB,
    handler::{
        EthFrame, MainContext, MainnetContext, PrecompileProvider, instructions::EthInstructions,
        precompile_output_to_interpreter_result,
    },
    interpreter::{CallInputs, InterpreterResult, interpreter::EthInterpreter},
    precompile::{PrecompileSpecId, Precompiles},
    primitives::{AddressSet, hardfork::SpecId},
};
use std::borrow::Cow;

/// EVM with the standard Ethereum instruction set plus Kanari PQC precompiles.
pub type KanariEvm<CTX> =
    Evm<CTX, (), EthInstructions<EthInterpreter, CTX>, KanariPrecompiles, EthFrame<EthInterpreter>>;

/// Precompile provider: standard set for the active spec plus Kanari customs.
#[derive(Debug, Clone)]
pub struct KanariPrecompiles {
    inner: Precompiles,
    spec: SpecId,
}

impl KanariPrecompiles {
    /// Build the provider for a hardfork spec (standard set + customs).
    pub fn new(spec: SpecId) -> Self {
        let mut inner = Precompiles::new(PrecompileSpecId::from_spec_id(spec)).clone();
        inner.extend(kanari_custom_precompiles());
        Self { inner, spec }
    }
}

impl<CTX: ContextTr> PrecompileProvider<CTX> for KanariPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as CfgTr>::Spec) -> bool {
        let spec = spec.into();
        if spec == self.spec {
            return false;
        }
        *self = Self::new(spec);
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        let Some(precompile) = self.inner.get(&inputs.bytecode_address) else {
            return Ok(None);
        };
        let output = precompile
            .execute(
                &inputs.input.as_bytes(context),
                inputs.gas_limit,
                inputs.reservoir,
            )
            .map_err(|e| e.to_string())?;
        Ok(Some(precompile_output_to_interpreter_result(
            output,
            inputs.gas_limit,
        )))
    }

    fn warm_addresses(&self) -> &AddressSet {
        self.inner.addresses_set()
    }
}

/// Parameters for [`build_kanari_evm`] (grouped to avoid `too_many_arguments`).
#[derive(Debug, Clone, Copy)]
pub struct KanariEvmParams {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Hardfork spec id.
    pub spec: SpecId,
    /// Block number.
    pub number: u64,
    /// Block timestamp (secs).
    pub timestamp: u64,
    /// Block base fee (wei).
    pub basefee: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Block beneficiary (fee recipient).
    pub beneficiary: Address,
}

/// Build a Kanari EVM over a borrowed database with chain cfg + block env set.
pub fn build_kanari_evm(
    db: &mut InMemoryDB,
    params: KanariEvmParams,
) -> KanariEvm<MainnetContext<&mut InMemoryDB>> {
    use revm::primitives::U256;
    let KanariEvmParams {
        chain_id,
        spec,
        number,
        timestamp,
        basefee,
        gas_limit,
        beneficiary,
    } = params;
    let ctx = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
            cfg.spec = spec;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(number);
            block.timestamp = U256::from(timestamp);
            block.beneficiary = beneficiary;
            block.basefee = basefee;
            block.gas_limit = gas_limit;
            block.difficulty = U256::ZERO;
            block.prevrandao = Some(revm::primitives::B256::ZERO);
        });
    let eth_spec = spec;
    Evm {
        ctx,
        inspector: (),
        instruction: EthInstructions::new_mainnet_with_spec(eth_spec),
        precompiles: KanariPrecompiles::new(spec),
        frame_stack: FrameStack::new_prealloc(8),
    }
}

/// Traced Kanari EVM: same interpreter + precompiles, pluggable inspector
/// (e.g. revm's EIP-3155 tracer for `debug_trace*`).
pub type KanariTracedEvm<'db, INSP> = Evm<
    MainnetContext<&'db mut InMemoryDB>,
    INSP,
    EthInstructions<EthInterpreter, MainnetContext<&'db mut InMemoryDB>>,
    KanariPrecompiles,
    EthFrame<EthInterpreter>,
>;

/// Build a traced Kanari EVM over a borrowed database. Unbounded generic:
/// `InspectEvm` bounds are checked where the tracer actually runs.
pub fn build_kanari_evm_traced<'db, INSP>(
    db: &'db mut InMemoryDB,
    params: KanariEvmParams,
    inspector: INSP,
) -> KanariTracedEvm<'db, INSP> {
    use revm::primitives::U256;
    let KanariEvmParams {
        chain_id,
        spec,
        number,
        timestamp,
        basefee,
        gas_limit,
        beneficiary,
    } = params;
    let ctx = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
            cfg.spec = spec;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(number);
            block.timestamp = U256::from(timestamp);
            block.beneficiary = beneficiary;
            block.basefee = basefee;
            block.gas_limit = gas_limit;
            block.difficulty = U256::ZERO;
            block.prevrandao = Some(revm::primitives::B256::ZERO);
        });
    Evm {
        ctx,
        inspector,
        instruction: EthInstructions::new_mainnet_with_spec(spec),
        precompiles: KanariPrecompiles::new(spec),
        frame_stack: FrameStack::new_prealloc(8),
    }
}

fn falcon512_verify_run(input: &[u8], gas_limit: u64) -> EthPrecompileResult {
    run_pqc_verify(
        input,
        gas_limit,
        FALCON512_VERIFY_GAS,
        FALCON512_PUBLIC_KEY_LEN,
        None,
        verify_falcon512_bytes,
    )
}

fn dilithium3_verify_run(input: &[u8], gas_limit: u64) -> EthPrecompileResult {
    run_pqc_verify(
        input,
        gas_limit,
        DILITHIUM3_VERIFY_GAS,
        DILITHIUM3_PUBLIC_KEY_LEN,
        Some(DILITHIUM3_SIGNATURE_LEN),
        verify_dilithium3_bytes,
    )
}

/// Shared verify driver: parse length-prefixed input, charge flat gas,
/// return a 32-byte boolean (fail-closed `0` on any failure).
fn run_pqc_verify(
    input: &[u8],
    gas_limit: u64,
    cost: u64,
    expected_pubkey_len: usize,
    expected_sig_len: Option<usize>,
    verify: impl FnOnce(&[u8], &[u8], &[u8]) -> bool,
) -> EthPrecompileResult {
    if cost > gas_limit {
        return Err(PrecompileHalt::OutOfGas);
    }
    let ok = parse_verify_input(input, expected_pubkey_len, expected_sig_len)
        .map(|(pubkey, msg, sig)| verify(pubkey, msg, sig))
        .unwrap_or(false);
    let mut word = [0u8; 32];
    word[31] = u8::from(ok);
    Ok(EthPrecompileOutput::new(
        cost,
        Bytes::copy_from_slice(&word),
    ))
}

/// Parse `pubkey_len:u16BE || pubkey || msg_len:u16BE || msg || sig`.
fn parse_verify_input(
    input: &[u8],
    expected_pubkey_len: usize,
    expected_sig_len: Option<usize>,
) -> Option<(&[u8], &[u8], &[u8])> {
    let (pubkey_len, rest) = split_u16_prefix(input)?;
    let (msg_len, _) = split_u16_prefix(rest.get(pubkey_len..)?)?;
    let pubkey = rest.get(..pubkey_len)?;
    if pubkey.len() != expected_pubkey_len {
        return None;
    }
    let after_pubkey = &rest[pubkey_len + 2..];
    let msg = after_pubkey.get(..msg_len)?;
    let sig = after_pubkey.get(msg_len..)?;
    if sig.is_empty() {
        return None;
    }
    if let Some(expected) = expected_sig_len
        && sig.len() != expected
    {
        return None;
    }
    Some((pubkey, msg, sig))
}

fn split_u16_prefix(input: &[u8]) -> Option<(usize, &[u8])> {
    let len_bytes: [u8; 2] = input.get(..2)?.try_into().ok()?;
    let len = u16::from_be_bytes(len_bytes) as usize;
    Some((len, &input[2..]))
}

fn verify_falcon512_bytes(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    kanari_crypto::signatures::falcon::verify_signature_falcon512(
        &kanari_evm_types::hex_encode(pubkey),
        msg,
        sig,
    )
    .unwrap_or(false)
}

fn verify_dilithium3_bytes(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    kanari_crypto::signatures::dilithium3::verify_signature_dilithium3(
        &kanari_evm_types::hex_encode(pubkey),
        msg,
        sig,
    )
    .unwrap_or(false)
}
