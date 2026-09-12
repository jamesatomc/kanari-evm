// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared gas economics for the Kanari EVM workspace: fee constants,
//! EIP-1559 base-fee dynamics, effective-price and burned-share math.
//!
//! Single source of truth — every crate (execution, RPC, tests) computes
//! fees through these, so wallets, validators and receipts can never
//! disagree on what a transaction costs.

use alloy_primitives::U256;

/// Block gas limit for sealed blocks.
pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;

/// Genesis base fee (wei): 1 gwei, the flat fee every chain starts from
/// before EIP-1559 dynamics take over.
pub const GENESIS_BASE_FEE_WEI: u128 = 1_000_000_000;

/// Floor for the dynamic base fee (wei): an idle chain never quotes zero.
pub const MIN_BASE_FEE_WEI: u128 = 1;

/// EIP-1559 maximum base-fee change per block: ±12.5%.
pub const BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 8;

/// EIP-1559 elasticity: gas target is `gas_limit / 2`.
pub const GAS_TARGET_DIVISOR: u128 = 2;

/// Next-block base fee from EIP-1559 dynamics around the gas target.
/// Pure arithmetic — identical on every validator for identical history.
pub fn calc_next_base_fee(parent_fee_wei: u128, gas_used: u128, gas_limit: u128) -> u128 {
    let gas_target = gas_limit / GAS_TARGET_DIVISOR;
    if gas_target == 0 {
        return parent_fee_wei.max(MIN_BASE_FEE_WEI);
    }
    if gas_used == gas_target {
        return parent_fee_wei.max(MIN_BASE_FEE_WEI);
    }
    if gas_used > gas_target {
        let delta = parent_fee_wei.saturating_mul(gas_used - gas_target)
            / gas_target
            / BASE_FEE_MAX_CHANGE_DENOMINATOR;
        parent_fee_wei
            .saturating_add(delta.max(1))
            .max(MIN_BASE_FEE_WEI)
    } else {
        let delta = parent_fee_wei.saturating_mul(gas_target - gas_used)
            / gas_target
            / BASE_FEE_MAX_CHANGE_DENOMINATOR;
        parent_fee_wei.saturating_sub(delta).max(MIN_BASE_FEE_WEI)
    }
}

/// Effective gas price paid per unit of gas: the sender's cap, or
/// base + priority when that is cheaper (1559/7702 semantics).
pub fn effective_gas_price(
    max_fee_per_gas: u128,
    base_fee_wei: u128,
    max_priority_fee_per_gas: u128,
) -> u128 {
    max_fee_per_gas.min(base_fee_wei.saturating_add(max_priority_fee_per_gas))
}

/// Base-fee share of a transaction's fee: what vanilla EIP-1559 destroys
/// and Kanari redirects to the block beneficiary instead. Always exactly
/// `gas_used * base_fee` (the priority share is separate).
pub fn base_fee_share(gas_used: u64, base_fee_wei: u128) -> U256 {
    U256::from(gas_used).saturating_mul(U256::from(base_fee_wei))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u128 = 30_000_000;
    const TARGET: u128 = LIMIT / 2;
    const GWEI: u128 = 1_000_000_000;

    #[test]
    fn at_target_unchanged() {
        assert_eq!(calc_next_base_fee(GWEI, TARGET, LIMIT), GWEI);
    }

    #[test]
    fn empty_block_decays_one_eighth() {
        assert_eq!(calc_next_base_fee(GWEI, 0, LIMIT), GWEI - GWEI / 8);
    }

    #[test]
    fn full_block_rises_one_eighth() {
        assert_eq!(calc_next_base_fee(GWEI, LIMIT, LIMIT), GWEI + GWEI / 8);
    }

    #[test]
    fn floor_never_zero() {
        assert_eq!(calc_next_base_fee(1, 0, LIMIT), 1);
        assert_eq!(calc_next_base_fee(0, 0, LIMIT), 1);
    }

    #[test]
    fn any_over_target_usage_raises() {
        assert!(calc_next_base_fee(GWEI, TARGET + 1, LIMIT) > GWEI);
    }

    #[test]
    fn effective_price_caps_at_max_fee() {
        assert_eq!(effective_gas_price(100, 10, 5), 15);
        assert_eq!(effective_gas_price(12, 10, 5), 12);
        assert_eq!(effective_gas_price(10, 10, 0), 10);
    }

    #[test]
    fn burned_share_is_product() {
        assert_eq!(base_fee_share(21_000, GWEI), U256::from(21_000u128 * GWEI));
        assert_eq!(base_fee_share(0, GWEI), U256::ZERO);
    }
}
