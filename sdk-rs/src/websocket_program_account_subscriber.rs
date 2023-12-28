// Standard Library Imports
use std::sync::{Arc, Mutex};
use core::pin::Pin;

// External Crate Imports
use anchor_lang::AccountDeserialize;
use base64::{engine::general_purpose, Engine as _};
use events_emitter::EventEmitter;
use futures_util::{Future, StreamExt};
use log::{error, warn};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::{
    nonblocking::pubsub_client::PubsubClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::RpcFilterType,
};
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::task::LocalSet;

// Internal Crate/Module Imports
use crate::types::{DataAndSlot, SdkResult};


pub type OnUpdate<T> = Arc<dyn Fn(Option<SafeEventEmitter<T>>, String, DataAndSlot<T>) + Send + Sync>;

pub type SafeEventEmitter<T> = Arc<Mutex<EventEmitter<(String, DataAndSlot<T>)>>>;

pub struct WebsocketProgramAccountOptions {
    pub filters: Vec<RpcFilterType>,
    pub commitment: CommitmentConfig,
    pub encoding: UiAccountEncoding,
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
    pub subscribed: bool,
    event_emitter: Option<SafeEventEmitter<T>>,
    unsubscriber: Option<Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>>,
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
        event_emitter: Option<SafeEventEmitter<T>>,
        resub_timeout_ms: Option<u64>
    ) -> Self {

        WebsocketProgramAccountSubscriber {
            _subscription_name: subscription_name,
            url,
            options,
            on_update,
            _resub_timeout_ms: resub_timeout_ms,
            subscribed: false, 
            event_emitter,
            unsubscriber: None,
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

        let event_emitter = self.event_emitter.clone();

        local.run_until(async move {
            
            let (mut accounts, unsubscribe) = pubsub.program_subscribe(
                &drift_program::ID,
                Some(config)
            ).await.unwrap(); 
            
            self.unsubscriber = Some(unsubscribe);
            
            while let Some(message) = accounts.next().await {
                let slot = message.context.slot;
                if slot >= latest_slot {
                    latest_slot = slot;
                    let pubkey = message.value.pubkey;
                    let account_data = message.value.account.data;
                    let on_update_clone = on_update.clone();
                    match Self::decode(account_data) {
                        Ok(data) => {
                            let data_and_slot = DataAndSlot { slot, data };
                            if let Some(ref on_update_callback) = on_update_clone {
                                on_update_callback(event_emitter.clone(), pubkey, data_and_slot);                          
                            }
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

    pub async fn unsubscribe(&mut self) -> SdkResult<()> {
        if let Some(unsubscriber) = self.unsubscriber.take() {
            let future = (unsubscriber)();
            future.await;
        }
        self.subscribed = false;
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

        fn on_update(_emitter: Option<SafeEventEmitter<User>>, pubkey: String, data: DataAndSlot<User>) {
            dbg!(pubkey);
            dbg!(data);
        }

        let on_update_fn: OnUpdate<User> = Arc::new(move |emitter, s, d| {
            on_update(emitter, s, d); 
        });

        let mut ws_subscriber = WebsocketProgramAccountSubscriber::new(
            subscription_name,
            url,
            options,
            Some(on_update_fn),
            None,
            Some(resub_timeout_ms)
        );

        let _ = ws_subscriber.subscribe().await;
    }
}