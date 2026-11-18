// SPDX-FileCopyrightText: 2024-2026 P2Poolv2 Developers (see AUTHORS)
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::accounting::OutputPair;
use crate::config::{MAX_CUT_BASIS_POINTS, StratumConfig};
#[cfg(test)]
#[mockall_double::double]
use crate::shares::chain::chain_store_handle::ChainStoreHandle;
#[cfg(not(test))]
use crate::shares::chain::chain_store_handle::ChainStoreHandle;
use bitcoin::{Address, Amount, BlockHash};
use std::collections::HashMap;
use std::error::Error;

/// A trait for implementing a payout distribution.
///
/// payout::simple_pplns implements this trait to provide a payout
/// distribution based on centralised PPLNS algorithm.
///
/// payout::share_chain_payout implements this trait from the share
/// chain data.
#[cfg_attr(test, mockall::automock)]
pub trait PayoutDistribution {
    /// Fill distribution according to the the Payout mechanism.  The
    /// donation and fees amount have already been filled by the trait
    /// common implementation of get_outpoint_distribution
    #[allow(clippy::too_many_arguments)]
    fn fill_distribution_from_shares(
        &mut self,
        distribution: &mut Vec<OutputPair>,
        chain_store_handle: &ChainStoreHandle,
        anchor: BlockHash,
        bitcoin_difficulty: u128,
        total_amount: bitcoin::Amount,
        remaining_total_amount: Amount,
        bootstrap_address: Address,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Generate output distribution based on PPLNS shares weighted by difficulty.
    ///
    /// # Arguments
    /// * `store` - Handle to the chain store for querying PPLNS shares
    /// * `anchor` - Share the payout window is anchored on. The producer
    ///   passes the same tip it commits as `prev_share_blockhash`, so the
    ///   window it pays matches the one the validator reconstructs from
    ///   that prev, closing the read-tip-vs-payout-tip race.
    /// * `bitcoin_difficulty` - Difficulty of the bitcoin block being paid for.
    ///   Only Hydrapool's time-window PPLNS uses it, as its window target; the
    ///   share chain window is `PPLNS_WINDOW_SHARES` mature shares and
    ///   ignores it.
    /// * `total_amount` - Total bitcoin amount to distribute among contributors
    ///
    /// # Returns
    /// Vector of OutputPair containing addresses and their proportional amounts
    fn get_output_distribution(
        &mut self,
        chain_store_handle: &ChainStoreHandle,
        anchor: BlockHash,
        bitcoin_difficulty: u128,
        total_amount: bitcoin::Amount,
        config: &StratumConfig<crate::config::Parsed>,
    ) -> Result<Vec<OutputPair>, Box<dyn Error + Send + Sync>> {
        // Extra two places for potential cuts
        let mut distribution = Vec::<OutputPair>::new();
        let remaining_total_amount = include_address_and_cut(
            &mut distribution,
            total_amount,
            &config.donation_address_parsed,
            config.donation,
        )?;
        let remaining_total_amount = include_address_and_cut(
            &mut distribution,
            remaining_total_amount,
            &config.fee_address_parsed,
            config.fee,
        )?;

        self.fill_distribution_from_shares(
            &mut distribution,
            chain_store_handle,
            anchor,
            bitcoin_difficulty,
            total_amount,
            remaining_total_amount,
            config.bootstrap_address().clone(),
        )?;
        Ok(distribution)
    }
}

/// Appends the cut's output pair to distribution and returns the remaining amount.
///
/// `cut` is in basis points of `total_amount`. A missing or zero cut, or a
/// missing address, appends nothing and returns `total_amount` unchanged.
///
/// Returns an error for a cut above `MAX_CUT_BASIS_POINTS` (100%), which
/// would take more than `total_amount`.
pub(crate) fn include_address_and_cut(
    distribution: &mut Vec<OutputPair>,
    total_amount: bitcoin::Amount,
    address: &Option<Address>,
    cut: Option<u16>,
) -> Result<Amount, Box<dyn Error + Send + Sync>> {
    if let Some(cut_basis_points) = cut.filter(|cut| *cut > MAX_CUT_BASIS_POINTS) {
        return Err(format!(
            "cut of {cut_basis_points} basis points exceeds {MAX_CUT_BASIS_POINTS} basis points"
        )
        .into());
    }
    let (Some(address), Some(cut_basis_points)) = (address.as_ref(), cut.filter(|cut| *cut > 0))
    else {
        return Ok(total_amount);
    };
    //* Compute in u128 to avoid overflow near MAX_MONEY; a cut of at most 100% keeps it within total_amount
    let cut_sats = u128::from(total_amount.to_sat()) * u128::from(cut_basis_points)
        / u128::from(MAX_CUT_BASIS_POINTS);
    let cut_amount = Amount::from_sat(cut_sats as u64);
    distribution.push(OutputPair {
        address: address.clone(),
        amount: cut_amount,
    });
    Ok(total_amount - cut_amount)
}

/// Appends proportional distribution of amount based on difficulty weights to the distribution.
///
/// Entries are sorted by address string to ensure the remainder from
/// integer truncation is always assigned to the same deterministic address.
pub(crate) fn append_proportional_distribution(
    address_difficulty_map: &HashMap<Address, u128>,
    total_amount: bitcoin::Amount,
    distribution: &mut Vec<OutputPair>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let total_difficulty: u128 = address_difficulty_map.values().sum();
    if total_difficulty == 0 {
        return Err(
            "Invalid total difficulty (0) when computing proportional payout"
                .to_string()
                .into(),
        );
    }

    let mut sorted_entries: Vec<(&Address, &u128)> = address_difficulty_map.iter().collect();
    sorted_entries.sort_by(|(address_a, _), (address_b, _)| {
        address_a.to_string().cmp(&address_b.to_string())
    });

    let mut distributed_amount = bitcoin::Amount::ZERO;
    let entry_count = sorted_entries.len();
    let total_sats = total_amount.to_sat() as u128;

    for (index, (address, difficulty)) in sorted_entries.into_iter().enumerate() {
        let amount = if index == entry_count - 1 {
            // Last address gets remainder from truncation
            total_amount - distributed_amount
        } else {
            let amount_sats = (total_sats * *difficulty / total_difficulty) as u64;
            bitcoin::Amount::from_sat(amount_sats)
        };

        distributed_amount += amount;
        distribution.push(OutputPair {
            address: address.clone(),
            amount,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::parse_address_from_string;

    #[test]
    fn test_create_proportional_distribution() {
        let mut address_difficulty_map = HashMap::new();
        let address_a = parse_address_from_string("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq");
        let address_b = parse_address_from_string("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
        address_difficulty_map.insert(address_a.clone(), 600);
        address_difficulty_map.insert(address_b.clone(), 400);

        let total_amount = bitcoin::Amount::from_sat(100_000_000); // 1.0 BTC
        let mut result = Vec::new();
        append_proportional_distribution(&address_difficulty_map, total_amount, &mut result)
            .unwrap();

        assert_eq!(result.len(), 2);

        let total_distributed: bitcoin::Amount = result.iter().map(|op| op.amount).sum();
        assert_eq!(total_distributed, total_amount);

        // Check proportional amounts (60% and 40%)
        let amounts: Vec<_> = result.iter().map(|op| op.amount.to_sat()).collect();
        assert!(amounts.contains(&60_000_000));
        assert!(amounts.contains(&40_000_000));
    }

    #[test]
    fn test_proportional_distribution_remainder_is_deterministic() {
        let address_a = parse_address_from_string("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq");
        let address_b = parse_address_from_string("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
        let address_c = parse_address_from_string("bc1q34aq5drpuwy3wgl9lhup9892qp6svr8ldzyy7c");

        // 3 addresses splitting 1 sat that cannot divide evenly
        let mut address_difficulty_map = HashMap::new();
        address_difficulty_map.insert(address_a.clone(), 1);
        address_difficulty_map.insert(address_b.clone(), 1);
        address_difficulty_map.insert(address_c.clone(), 1);

        // 100 sats / 3 = 33 + 33 + 34 (1 sat remainder)
        let total_amount = bitcoin::Amount::from_sat(100);

        // Run multiple times to confirm the same address always gets the remainder
        let mut remainder_addresses: Vec<Address> = Vec::with_capacity(10);
        for _ in 0..10 {
            let mut result = Vec::new();
            append_proportional_distribution(&address_difficulty_map, total_amount, &mut result)
                .unwrap();

            let total_distributed: bitcoin::Amount = result.iter().map(|op| op.amount).sum();
            assert_eq!(total_distributed, total_amount);

            let remainder_entry = result
                .iter()
                .find(|output_pair| output_pair.amount.to_sat() == 34)
                .expect("one address should receive the 1 sat remainder");
            remainder_addresses.push(remainder_entry.address.clone());
        }

        let first_remainder = &remainder_addresses[0];
        assert!(
            remainder_addresses
                .iter()
                .all(|address| address == first_remainder),
            "remainder should always go to the same address across runs"
        );
    }

    #[test]
    fn test_include_address_and_cut_rejects_cut_just_above_maximum() {
        let address = Some(parse_address_from_string(
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
        ));
        let mut distribution = Vec::with_capacity(1);

        let error = include_address_and_cut(
            &mut distribution,
            Amount::from_sat(100_000_000),
            &address,
            Some(10_001),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("10001 basis points"),
            "got: {error}"
        );
        assert!(distribution.is_empty());
    }

    #[test]
    fn test_include_address_and_cut_rejects_largest_cut() {
        let address = Some(parse_address_from_string(
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
        ));
        let mut distribution = Vec::with_capacity(1);

        let error = include_address_and_cut(
            &mut distribution,
            Amount::from_sat(100_000_000),
            &address,
            Some(u16::MAX),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("65535 basis points"),
            "got: {error}"
        );
        assert!(distribution.is_empty());
    }

    #[test]
    fn test_include_address_and_cut_rejects_cut_above_maximum_without_address() {
        let mut distribution = Vec::with_capacity(1);

        let result = include_address_and_cut(
            &mut distribution,
            Amount::from_sat(100_000_000),
            &None,
            Some(10_001),
        );

        assert!(result.is_err());
        assert!(distribution.is_empty());
    }

    /// MAX_MONEY times the cut overflows u64, so the cut is computed in u128.
    #[test]
    fn test_include_address_and_cut_takes_full_cut_of_max_money() {
        let address = parse_address_from_string("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq");
        let mut distribution = Vec::with_capacity(1);

        let remaining = include_address_and_cut(
            &mut distribution,
            Amount::MAX_MONEY,
            &Some(address.clone()),
            Some(5_000),
        )
        .unwrap();

        assert_eq!(
            distribution[0].amount,
            Amount::from_sat(1_050_000_000_000_000)
        );
        assert_eq!(remaining, Amount::from_sat(1_050_000_000_000_000));
    }

    #[test]
    fn test_include_address_and_cut_allows_one_hundred_percent() {
        let address = parse_address_from_string("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq");
        let total_amount = Amount::from_sat(100_000_000);
        let mut distribution = Vec::with_capacity(1);

        let remaining = include_address_and_cut(
            &mut distribution,
            total_amount,
            &Some(address.clone()),
            Some(10_000),
        )
        .unwrap();

        assert_eq!(remaining, Amount::ZERO);
        assert_eq!(distribution.len(), 1);
        assert_eq!(distribution[0].address, address);
        assert_eq!(distribution[0].amount, total_amount);
    }
}
