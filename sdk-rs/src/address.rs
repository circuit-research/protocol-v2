use drift::state::user::UserStats;
use solana_sdk::pubkey::Pubkey;

use crate::types::ReferrerInfo;

pub fn get_user_stats_key(authority: Pubkey) -> Pubkey {
    return Pubkey::find_program_address(
        &[
            b"user_stats",
            authority.as_ref(),
        ], 
        &drift::ID).0
}

pub fn get_referrer_info(taker_stats: UserStats) -> Option<ReferrerInfo> {
    if taker_stats.referrer == Pubkey::default() {
        return None
    } else {
        return Some(ReferrerInfo {
            referrer: taker_stats.referrer,
            referrer_stats: get_user_stats_key(taker_stats.referrer)
        })
    }
}