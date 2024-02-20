//! liquidation and margin helpers
//!

use std::ops::Neg;

use anchor_lang::{prelude::AccountInfo, AnchorDeserialize};
use drift_program::{
    ids::pyth_program,
    instructions::optional_accounts::AccountMaps,
    math::{
        casting::Cast,
        constants::{
            BASE_PRECISION_I64, MARGIN_PRECISION, QUOTE_PRECISION_I64, SPOT_WEIGHT_PRECISION,
        },
        margin::{
            calculate_margin_requirement_and_total_collateral_and_liability_info,
            MarginRequirementType,
        },
    },
    state::{
        margin_calculation::MarginContext,
        oracle_map::OracleMap,
        perp_market::PerpMarket,
        perp_market_map::{MarketSet, PerpMarketMap},
        spot_market::SpotMarket,
        spot_market_map::SpotMarketMap,
        state::State,
        user::{PerpPosition, SpotPosition, User},
    },
};
use fnv::FnvHashSet;
use solana_client::rpc_response::Response;
use solana_sdk::{
    account::{Account, ReadableAccount},
    pubkey::Pubkey,
};

use crate::{constants, AccountProvider, DriftClient, SdkError, SdkResult};

/// Builds an AccountMap of relevant spot, perp, and oracle accounts from rpc
#[derive(Default)]
struct AccountMapBuilder {
    accounts: Vec<Account>,
    account_keys: Vec<Pubkey>,
}

impl AccountMapBuilder {
    /// Constructs the account map + drift state account
    pub async fn build<T: AccountProvider>(
        &mut self,
        client: &DriftClient<T>,
        user: &User,
    ) -> SdkResult<AccountMaps> {
        let mut oracles = FnvHashSet::<Pubkey>::default();
        let mut spot_markets_count = 0_usize;
        let mut perp_markets_count = 0_usize;

        for p in user.spot_positions.iter().filter(|p| !p.is_available()) {
            let market = *client
                .program_data()
                .spot_market_config_by_index(p.market_index)
                .unwrap();
            self.account_keys.push(market.pubkey);
            oracles.insert(market.oracle);
            spot_markets_count += 1;
        }

        for p in user.perp_positions.iter().filter(|p| !p.is_available()) {
            let market = *client
                .program_data()
                .perp_market_config_by_index(p.market_index)
                .unwrap();
            self.account_keys.push(market.pubkey);
            oracles.insert(market.amm.oracle);
            perp_markets_count += 1;
        }

        self.account_keys.extend(oracles.iter());
        self.account_keys.push(*constants::state_account());

        let Response { context, value } = client
            .inner()
            .get_multiple_accounts_with_config(self.account_keys.as_slice(), Default::default())
            .await?;

        self.accounts = value.into_iter().flatten().collect();

        if self.accounts.len() != self.account_keys.len() {
            return Err(SdkError::InvalidAccount);
        }
        let mut accounts_iter = self.account_keys.iter().zip(self.accounts.iter_mut());

        let mut spot_accounts = Vec::<AccountInfo>::with_capacity(spot_markets_count);
        for _ in 0..spot_markets_count {
            let (pubkey, acc) = accounts_iter.next().unwrap();
            spot_accounts.push(AccountInfo::new(
                pubkey,
                false,
                false,
                &mut acc.lamports,
                &mut acc.data[..],
                &constants::PROGRAM_ID,
                false,
                0,
            ));
        }

        let mut perp_accounts = Vec::<AccountInfo>::with_capacity(perp_markets_count);
        for _ in 0..perp_markets_count {
            let (pubkey, acc) = accounts_iter.next().unwrap();
            perp_accounts.push(AccountInfo::new(
                pubkey,
                false,
                false,
                &mut acc.lamports,
                &mut acc.data[..],
                &constants::PROGRAM_ID,
                false,
                0,
            ));
        }

        let mut oracle_accounts = Vec::<AccountInfo>::with_capacity(oracles.len());
        for _ in 0..oracles.len() {
            let (pubkey, acc) = accounts_iter.next().unwrap();
            oracle_accounts.push(AccountInfo::new(
                pubkey,
                false,
                false,
                &mut acc.lamports,
                &mut acc.data[..],
                &pyth_program::ID, // this could be wrong but it doesn't really matter for the liquidity calculation
                false,
                0,
            ));
        }

        let perp_market_map =
            PerpMarketMap::load(&MarketSet::default(), &mut perp_accounts.iter().peekable())
                .map_err(|err| SdkError::Anchor(Box::new(err.into())))?;
        let spot_market_map =
            SpotMarketMap::load(&MarketSet::default(), &mut spot_accounts.iter().peekable())
                .map_err(|err| SdkError::Anchor(Box::new(err.into())))?;

        let (_, state_account) = accounts_iter.next().unwrap();
        let state = State::deserialize(&mut state_account.data()).expect("valid state");
        let oracle_map = OracleMap::load(
            &mut oracle_accounts.iter().peekable(),
            context.slot,
            Some(state.oracle_guard_rails),
        )
        .map_err(|err| SdkError::Anchor(Box::new(err.into())))?;

        Ok(AccountMaps {
            spot_market_map,
            perp_market_map,
            oracle_map,
        })
    }
}

/// Calculate the liquidation price of a user's perp position (given by `market_index`)
///
/// Returns the liquidaton price (PRICE_PRECISION / 1e6)
pub async fn calculate_liquidation_price<'a, T: AccountProvider>(
    client: &DriftClient<T>,
    user: &User,
    market_index: u16,
) -> SdkResult<i64> {
    // TODO: this does a decent amount of rpc queries, it could make sense to cache it e.g. for calculating multiple perp positions
    let mut accounts_builder = AccountMapBuilder::default();
    let account_maps = accounts_builder.build(client, user).await?;
    calculate_liquidation_price_inner(user, market_index, account_maps)
}

fn calculate_liquidation_price_inner(
    user: &User,
    market_index: u16,
    account_maps: AccountMaps<'_>,
) -> SdkResult<i64> {
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = account_maps;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )
    .map_err(|err| SdkError::Anchor(Box::new(err.into())))?;

    // calculate perp free collateral delta
    // TODO should: error if user doesn't have position
    let perp_market = perp_market_map.get_ref(&market_index).unwrap();
    let perp_free_collateral_delta = calculate_perp_free_collateral_delta(
        user.get_perp_position(market_index).unwrap(),
        &perp_market,
    );
    // user holding spot asset case
    let mut spot_free_collateral_delta = 0;
    if let Some(spot_market_index) = spot_market_map
        .0
        .iter()
        .position(|x| x.1.load().is_ok_and(|x| x.oracle == perp_market.amm.oracle))
    {
        if let Ok(spot_position) = user.get_spot_position(spot_market_index as u16) {
            if !spot_position.is_available() {
                let market = spot_market_map
                    .get_ref(&(spot_market_index as u16))
                    .unwrap();
                spot_free_collateral_delta =
                    calculate_spot_free_collateral_delta(spot_position, &market);
            }
        }
    }

    // calculate liquidation price
    // what price delta causes free collateral == 0
    let free_collateral = margin_calculation.get_free_collateral().unwrap();
    let free_collateral_delta = perp_free_collateral_delta + spot_free_collateral_delta;
    if free_collateral == 0 {
        return Ok(-1);
    }
    let liquidation_price_delta =
        (free_collateral as i64 * QUOTE_PRECISION_I64) / free_collateral_delta;

    let oracle_price_data = *oracle_map.get_price_data(&perp_market.amm.oracle).unwrap();
    let liquidation_price = oracle_price_data.price - liquidation_price_delta;
    if liquidation_price < 0 {
        Ok(-1)
    } else {
        Ok(liquidation_price)
    }
}

fn calculate_perp_free_collateral_delta(position: &PerpPosition, market: &PerpMarket) -> i64 {
    let worst_case_base_amount = position.worst_case_base_asset_amount().unwrap();
    let margin_ratio = market
        .get_margin_ratio(
            worst_case_base_amount.unsigned_abs(),
            MarginRequirementType::Maintenance,
        )
        .unwrap();
    let margin_ratio = (margin_ratio as i64 * QUOTE_PRECISION_I64) / MARGIN_PRECISION as i64;

    if worst_case_base_amount > 0 {
        ((QUOTE_PRECISION_I64 - margin_ratio) * worst_case_base_amount as i64) / BASE_PRECISION_I64
    } else {
        ((QUOTE_PRECISION_I64.neg() - margin_ratio) * worst_case_base_amount.abs() as i64)
            / BASE_PRECISION_I64
    }
}

fn calculate_spot_free_collateral_delta(position: &SpotPosition, market: &SpotMarket) -> i64 {
    let market_precision = 10_i64.pow(market.decimals);
    let signed_token_amount = position.get_signed_token_amount(market).unwrap();
    if signed_token_amount > 0 {
        let weight = market
            .get_asset_weight(
                signed_token_amount.unsigned_abs(),
                0, // unused by Maintenance margin type, hence 0
                &MarginRequirementType::Maintenance,
            )
            .unwrap() as i64;
        (((QUOTE_PRECISION_I64 * weight) / SPOT_WEIGHT_PRECISION as i64)
            * signed_token_amount.cast::<i64>().unwrap())
            / market_precision
    } else {
        let weight = market
            .get_liability_weight(
                signed_token_amount.unsigned_abs(),
                &MarginRequirementType::Maintenance,
            )
            .unwrap() as i64;
        (((QUOTE_PRECISION_I64.neg() * weight) / SPOT_WEIGHT_PRECISION as i64)
            * signed_token_amount.abs().cast::<i64>().unwrap())
            / market_precision
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use solana_sdk::signature::Keypair;

    use super::*;
    use crate::{RpcAccountProvider, Wallet};

    #[tokio::test]
    async fn calculate_liq_price() {
        let wallet = Wallet::read_only(
            Pubkey::from_str("DxoRJ4f5XRMvXU9SGuM4ZziBFUxbhB3ubur5sVZEvue2").unwrap(),
        );
        let client = DriftClient::new(
            crate::Context::MainNet,
            RpcAccountProvider::new("https://api.devnet.solana.com"),
            Keypair::new(),
        )
        .await
        .unwrap();
        let user = client
            .get_user_account(&wallet.default_sub_account())
            .await
            .unwrap();
        dbg!(calculate_liquidation_price(&client, &user, 0).await);
    }

    #[test]
    fn liquidation_price_short() {
        calculate_liquidation_price_inner(user, market_index, account_maps);
    }

    #[test]
    fn liquidation_price_long() {}

    #[test]
    fn liquidation_price_short_with_spot_balance() {}

    #[test]
    fn liquidation_price_long_with_spot_balance() {}
}
