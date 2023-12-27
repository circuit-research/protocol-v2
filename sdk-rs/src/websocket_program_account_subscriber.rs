use std::pin::Pin;
use futures_util::Future;
use solana_client::rpc_filter::{RpcFilterType, Memcmp};
use solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::Client;
use crate::types::DataAndSlot;
use drift_program::state::user::User;

pub struct WebsocketProgramAccountOptions {
    filters: Vec<RpcFilterType>,
    commitment: CommitmentConfig,
    encoding: String,
}

pub struct WebsocketProgramAccountSubscriber<T, C> {
    subscription_name: String,
    account_discriminator: String,
    client: Client<C>,
    options: WebsocketProgramAccountOptions,
    on_update: Option<Box<dyn Fn(String, DataAndSlot<T>) -> Pin<Box<dyn Future<Output = ()> + Send>>>>,
    decode: Option<Box<dyn Fn(&[u8]) -> T>>,
    resub_timeout_ms: Option<u64>
}

impl<T, C> WebsocketProgramAccountSubscriber<T, C> {
    pub fn new(
        subscription_name: String,
        account_discriminator: String,
        client: Client<C>,
        options: WebsocketProgramAccountOptions,
        on_update: Option<Box<dyn Fn(String, DataAndSlot<T>) -> Pin<Box<dyn Future<Output = ()> + Send>>>>,
        decode: Option<Box<dyn Fn(&[u8]) -> T>>,
        resub_timeout_ms: Option<u64>
    ) -> Self {
        WebsocketProgramAccountSubscriber {
            subscription_name,
            account_discriminator,
            client,
            options,
            on_update,
            decode,
            resub_timeout_ms
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::memcmp::{get_user_filter, get_non_idle_user_filter};
    use anchor_client::Cluster;
    use futures_util::future::BoxFuture;
    use solana_sdk::signer::keypair::Keypair;
    use std::str::FromStr;

    // this is my (frank) free helius endpoint
    const MAINNET_ENDPOINT: &str = "https://mainnet.helius-rpc.com/?api-key=3a1ca16d-e181-4755-9fe7-eac27579b48c";

    #[test]
    fn test_create_subscriber() {
        let filters = vec![get_user_filter(), get_non_idle_user_filter()];
        let commitment = CommitmentConfig::confirmed();
        let encoding = "base64".to_string();
        let options = WebsocketProgramAccountOptions {
            filters,
            commitment,
            encoding
        };
        let keypair = std::rc::Rc::new(Keypair::new());
        let cluster = Cluster::from_str(MAINNET_ENDPOINT).unwrap();
        let client = Client::new_with_options(cluster, keypair, CommitmentConfig::confirmed());
        
        fn decode(_: &[u8]) -> User {
            User::default()
        }
        
        async fn on_update(_: String, _: DataAndSlot<User>) {
            println!("Updating");
        }

        let resub_timeout_ms = 10_000;
        let subscription_name = "Test".to_string();
        let account_discriminator = "User".to_string();
        
        let on_update_fn = Box::new(|_s, _d| {
            let fut: BoxFuture<()> = Box::pin(async move {
                on_update(_s, _d).await;
            });
            fut
        });
        let decode_fn = Box::new(decode);

        let websocket_program_account_subscriber = WebsocketProgramAccountSubscriber {
            subscription_name,
            account_discriminator,
            client,
            options,
            on_update: Some(on_update_fn),
            decode: Some(decode_fn),
            resub_timeout_ms: Some(resub_timeout_ms)
        };
    }
}