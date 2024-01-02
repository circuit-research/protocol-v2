use std::{
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};

use anchor_lang::{AnchorDeserialize, Discriminator};
use drift_program::state::events::OrderActionRecord;
use futures_util::{future::BoxFuture, stream::FuturesOrdered, FutureExt, Stream, StreamExt};
use log::debug;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_client::GetConfirmedSignaturesForAddress2Config,
};
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature,
    transaction::VersionedTransaction,
};
use solana_transaction_status::{
    option_serializer::OptionSerializer, EncodedTransactionWithStatusMeta, UiTransactionEncoding,
};
use tokio::sync::mpsc::{channel, Receiver};

use crate::{constants, types::SdkResult};

impl EventRpcProvider for RpcClient {
    fn get_tx(
        &self,
        signature: Signature,
    ) -> BoxFuture<SdkResult<EncodedTransactionWithStatusMeta>> {
        async move {
            let result = self
                .get_transaction_with_config(
                    &signature,
                    solana_client::rpc_config::RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: None,
                    },
                )
                .await?;

            Ok(result.transaction)
        }
        .boxed()
    }
    fn get_tx_signatures(
        &self,
        account: Pubkey,
        after: Option<Signature>,
        limit: Option<usize>,
    ) -> BoxFuture<SdkResult<Vec<Signature>>> {
        async move {
            let results = self
                .get_signatures_for_address_with_config(
                    &account,
                    GetConfirmedSignaturesForAddress2Config {
                        until: after,
                        limit,
                        ..Default::default()
                    },
                )
                .await?;

            Ok(results
                .iter()
                .map(|r| Signature::from_str(r.signature.as_str()).expect("ok"))
                .collect())
        }
        .boxed()
    }
}

/// RPC functions required for drift event subscriptions
pub trait EventRpcProvider: Send + Sync + 'static {
    /// Fetch tx signatures of account
    /// `after` only return txs more recent than this signature, if given
    /// `limit` return at most this many signatures, if given
    fn get_tx_signatures(
        &self,
        account: Pubkey,
        after: Option<Signature>,
        limit: Option<usize>,
    ) -> BoxFuture<SdkResult<Vec<Signature>>>;
    /// Fetch tx with `signature`
    fn get_tx(
        &self,
        signature: Signature,
    ) -> BoxFuture<SdkResult<EncodedTransactionWithStatusMeta>>;
}

pub struct EventSubscriber<T: EventRpcProvider> {
    provider: &'static T,
}

impl<T: EventRpcProvider> EventSubscriber<T> {
    pub fn new(provider: T) -> Self {
        Self {
            provider: Box::leak(Box::new(provider)),
        }
    }

    /// Subscribe to drift events of `account`
    ///
    /// it uses an RPC polling mechanism to fetch the events
    pub fn subscribe(&self, account: Pubkey) -> DriftEventStream {
        DriftEventStream::new(self.provider, account)
    }
}

impl<T: EventRpcProvider> Drop for EventSubscriber<T> {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw((self.provider as *const T) as *mut T));
        }
    }
}

/// Provides a stream API of drift account events
pub struct DriftEventStream(Receiver<DriftEvent>);

impl DriftEventStream {
    pub fn new(provider: &'static impl EventRpcProvider, account: Pubkey) -> Self {
        let (event_tx, event_rx) = channel(32);

        // spawn the event subscription task
        tokio::spawn(async move {
            let mut last_seen_tx = Option::<Signature>::None;
            let mut futs = FuturesOrdered::new();

            loop {
                // TODO: on first start up should only consider events newer than current timestamp
                let signatures = provider
                    .get_tx_signatures(account, last_seen_tx, Some(5))
                    .await
                    .unwrap();
                // process in LIFO order
                for sig in signatures {
                    futs.push_front(async move { (sig, provider.get_tx(sig).await) });
                }

                // TODO: handle error/retry
                while let Some((sig, Ok(response))) = futs.next().await {
                    let _ = last_seen_tx.insert(sig);
                    if response.meta.is_none() {
                        continue;
                    }
                    if let Some(VersionedTransaction { message, .. }) =
                        response.transaction.decode()
                    {
                        // only txs interacting with drift program
                        if !message
                            .static_account_keys()
                            .iter()
                            .any(|k| k == &constants::PROGRAM_ID)
                        {
                            continue;
                        }
                    }

                    if let OptionSerializer::Some(logs) = response.meta.unwrap().log_messages {
                        for log in logs {
                            try_parse_log(log.as_str()).map(|e| {
                                event_tx.try_send(e).expect("capacity");
                            });
                        }
                    }
                }
                // don't spam the RPC nor spin lock the tokio thread
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        Self(event_rx)
    }
}

impl Stream for DriftEventStream {
    type Item = DriftEvent;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.as_mut().0.poll_recv(cx)
    }
}

pub enum DriftEvent {
    OAR(OrderActionRecord),
}

impl DriftEvent {
    /// Deserialize drift event by discriminant
    fn from_discriminant(disc: [u8; 8], data: &mut &[u8]) -> Option<Self> {
        match disc {
            // deser should only fail on a breaking protocol change
            OrderActionRecord::DISCRIMINATOR => Some(Self::OAR(
                OrderActionRecord::deserialize(data).expect("deserializes"),
            )),
            _ => {
                debug!("unhandled event: {disc:?}");
                None
            }
        }
    }
}

const PROGRAM_LOG: &str = "Program log: ";
const PROGRAM_DATA: &str = "Program data: ";

/// Try deserialize a drift event type from raw log string
/// https://github.com/coral-xyz/anchor/blob/9d947cb26b693e85e1fd26072bb046ff8f95bdcf/client/src/lib.rs#L552
fn try_parse_log(raw: &str) -> Option<DriftEvent> {
    // Log emitted from the current program.
    if let Some(log) = raw
        .strip_prefix(PROGRAM_LOG)
        .or_else(|| raw.strip_prefix(PROGRAM_DATA))
    {
        if let Ok(borsh_bytes) = anchor_lang::__private::base64::decode(log) {
            let (disc, mut data) = borsh_bytes.split_at(8);
            let disc: [u8; 8] = disc.try_into().unwrap();

            return DriftEvent::from_discriminant(disc, &mut data);
        }
    }

    None
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn event_streaming() {
        let event_subscriber =
            EventSubscriber::new(RpcClient::new("https://api.devnet.solana.com".into()));

        let mut event_stream = event_subscriber
            .subscribe(Pubkey::from_str("9JtczxrJjPM4J1xooxr2rFXmRivarb4BwjNiBgXDwe2p").unwrap());

        while let Some(event) = event_stream.next().await {
            let DriftEvent::OAR(oar) = event;
            dbg!(oar.maker, oar.taker, oar.filler);
            dbg!(oar.ts);
        }
    }
}
