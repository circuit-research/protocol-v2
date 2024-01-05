use solana_client::rpc_filter::{RpcFilterType, Memcmp};
use anchor_lang::Discriminator;
use drift::state::user::User;

pub fn get_user_filter() -> RpcFilterType {
    return RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        0,
        User::discriminator().into(),
    ))
}

pub fn get_non_idle_user_filter() -> RpcFilterType {
    return RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        4350,
        vec![1],
    ))
}

pub fn get_user_with_auction_filter() -> RpcFilterType {
    return RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        4354,
        vec![1],
    ))
}