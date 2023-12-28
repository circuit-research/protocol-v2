// Standard Library Imports
use std::sync::Arc;

// External Crate Imports
use anchor_lang::AccountDeserialize;
use base64::{engine::general_purpose, Engine as _};
use futures_util::{future::BoxFuture, StreamExt};
use log::{error, warn};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::{
    nonblocking::pubsub_client::PubsubClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::RpcFilterType,
};
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::task::{spawn_local, LocalSet};

// Internal Crate/Module Imports
use crate::types::{DataAndSlot, SdkResult};


type OnUpdate<T> = Arc<dyn Fn(String, DataAndSlot<T>) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct WebsocketProgramAccountOptions {
    filters: Vec<RpcFilterType>,
    commitment: CommitmentConfig,
    encoding: UiAccountEncoding,
}

pub struct WebsocketProgramAccountSubscriber<T>
where 
    T: AccountDeserialize + core::fmt::Debug + 'static,
{
    _subscription_name: String,
    url: String,
    options: WebsocketProgramAccountOptions,
    on_update: Option<OnUpdate<T>>,
    _resub_timeout_ms: Option<u64>,
    subscribed: bool,
}

impl<T> WebsocketProgramAccountSubscriber<T>
where 
    T: AccountDeserialize + core::fmt::Debug + 'static,
{
    pub fn new(
        subscription_name: String,
        url: String,
        options: WebsocketProgramAccountOptions,
        on_update: Option<OnUpdate<T>>,
        resub_timeout_ms: Option<u64>
    ) -> Self {

        WebsocketProgramAccountSubscriber {
            _subscription_name: subscription_name,
            url,
            options,
            on_update,
            _resub_timeout_ms: resub_timeout_ms,
            subscribed: false, 
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

    pub async fn subscribe(&mut self) -> SdkResult<()> {
        if self.subscribed {
            return Ok(());
        }
        self.subscribed = true;
        self.subscribe_ws().await?;

        Ok(())
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
        let mut latest_slot = 0;
        let on_update = self.on_update.clone();
        let local = LocalSet::new();

        let pubsub = PubsubClient::new(&url).await?;

        local.run_until(async move {
            
            let (mut subscription, _receiver) = pubsub.program_subscribe(
                &drift_program::ID,
                Some(config)
            ).await.unwrap(); 

            while let Some(message) = subscription.next().await {
                let slot = message.context.slot;
                if slot >= latest_slot {
                    latest_slot = slot;
                    let pubkey = message.value.pubkey;
                    let account_data = message.value.account.data;
                    let on_update_clone = on_update.clone();
                    match Self::decode(account_data) {
                        Ok(data) => {
                            spawn_local(async move {
                                let data_and_slot = DataAndSlot { slot, data };
                                if let Some(ref on_update_callback) = on_update_clone {
                                   on_update_callback(pubkey, data_and_slot).await;
                                }
                            });
                        },
                        Err(e) => {
                            error!("Error decoding account data: {e}");
                        }
                    }
                } else {
                    warn!("Received stale data from slot: {slot}");
                }
            }

        }).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::memcmp::{get_user_filter, get_non_idle_user_filter};
    use anchor_client::Cluster;
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
        let cluster = Cluster::from_str(MAINNET_ENDPOINT).unwrap();
        let url = cluster.ws_url().to_string();
        let resub_timeout_ms = 10_000;
        let subscription_name = "Test".to_string();

        async fn on_update(pubkey: String, data: DataAndSlot<User>) {
            dbg!(pubkey);
            dbg!(data);
        }

        let on_update_fn: Arc<dyn Fn(String, DataAndSlot<User>) 
        -> BoxFuture<'static, ()> + Send + Sync> = Arc::new(move |s: String, d: DataAndSlot<User>| {
            Box::pin(async move {
                on_update(s, d).await;
            })
        });

        let mut ws_subscriber = WebsocketProgramAccountSubscriber::new(
            subscription_name,
            url,
            options,
            Some(on_update_fn),
            Some(resub_timeout_ms)
        );

        let _ = ws_subscriber.subscribe().await;

        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
    }
}