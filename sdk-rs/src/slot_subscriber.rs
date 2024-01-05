use std::sync::Arc;

use events_emitter::EventEmitter;
use futures_util::StreamExt;
use log::{warn, info, error};
use solana_client::{nonblocking::pubsub_client::PubsubClient, rpc_response::SlotInfo};
use tokio::sync::Mutex;

use crate::{websocket_program_account_subscriber::{SafeEventEmitter, OnUpdate}, types::{SdkResult, DataAndSlot}};


pub struct SlotSubscriber {
    pub event_emitter: SafeEventEmitter<SlotInfo>,
    url: String,
    current_slot: Arc<Mutex<u64>>,
    subscribed: bool,
    unsubscriber: Option<tokio::sync::mpsc::Sender<()>>
}

impl SlotSubscriber {
    pub fn new(url: String) -> Self {
        let safe_event_emitter: SafeEventEmitter<SlotInfo> = Arc::new(std::sync::Mutex::new(EventEmitter::new()));

        SlotSubscriber{
            event_emitter: safe_event_emitter,
            url,
            current_slot: Arc::new(Mutex::new(0)),
            subscribed: false,
            unsubscriber: None
        }
    }
    
    fn on_update(emitter: SafeEventEmitter<SlotInfo>, data: DataAndSlot<SlotInfo>) {
        let mut emitter = emitter.lock().unwrap();
        emitter.emit("Slot", &("Slot".to_string(), data));
    }
    
    pub async fn subscribe(&mut self) -> SdkResult<()> {
        let pubsub: PubsubClient = PubsubClient::new(&self.url).await?;

        let (unsub_tx, mut unsub_rx) = tokio::sync::mpsc::channel::<()>(1);
        
        self.subscribed = true;
        self.unsubscriber = Some(unsub_tx);

        let current_slot_shared = self.current_slot.clone();

        let event_emitter = self.event_emitter.clone();

        let on_update_fn: OnUpdate<SlotInfo> = Arc::new(move |emitter, _, d| {
           SlotSubscriber::on_update(emitter.unwrap(), d);
        });

        tokio::spawn(async move {
            let (mut slot_notifications, unsubscriber) = pubsub.slot_subscribe().await.unwrap();
            loop {
                tokio::select! {
                    slot_info = slot_notifications.next() => {
                        match slot_info {
                            Some(slot_info) => {
                                let mut current_slot = current_slot_shared.lock().await;
                                *current_slot = slot_info.slot;
                                let data_and_slot = DataAndSlot { 
                                    slot: slot_info.slot, 
                                    data: slot_info 
                                };
                                on_update_fn(Some(event_emitter.clone()), "".to_string(), data_and_slot);
                            }
                            None => {
                                warn!("slot subscription ended");
                                let future = unsubscriber();
                                future.await;
                                break;
                            }
                        }
                    }
                    _ = unsub_rx.recv() => {
                        info!("Unsubscribing from slots");
                        let future = unsubscriber();
                        future.await;
                        break;
                    }
                }
            }

        });

        Ok(())
    }

    pub async fn unsubscribe(&mut self) -> SdkResult<()> {
        if self.subscribed && self.unsubscriber.is_some() {
            if let Err(e) = self.unsubscriber.clone().unwrap().send(()).await {
                error!("Failed to send unsubscribe signal: {:?}", e);
                return Err(crate::types::SdkError::CouldntUnsubscribe(e)); 
            }
            self.subscribed = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // this is my (frank) free helius endpoint
    const MAINNET_ENDPOINT: &str = "wss://mainnet.helius-rpc.com/?api-key=3a1ca16d-e181-4755-9fe7-eac27579b48c";

    #[tokio::test]
    async fn test_subscribe() {
        env_logger::init();

        let mut slot_subscriber = SlotSubscriber::new(MAINNET_ENDPOINT.to_string());
        let mut emitter = slot_subscriber.event_emitter.lock().unwrap();

        emitter.on("Slot", |(_, d)| {
            dbg!(d);
        });

        drop(emitter);
        
        let _ = slot_subscriber.subscribe().await;
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        let _ = slot_subscriber.unsubscribe().await;
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    }
}