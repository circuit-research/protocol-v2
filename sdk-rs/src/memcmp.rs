use solana_client::rpc_filter::{RpcFilterType, Memcmp};
use anchor_lang::Discriminator;
use drift_program::state::user::User;

pub fn get_user_filter() -> RpcFilterType {
    return RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        0,
        User::discriminator().into(),
    ))
}

pub fn get_non_idle_user_filter() -> RpcFilterType {
    return RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        4350,
        bs58::encode(&[0]).into_vec(),
    ))
}