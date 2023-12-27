use std::pin::Pin;
use anchor_lang::AccountDeserialize;
use base64::{Engine as _, engine::general_purpose};
use drift_program::state::user::User;
use futures_util::Future;
use solana_account_decoder::{UiAccountEncoding, UiAccountData};
use solana_client::rpc_filter::RpcFilterType;
use solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::Client;
use solana_client::pubsub_client::PubsubClient;
use solana_client::rpc_config::{RpcProgramAccountsConfig, RpcAccountInfoConfig};
use crate::types::{DataAndSlot, SdkResult};
use log::{error, info};


pub trait AccountDecoder {
    type Output;
    fn decode(data: UiAccountData) -> SdkResult<Self::Output>;
}

pub struct DefaultDecoder;

pub struct WebsocketProgramAccountOptions {
    filters: Vec<RpcFilterType>,
    commitment: CommitmentConfig,
    encoding: UiAccountEncoding,
}

pub struct WebsocketProgramAccountSubscriber<T, C>
where 
    T: AccountDeserialize + core::fmt::Debug,
{
    subscription_name: String,
    account_discriminator: String,
    client: Client<C>,
    url: String,
    options: WebsocketProgramAccountOptions,
    on_update: Option<Box<dyn Fn(String, DataAndSlot<T>) -> Pin<Box<dyn Future<Output = ()> + Send>>>>,
    resub_timeout_ms: Option<u64>,
    subscribed: bool,
    latest_slot: u64,
}

impl<T, C> WebsocketProgramAccountSubscriber<T, C>
where 
    T: AccountDeserialize + core::fmt::Debug,
{
    pub fn new(
        subscription_name: String,
        account_discriminator: String,
        client: Client<C>,
        url: String,
        options: WebsocketProgramAccountOptions,
        on_update: Option<Box<dyn Fn(String, DataAndSlot<T>) -> Pin<Box<dyn Future<Output = ()> + Send>>>>,
        resub_timeout_ms: Option<u64>
    ) -> Self {

        WebsocketProgramAccountSubscriber {
            subscription_name,
            account_discriminator,
            client,
            url,
            options,
            on_update,
            resub_timeout_ms,
            subscribed: false, 
            latest_slot: 0,
        }
    }

    #[inline(always)]
    fn decode(data: UiAccountData) -> SdkResult<T> {
        let data_str = match data {
            UiAccountData::Binary(encoded, _) => encoded,
            _ => return Err(crate::types::SdkError::UnsupportedAccountData)
        };

        let decoded_data = general_purpose::STANDARD.decode(data_str)?;
        let mut decoded_data_slice = decoded_data.as_slice();

        let result = T::try_deserialize(&mut decoded_data_slice)?;

        Ok(result)
    }

    pub async fn subscribe(&mut self) {
        if self.subscribed {
            return;
        }
        self.subscribed = true;
        self.subscribe_ws().await;
    }

    async fn subscribe_ws(&mut self) -> SdkResult<()> {
        let account_config = RpcAccountInfoConfig {
            commitment: Some(self.options.commitment),
            encoding: Some(self.options.encoding),
            ..RpcAccountInfoConfig::default()
        };
        let config = RpcProgramAccountsConfig {
            filters: Some(self.options.filters.clone()),
            account_config,
            ..RpcProgramAccountsConfig::default()
        };

        let url = self.url.clone();
        tokio::spawn(async move {
            let (mut _subscription, receiver) = PubsubClient::program_subscribe(
                &url,
                &drift_program::ID,
                Some(config)
            ).unwrap(); // I think unwrapping here is fine because if the connection fails the whole thing is useless
            
            while let Ok(message) = receiver.recv() {
                let slot = message.context.slot;
                let data = message.value.account.data;
                let _data_and_slot = DataAndSlot { slot, data: data.clone() };
                match Self::decode(data.clone()) {
                    Ok(obj) => {
                        dbg!(obj);
                    },
                    Err(e) => {
                        error!("Error decoding account data: {e}");
                    }
                }
            }
        });

        Ok(())
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
    use drift_program::state::user::User;

    // this is my (frank) free helius endpoint
    const MAINNET_ENDPOINT: &str = "https://mainnet.helius-rpc.com/?api-key=3a1ca16d-e181-4755-9fe7-eac27579b48c";

    #[tokio::test]
    async fn test_subscribe() {
        env_logger::init();
        let filters = vec![get_user_filter(), get_non_idle_user_filter()];
        let commitment = CommitmentConfig::confirmed();
        let options = WebsocketProgramAccountOptions {
            filters,
            commitment,
            encoding: UiAccountEncoding::Base64
        };
        let keypair = std::rc::Rc::new(Keypair::new());
        let cluster = Cluster::from_str(MAINNET_ENDPOINT).unwrap();
        let url = cluster.ws_url().to_string();
        let client = Client::new_with_options(cluster, keypair, CommitmentConfig::confirmed());
        
        async fn on_update(_: String, _: DataAndSlot<User>) {}

        let resub_timeout_ms = 10_000;
        let subscription_name = "Test".to_string();
        let account_discriminator = "User".to_string();
        
        let on_update_fn = Box::new(|_s, _d| {
            let fut: BoxFuture<()> = Box::pin(async move {
                on_update(_s, _d).await;
            });
            fut
        });

        let mut ws_subscriber = WebsocketProgramAccountSubscriber::new(
            subscription_name,
            account_discriminator,
            client,
            url,
            options,
            Some(on_update_fn),
            Some(resub_timeout_ms)
        );

        ws_subscriber.subscribe().await;

        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
    }
}