// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::faucet::write_ahead_log;
use crate::metrics::FaucetMetrics;
use async_recursion::async_recursion;
use async_trait::async_trait;
use mysten_metrics::spawn_monitored_task;
use prometheus::Registry;
use shared_crypto::intent::Intent;
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Weak};
use tap::tap::TapFallible;
use tokio::sync::oneshot;
use ttl_cache::TtlCache;
use typed_store::Map;

use mys_json_rpc_types::{
    OwnedObjectRef, MysObjectDataOptions, MysTransactionBlockEffectsAPI,
    MysTransactionBlockResponse, MysTransactionBlockResponseOptions,
};
use mys_keys::keystore::AccountKeystore;
use mys_sdk::wallet_context::WalletContext;
use mys_types::object::Owner;
use mys_types::quorum_driver_types::ExecuteTransactionRequestType;
use mys_types::{
    base_types::{ObjectID, MysAddress, TransactionDigest},
    gas_coin::GasCoin,
    transaction::{Transaction, TransactionData, TransactionDataAPI},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    gas_coin::MIST_PER_MYS,
};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use tokio::time::{timeout, Duration};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::write_ahead_log::WriteAheadLog;
use crate::{
    BatchFaucetReceipt, BatchSendStatus, BatchSendStatusType, CoinInfo, Faucet, FaucetConfig,
    FaucetError, FaucetReceipt,
};

pub struct SimpleFaucet {
    wallet: WalletContext,
    active_address: MysAddress,
    producer: Mutex<Sender<ObjectID>>,
    consumer: Mutex<Receiver<ObjectID>>,
    batch_producer: Mutex<Sender<ObjectID>>,
    batch_consumer: Mutex<Receiver<ObjectID>>,
    pub metrics: FaucetMetrics,
    pub wal: Mutex<WriteAheadLog>,
    request_producer: Sender<(Uuid, MysAddress, Vec<u64>)>,
    batch_request_size: u64,
    task_id_cache: Mutex<TtlCache<Uuid, BatchSendStatus>>,
    ttl_expiration: u64,
    coin_amount: u64,
    config: FaucetConfig,
    /// Shuts down the batch transfer task. Used only in testing.
    #[allow(unused)]
    batch_transfer_shutdown: parking_lot::Mutex<Option<oneshot::Sender<()>>>,
}

/// We do not just derive(Debug) because WalletContext and the WriteAheadLog do not implement Debug / are also hard
/// to implement Debug.
impl fmt::Debug for SimpleFaucet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SimpleFaucet")
            .field("faucet_wallet", &self.active_address)
            .field("producer", &self.producer)
            .field("consumer", &self.consumer)
            .field("batch_request_size", &self.batch_request_size)
            .field("ttl_expiration", &self.ttl_expiration)
            .field("coin_amount", &self.coin_amount)
            .finish()
    }
}

enum GasCoinResponse {
    GasCoinWithInsufficientBalance(ObjectID),
    InvalidGasCoin(ObjectID),
    NoGasCoinAvailable,
    UnknownGasCoin(ObjectID),
    ValidGasCoin(ObjectID),
}

// TODO: replace this with dryrun at the SDK level
const DEFAULT_GAS_COMPUTATION_BUCKET: u64 = 10_000_000;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const BATCH_TIMEOUT: Duration = Duration::from_secs(10);

impl SimpleFaucet {
    /// Check if we need to split coins on-demand (when pool is running low)
    async fn should_split_coins(&self, config: &FaucetConfig) -> bool {
        if !config.auto_split_coins {
            return false;
        }

        let available_coins = self.metrics.total_available_coins.get() as usize;
        let needs_split = available_coins < config.min_coin_threshold;
        
        if needs_split {
            info!(
                "Pool running low: {} coins available, threshold is {}. Triggering on-demand split.",
                available_coins, config.min_coin_threshold
            );
        }
        
        needs_split
    }

    /// Find a large coin suitable for splitting from the wallet
    async fn find_splittable_coin(&self, config: &FaucetConfig) -> Result<Option<ObjectID>, FaucetError> {
        let split_amount = if config.split_amount > 0 {
            config.split_amount
        } else {
            // Auto-calculate: enough for multiple faucet requests + buffer
            config.amount * 100 // 100 faucet requests worth
        };

        // Get fresh coin list from wallet to find large coins
        let all_coins = self.wallet
            .gas_objects(self.active_address)
            .await
            .map_err(|e| FaucetError::Wallet(e.to_string()))?;
            
        // Look for a coin large enough to split
        for (_, coin_data) in all_coins {
            if let Ok(gas_coin) = GasCoin::try_from(&coin_data) {
                let coin_value = gas_coin.value();
                
                // Check if this coin is large enough to split and not currently in use
                if coin_value > split_amount * 2 {
                    let coin_id = *gas_coin.id();
                    info!(
                        "Found splittable coin {} with {} mist ({} MYS), will split {} mist",
                        coin_id,
                        coin_value,
                        coin_value / MIST_PER_MYS,
                        split_amount
                    );
                    return Ok(Some(coin_id));
                }
            }
        }
        
        warn!("No suitable large coins found for splitting");
        Ok(None)
    }

    /// Split a coin on-demand to replenish the pool
    async fn split_coin_on_demand(
        &self,
        coin_id: ObjectID,
        config: &FaucetConfig,
        uuid: Uuid,
    ) -> Result<(), FaucetError> {
        let split_amount = if config.split_amount > 0 {
            config.split_amount
        } else {
            config.amount * 100 // 100 faucet requests worth
        };

        info!(?uuid, ?coin_id, "Performing on-demand coin split of {} mist", split_amount);

        // Build and execute split transaction
        let gas_cost = 10_000_000; // Estimate gas cost
        match Self::build_split_coin_transaction(
            &self.wallet,
            coin_id,
            self.active_address,
            &[split_amount],
            gas_cost,
        ).await {
            Ok(tx_data) => {
                match Self::execute_split_transaction(&self.wallet, tx_data).await {
                    Ok(_) => {
                        info!(?uuid, ?coin_id, "Successfully split coin on-demand");
                        
                        // Refresh and add new coins to pool
                        self.refresh_coin_pool(config).await?;
                        Ok(())
                    },
                    Err(e) => {
                        warn!(?uuid, ?coin_id, "Failed to execute on-demand split: {:?}", e);
                        Err(FaucetError::internal(e))
                    }
                }
            },
            Err(e) => {
                warn!(?uuid, ?coin_id, "Failed to build on-demand split transaction: {:?}", e);
                Err(FaucetError::internal(e))
            }
        }
    }

    /// Refresh the coin pool by finding new coins and adding them
    async fn refresh_coin_pool(&self, config: &FaucetConfig) -> Result<(), FaucetError> {
        let fresh_coins = self.wallet
            .gas_objects(self.active_address)
            .await
            .map_err(|e| FaucetError::Wallet(e.to_string()))?;

        let producer = self.producer.lock().await;
        let mut added_count = 0;

        for (_, coin_data) in fresh_coins {
            if let Ok(gas_coin) = GasCoin::try_from(&coin_data) {
                let coin_id = *gas_coin.id();
                let coin_value = gas_coin.value();
                
                // Only add coins that meet minimum requirements for faucet usage
                if coin_value >= (config.amount * config.num_coins as u64) {
                    if producer.try_send(coin_id).is_ok() {
                        info!(?coin_id, "Added fresh coin to pool with {} mist", coin_value);
                        self.metrics.total_available_coins.inc();
                        added_count += 1;
                    }
                }
            }
        }

        info!("Refreshed coin pool: added {} new coins", added_count);
        Ok(())
    }

    /// Build a transaction to split a coin into smaller amounts
    async fn build_split_coin_transaction(
        wallet: &WalletContext,
        coin_id: ObjectID,
        signer: MysAddress,
        split_amounts: &[u64],
        gas_budget: u64,
    ) -> Result<TransactionData, anyhow::Error> {
        let client = wallet.get_client().await?;
        let gas_price = client.read_api().get_reference_gas_price().await?;
        let coin_ref = wallet.get_object_ref(coin_id).await?;
        
        let mut builder = ProgrammableTransactionBuilder::new();
        
        // Split the coin into multiple smaller coins
        let mut amounts_args = Vec::new();
        for &amount in split_amounts {
            amounts_args.push(builder.pure(amount)?);
        }
            
        builder.command(mys_types::transaction::Command::SplitCoins(
            mys_types::transaction::Argument::GasCoin, 
            amounts_args
        ));
        
        let pt = builder.finish();
        
        Ok(TransactionData::new_programmable(
            signer,
            vec![coin_ref],
            pt,
            gas_budget,
            gas_price,
        ))
    }

    /// Execute a split transaction
    async fn execute_split_transaction(
        wallet: &WalletContext,
        tx_data: TransactionData,
    ) -> Result<(), anyhow::Error> {
        let signature = wallet
            .config
            .keystore
            .sign_secure(&tx_data.sender(), &tx_data, Intent::mys_transaction())?;
        let tx = Transaction::from_data(tx_data, vec![signature]);
        
        let client = wallet.get_client().await?;
        let _response = client
            .quorum_driver_api()
            .execute_transaction_block(
                tx,
                MysTransactionBlockResponseOptions::new().with_effects(),
                Some(mys_types::quorum_driver_types::ExecuteTransactionRequestType::WaitForLocalExecution),
            )
            .await?;
            
        Ok(())
    }

    pub async fn new(
        mut wallet: WalletContext,
        prometheus_registry: &Registry,
        wal_path: &Path,
        config: FaucetConfig,
    ) -> Result<Arc<Self>, FaucetError> {
        let active_address = wallet
            .active_address()
            .map_err(|err| FaucetError::Wallet(err.to_string()))?;
        info!("SimpleFaucet::new with active address: {active_address}");

        let initial_coins = wallet
            .gas_objects(active_address)
            .await
            .map_err(|e| FaucetError::Wallet(e.to_string()))?
            .iter()
            // Ok to unwrap() since `get_gas_objects` guarantees gas
            .map(|q| GasCoin::try_from(&q.1).unwrap())
            .filter(|coin| coin.0.balance.value() >= (config.amount * config.num_coins as u64))
            .collect::<Vec<GasCoin>>();
        
        info!("Found {} initial coins meeting minimum requirements", initial_coins.len());
        
        // Use coins as-is for on-demand splitting approach
        let coins = initial_coins;
        let metrics = FaucetMetrics::new(prometheus_registry);
        // set initial balance when faucet starts
        let balance = coins.iter().map(|coin| coin.0.balance.value()).sum::<u64>();
        metrics.balance.set(balance as i64);

        let wal = WriteAheadLog::open(wal_path);
        let mut pending = vec![];

        let (producer, consumer) = mpsc::channel(coins.len());
        let (batch_producer, batch_consumer) = mpsc::channel(coins.len());

        let (sender, mut receiver) =
            mpsc::channel::<(Uuid, MysAddress, Vec<u64>)>(config.max_request_queue_length as usize);

        // This is to handle the case where there is only 1 coin, we want it to go to the normal queue
        let split_point = if coins.len() > 10 {
            coins.len() / 2
        } else {
            coins.len()
        };
        // Put half of the coins in the old faucet impl queue, and put half in the other queue for batch coins.
        // In the test cases we create an account with 5 coins so we just let this run with a minimum of 5 coins
        for (coins_processed, coin) in coins.iter().enumerate() {
            let coin_id = *coin.id();
            if let Some(write_ahead_log::Entry {
                uuid,
                recipient,
                tx,
                retry_count: _,
                in_flight: _,
            }) = wal.reclaim(coin_id).map_err(FaucetError::internal)?
            {
                let uuid = Uuid::from_bytes(uuid);
                info!(?uuid, ?recipient, ?coin_id, "Retrying txn from WAL.");
                pending.push((uuid, recipient, coin_id, tx));
            } else if coins_processed < split_point {
                producer
                    .send(coin_id)
                    .await
                    .tap_ok(|_| {
                        info!(?coin_id, "Adding coin to gas pool");
                        metrics.total_available_coins.inc();
                    })
                    .tap_err(|e| error!(?coin_id, "Failed to add coin to gas pools: {e:?}"))
                    .unwrap();
            } else {
                batch_producer
                    .send(coin_id)
                    .await
                    .tap_ok(|_| {
                        info!(?coin_id, "Adding coin to batch gas pool");
                        metrics.total_available_coins.inc();
                    })
                    .tap_err(|e| error!(?coin_id, "Failed to add coin to batch gas pools: {e:?}"))
                    .unwrap();
            }
        }
        let (batch_transfer_shutdown, mut rx_batch_transfer_shutdown) = oneshot::channel();

        let faucet = Self {
            wallet,
            active_address,
            producer: Mutex::new(producer),
            consumer: Mutex::new(consumer),
            batch_producer: Mutex::new(batch_producer),
            batch_consumer: Mutex::new(batch_consumer),
            metrics,
            wal: Mutex::new(wal),
            request_producer: sender,
            batch_request_size: config.batch_request_size,
            // Max faucet requests times 10 minutes worth of requests to hold onto at max.
            // Note that the cache holds onto a Uuid for [ttl_expiration] in from every update in status with both INPROGRESS and SUCCEEDED
            task_id_cache: TtlCache::new(config.max_request_per_second as usize * 60 * 10).into(),
            ttl_expiration: config.ttl_expiration,
            coin_amount: config.amount,
            config,
            batch_transfer_shutdown: parking_lot::Mutex::new(Some(batch_transfer_shutdown)),
        };

        let arc_faucet = Arc::new(faucet);
        let batch_clone = Arc::downgrade(&arc_faucet);
        spawn_monitored_task!(async move {
            info!("Starting task to handle batch faucet requests.");
            loop {
                match batch_transfer_gases(
                    &batch_clone,
                    &mut receiver,
                    &mut rx_batch_transfer_shutdown,
                )
                .await
                {
                    Ok(response) => {
                        if response == TransactionDigest::ZERO {
                            info!("Batch transfer incomplete due to faucet shutting down.");
                        } else {
                            info!(
                                "Batch transfer completed with transaction digest: {:?}",
                                response
                            );
                        }
                    }
                    Err(err) => {
                        error!("{:?}", err);
                    }
                }
            }
        });
        // Retrying all the pending transactions from the WAL, before continuing.  Ignore return
        // values -- if the executions failed, the pending coins will simply remain in the WAL, and
        // not recycled.
        futures::future::join_all(pending.into_iter().map(|(uuid, recipient, coin_id, tx)| {
            arc_faucet.sign_and_execute_txn(uuid, recipient, coin_id, tx, false)
        }))
        .await;

        Ok(arc_faucet)
    }

    /// Take the consumer lock and pull a Coin ID from the queue, without checking whether it is
    /// valid or not.
    async fn pop_gas_coin(&self, uuid: Uuid) -> Option<ObjectID> {
        // If the gas candidate queue is exhausted, the request will be suspended indefinitely until
        // a producer puts in more candidate gas objects. At the same time, other requests will be
        // blocked by the lock acquisition as well.
        let Ok(mut consumer) = tokio::time::timeout(LOCK_TIMEOUT, self.consumer.lock()).await
        else {
            error!(?uuid, "Timeout when getting consumer lock");
            return None;
        };

        info!(?uuid, "Got consumer lock, pulling coins.");
        let Ok(coin) = tokio::time::timeout(RECV_TIMEOUT, consumer.recv()).await else {
            error!(?uuid, "Timeout when getting gas coin from the queue");
            return None;
        };

        let Some(coin) = coin else {
            unreachable!("channel is closed");
        };

        self.metrics.total_available_coins.dec();
        Some(coin)
    }

    /// Take the consumer lock and pull a Coin ID from the queue, without checking whether it is
    /// valid or not.
    async fn pop_gas_coin_for_batch(&self, uuid: Uuid) -> Option<ObjectID> {
        // If the gas candidate queue is exhausted, the request will be suspended indefinitely until
        // a producer puts in more candidate gas objects. At the same time, other requests will be
        // blocked by the lock acquisition as well.
        let Ok(mut batch_consumer) =
            tokio::time::timeout(LOCK_TIMEOUT, self.batch_consumer.lock()).await
        else {
            error!(?uuid, "Timeout when getting batch consumer lock");
            return None;
        };

        info!(?uuid, "Got consumer lock, pulling coins.");
        let Ok(coin) = tokio::time::timeout(RECV_TIMEOUT, batch_consumer.recv()).await else {
            error!(?uuid, "Timeout when getting gas coin from the queue");
            return None;
        };

        let Some(coin) = coin else {
            unreachable!("channel is closed");
        };

        self.metrics.total_available_coins.dec();
        Some(coin)
    }

    /// Pulls a coin from the queue and makes sure it is fit for use (belongs to the faucet, has
    /// sufficient balance). If no coins are available, attempts cross-pool fallback and on-demand splitting.
    async fn prepare_gas_coin(
        &self,
        total_amount: u64,
        uuid: Uuid,
        for_batch: bool,
        config: &FaucetConfig,
    ) -> GasCoinResponse {
        let coin_id = if for_batch {
            self.pop_gas_coin_for_batch(uuid).await
        } else {
            self.pop_gas_coin(uuid).await
        };

        let coin_id = if let Some(id) = coin_id {
            id
        } else {
            info!(?uuid, "No coins available in primary pool, trying fallback strategies");
            
            // Strategy 1: Try the other pool if we have coins there
            let fallback_coin = if for_batch {
                info!(?uuid, "Trying regular pool as fallback for batch request");
                self.pop_gas_coin(uuid).await
            } else {
                info!(?uuid, "Trying batch pool as fallback for regular request");
                self.pop_gas_coin_for_batch(uuid).await
            };
            
            if let Some(id) = fallback_coin {
                info!(?uuid, "Successfully got coin from fallback pool");
                id
            } else {
                // Strategy 2: Try on-demand splitting if enabled and needed
                if self.should_split_coins(config).await {
                    if let Ok(Some(splittable_coin)) = self.find_splittable_coin(config).await {
                        info!(?uuid, "No coins in either pool, attempting on-demand split");
                        if self.split_coin_on_demand(splittable_coin, config, uuid).await.is_ok() {
                            // Try again after splitting - prefer original pool first, then fallback
                            if let Some(new_coin_id) = if for_batch {
                                self.pop_gas_coin_for_batch(uuid).await
                            } else {
                                self.pop_gas_coin(uuid).await
                            } {
                                info!(?uuid, "Successfully obtained coin from primary pool after split");
                                new_coin_id
                            } else if let Some(fallback_coin_id) = if for_batch {
                                self.pop_gas_coin(uuid).await
                            } else {
                                self.pop_gas_coin_for_batch(uuid).await
                            } {
                                info!(?uuid, "Successfully obtained coin from fallback pool after split");
                                fallback_coin_id
                            } else {
                                warn!(?uuid, "Still no coins available after splitting");
                                return GasCoinResponse::NoGasCoinAvailable;
                            }
                        } else {
                            warn!(?uuid, "On-demand splitting failed");
                            return GasCoinResponse::NoGasCoinAvailable;
                        }
                    } else {
                        warn!(?uuid, "No splittable coins found for on-demand split");
                        return GasCoinResponse::NoGasCoinAvailable;
                    }
                } else {
                    warn!(?uuid, "No coins available in either pool and splitting not enabled/needed");
                    return GasCoinResponse::NoGasCoinAvailable;
                }
            }
        };

        match self.get_gas_coin_and_check_faucet_owner(coin_id).await {
            Ok(Some(gas_coin)) if gas_coin.value() >= total_amount => {
                info!(?uuid, ?coin_id, "balance: {}", gas_coin.value());
                GasCoinResponse::ValidGasCoin(coin_id)
            }

            Ok(Some(_)) => {
                info!(?uuid, ?coin_id, "insufficient balance",);
                GasCoinResponse::GasCoinWithInsufficientBalance(coin_id)
            }

            Ok(None) => {
                info!(?uuid, ?coin_id, "No gas coin returned.",);
                GasCoinResponse::InvalidGasCoin(coin_id)
            }

            Err(e) => {
                error!(?uuid, ?coin_id, "Fullnode read error: {e:?}");
                GasCoinResponse::UnknownGasCoin(coin_id)
            }
        }
    }

    /// Check if the gas coin is still valid. A valid gas coin
    /// 1. Exists presently
    /// 2. is a gas coin
    ///
    /// If the coin is valid, return Ok(Some(GasCoin))
    /// If the coin invalid, return Ok(None)
    /// If the fullnode returns an unexpected error, returns Err(e)
    async fn get_coin(
        &self,
        coin_id: ObjectID,
    ) -> anyhow::Result<Option<(Option<Owner>, GasCoin)>> {
        let client = self.wallet.get_client().await?;
        let gas_obj = client
            .read_api()
            .get_object_with_options(
                coin_id,
                MysObjectDataOptions::new()
                    .with_type()
                    .with_owner()
                    .with_content(),
            )
            .await?;
        let o = gas_obj.data;
        if let Some(o) = o {
            Ok(GasCoin::try_from(&o).ok().map(|coin| (o.owner, coin)))
        } else {
            Ok(None)
        }
    }

    /// Similar to get_coin but checks that the owner is the active
    /// faucet address. If the coin exists, but does not have the correct owner,
    /// returns None
    async fn get_gas_coin_and_check_faucet_owner(
        &self,
        coin_id: ObjectID,
    ) -> anyhow::Result<Option<GasCoin>> {
        let gas_obj = self.get_coin(coin_id).await?;
        info!(?coin_id, "Reading gas coin object: {:?}", gas_obj);
        Ok(gas_obj.and_then(|(owner_opt, coin)| match owner_opt {
            Some(Owner::AddressOwner(owner_addr)) if owner_addr == self.active_address => {
                Some(coin)
            }
            _ => None,
        }))
    }

    /// Clear the WAL list in the faucet
    pub async fn retry_wal_coins(&self) -> Result<(), FaucetError> {
        let mut wal = self.wal.lock().await;
        let mut pending = vec![];

        for item in wal.log.safe_iter() {
            // Safe unwrap as we are the only ones that ever add to the WAL.
            let (coin_id, entry) = item.unwrap();
            let uuid = Uuid::from_bytes(entry.uuid);
            if !entry.in_flight {
                pending.push((uuid, entry.recipient, coin_id, entry.tx));
            }
        }

        for (_, _, coin_id, _) in &pending {
            wal.increment_retry_count(*coin_id)
                .map_err(FaucetError::internal)?;
            wal.set_in_flight(*coin_id, true)
                .map_err(FaucetError::internal)?;
        }

        info!("Retrying WAL of length: {:?}", pending.len());
        // Drops the lock early because sign_and_execute_txn requires the lock.
        drop(wal);

        futures::future::join_all(pending.into_iter().map(|(uuid, recipient, coin_id, tx)| {
            self.sign_and_execute_txn(uuid, recipient, coin_id, tx, false)
        }))
        .await;

        Ok(())
    }

    /// Sign an already created transaction (in `tx_data`) and keep trying to execute it until
    /// fullnode returns a definite response or a timeout is hit.
    async fn sign_and_execute_txn(
        &self,
        uuid: Uuid,
        recipient: MysAddress,
        coin_id: ObjectID,
        tx_data: TransactionData,
        for_batch: bool,
    ) -> Result<MysTransactionBlockResponse, FaucetError> {
        let signature = self
            .wallet
            .config
            .keystore
            .sign_secure(&self.active_address, &tx_data, Intent::mys_transaction())
            .map_err(FaucetError::internal)?;
        let tx = Transaction::from_data(tx_data, vec![signature]);
        let tx_digest = *tx.digest();
        info!(
            ?tx_digest,
            ?recipient,
            ?coin_id,
            ?uuid,
            "PayMys transaction in faucet."
        );

        match timeout(
            Duration::from_secs(300),
            self.execute_pay_mys_txn_with_retries(&tx, coin_id, recipient, uuid),
        )
        .await
        {
            Err(elapsed) => {
                warn!(
                    ?recipient,
                    ?coin_id,
                    ?uuid,
                    "Failed to execute PayMys transactions in faucet after {elapsed}. Coin will \
                     not be reused."
                );

                // We set the inflight status to false so that the async thread that
                // retries this transactions will attempt to try again.
                // We should only set this inflight if we see that it's not a client error
                if let Err(err) = self.wal.lock().await.set_in_flight(coin_id, false) {
                    error!(
                        ?recipient,
                        ?coin_id,
                        ?uuid,
                        "Failed to set coin in flight status in WAL: {:?}",
                        err
                    );
                }

                Err(FaucetError::Transfer(
                    "could not complete transfer within timeout".into(),
                ))
            }

            Ok(result) => {
                // Note: we do not recycle gas unless the transaction was successful - the faucet
                // may run out of available coins due to errors, but this allows a human to
                // intervene and attempt to fix things. If we re-use coins that had errors, we may
                // lock them permanently.

                // It's important to remove the coin from the WAL before recycling it, to avoid a
                // race with the next request served with this coin.  If this operation fails, log
                // it and continue so we don't lose access to the coin -- the worst that can happen
                // is that the WAL contains a stale entry.
                if self.wal.lock().await.commit(coin_id).is_err() {
                    error!(?coin_id, "Failed to remove coin from WAL");
                }
                if for_batch {
                    self.recycle_gas_coin_for_batch(coin_id, uuid).await;
                } else {
                    self.recycle_gas_coin(coin_id, uuid).await;
                }

                if let Some(ref balances) = result.balance_changes {
                    let mys_used = balances
                        .iter()
                        .find(|balance| {
                            balance
                                .owner
                                .get_address_owner_address()
                                .is_ok_and(|address| address == self.active_address)
                        })
                        .map(|b| b.amount)
                        .unwrap_or_else(|| 0);
                    info!("MYS used in this tx {}: {}", tx_digest, mys_used);
                    self.metrics.balance.add(mys_used as i64);
                }

                Ok(result)
            }
        }
    }

    #[async_recursion]
    async fn transfer_gases(
        &self,
        amounts: &[u64],
        recipient: MysAddress,
        uuid: Uuid,
    ) -> Result<(TransactionDigest, Vec<ObjectID>), FaucetError> {
        let number_of_coins = amounts.len();
        let total_amount: u64 = amounts.iter().sum();
        let gas_cost = self.get_gas_cost().await?;

        let gas_coin_response = self
            .prepare_gas_coin(total_amount + gas_cost, uuid, false, &self.config)
            .await;
        match gas_coin_response {
            GasCoinResponse::ValidGasCoin(coin_id) => {
                let tx_data = self
                    .build_pay_mys_txn(coin_id, self.active_address, recipient, amounts, gas_cost)
                    .await
                    .map_err(FaucetError::internal)?;

                {
                    // Register the intention to send this transaction before we send it, so that if
                    // faucet fails or we give up before we get a definite response, we have a
                    // chance to retry later.
                    let mut wal = self.wal.lock().await;
                    wal.reserve(uuid, coin_id, recipient, tx_data.clone())
                        .map_err(FaucetError::internal)?;
                }
                let response = self
                    .sign_and_execute_txn(uuid, recipient, coin_id, tx_data, false)
                    .await?;
                self.metrics.total_coin_requests_succeeded.inc();
                self.check_and_map_transfer_gas_result(response, number_of_coins, recipient)
                    .await
            }

            GasCoinResponse::UnknownGasCoin(coin_id) => {
                self.recycle_gas_coin(coin_id, uuid).await;
                Err(FaucetError::FullnodeReadingError(format!(
                    "unknown gas coin {coin_id:?}"
                )))
            }

            GasCoinResponse::GasCoinWithInsufficientBalance(coin_id) => {
                warn!(?uuid, ?coin_id, "Insufficient balance, removing from pool");
                self.metrics.total_discarded_coins.inc();
                self.transfer_gases(amounts, recipient, uuid).await
            }

            GasCoinResponse::InvalidGasCoin(coin_id) => {
                // The coin does not exist, or does not belong to the current active address.
                warn!(?uuid, ?coin_id, "Invalid, removing from pool");
                self.metrics.total_discarded_coins.inc();
                self.transfer_gases(amounts, recipient, uuid).await
            }

            GasCoinResponse::NoGasCoinAvailable => Err(FaucetError::NoGasCoinAvailable),
        }
    }

    async fn recycle_gas_coin(&self, coin_id: ObjectID, uuid: Uuid) {
        // Once transactions are done, in despite of success or failure,
        // we put back the coins. The producer should never wait indefinitely,
        // in that the channel is initialized with big enough capacity.
        let producer = self.producer.lock().await;
        info!(?uuid, ?coin_id, "Got producer lock and recycling coin");
        producer
            .try_send(coin_id)
            .expect("unexpected - queue is large enough to hold all coins");
        self.metrics.total_available_coins.inc();
        info!(?uuid, ?coin_id, "Recycled coin");
    }

    async fn recycle_gas_coin_for_batch(&self, coin_id: ObjectID, uuid: Uuid) {
        // Once transactions are done, in despite of success or failure,
        // we put back the coins. The producer should never wait indefinitely,
        // in that the channel is initialized with big enough capacity.
        let batch_producer = self.batch_producer.lock().await;
        info!(?uuid, ?coin_id, "Got producer lock and recycling coin");
        batch_producer
            .try_send(coin_id)
            .expect("unexpected - queue is large enough to hold all coins");
        self.metrics.total_available_coins.inc();
        info!(?uuid, ?coin_id, "Recycled coin");
    }

    async fn execute_pay_mys_txn_with_retries(
        &self,
        tx: &Transaction,
        coin_id: ObjectID,
        recipient: MysAddress,
        uuid: Uuid,
    ) -> MysTransactionBlockResponse {
        let mut retry_delay = Duration::from_millis(500);

        loop {
            let res = self.execute_pay_mys_txn(tx, coin_id, recipient, uuid).await;

            if let Ok(res) = res {
                return res;
            }

            info!(
                ?recipient,
                ?coin_id,
                ?uuid,
                ?retry_delay,
                "PayMys transaction in faucet failed, previous error: {:?}",
                &res,
            );

            tokio::time::sleep(retry_delay).await;
            retry_delay *= 2;
        }
    }

    async fn execute_pay_mys_txn(
        &self,
        tx: &Transaction,
        coin_id: ObjectID,
        recipient: MysAddress,
        uuid: Uuid,
    ) -> Result<MysTransactionBlockResponse, anyhow::Error> {
        self.metrics.current_executions_in_flight.inc();
        let _metrics_guard = scopeguard::guard(self.metrics.clone(), |metrics| {
            metrics.current_executions_in_flight.dec();
        });

        let tx_digest = tx.digest();
        let client = self.wallet.get_client().await?;

        Ok(client
            .quorum_driver_api()
            .execute_transaction_block(
                tx.clone(),
                MysTransactionBlockResponseOptions::new()
                    .with_effects()
                    .with_balance_changes(),
                Some(ExecuteTransactionRequestType::WaitForLocalExecution),
            )
            .await
            .tap_err(|e| {
                error!(
                    ?tx_digest,
                    ?recipient,
                    ?coin_id,
                    ?uuid,
                    "Transfer Transaction failed: {:?}",
                    e
                )
            })?)
    }

    async fn get_gas_cost(&self) -> Result<u64, FaucetError> {
        let gas_price = self.get_gas_price().await?;
        Ok(gas_price * DEFAULT_GAS_COMPUTATION_BUCKET)
    }

    async fn get_gas_price(&self) -> Result<u64, FaucetError> {
        let client = self
            .wallet
            .get_client()
            .await
            .map_err(|e| FaucetError::Wallet(format!("Unable to get client: {e:?}")))?;
        client
            .read_api()
            .get_reference_gas_price()
            .await
            .map_err(|e| FaucetError::FullnodeReadingError(format!("Error fetch gas price {e:?}")))
    }

    async fn build_pay_mys_txn(
        &self,
        coin_id: ObjectID,
        signer: MysAddress,
        recipient: MysAddress,
        amounts: &[u64],
        budget: u64,
    ) -> Result<TransactionData, anyhow::Error> {
        let recipients = vec![recipient; amounts.len()];
        let client = self.wallet.get_client().await?;
        client
            .transaction_builder()
            .pay_mys(signer, vec![coin_id], recipients, amounts.to_vec(), budget)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to build PayMys transaction for coin {:?}, with err {:?}",
                    coin_id,
                    e
                )
            })
    }

    async fn check_and_map_transfer_gas_result(
        &self,
        res: MysTransactionBlockResponse,
        number_of_coins: usize,
        recipient: MysAddress,
    ) -> Result<(TransactionDigest, Vec<ObjectID>), FaucetError> {
        let created = res
            .effects
            .ok_or_else(|| {
                FaucetError::ParseTransactionResponseError(format!(
                    "effects field missing for txn {}",
                    res.digest
                ))
            })?
            .created()
            .to_vec();
        if created.len() != number_of_coins {
            return Err(FaucetError::CoinAmountTransferredIncorrect(format!(
                "PayMys Transaction should create exact {:?} new coins, but got {:?}",
                number_of_coins, created
            )));
        }
        assert!(created.iter().all(|created_coin_owner_ref| {
            created_coin_owner_ref
                .owner
                .get_address_owner_address()
                .is_ok_and(|address| address == recipient)
        }));
        let coin_ids: Vec<ObjectID> = created
            .iter()
            .map(|created_coin_owner_ref| created_coin_owner_ref.reference.object_id)
            .collect();
        Ok((res.digest, coin_ids))
    }

    async fn build_batch_pay_mys_txn(
        &self,
        coin_id: ObjectID,
        batch_requests: Vec<(Uuid, MysAddress, Vec<u64>)>,
        signer: MysAddress,
        budget: u64,
    ) -> Result<TransactionData, anyhow::Error> {
        let gas_payment = self.wallet.get_object_ref(coin_id).await?;
        let gas_price = self.wallet.get_reference_gas_price().await?;
        // TODO (Jian): change to make this more efficient by changing impl to one Splitcoin, and many TransferObjects
        let pt = {
            let mut builder = ProgrammableTransactionBuilder::new();
            for (_uuid, recipient, amounts) in batch_requests {
                let recipients = vec![recipient; amounts.len()];
                builder.pay_mys(recipients, amounts)?;
            }
            builder.finish()
        };

        Ok(TransactionData::new_programmable(
            signer,
            vec![gas_payment],
            pt,
            budget,
            gas_price,
        ))
    }

    async fn check_and_map_batch_transfer_gas_result(
        &self,
        res: MysTransactionBlockResponse,
        requests: Vec<(Uuid, MysAddress, Vec<u64>)>,
    ) -> Result<(), FaucetError> {
        // Grab the list of created coins and turn it into a map of destination MysAddress to Vec<Coins>
        let created = res
            .effects
            .ok_or_else(|| {
                FaucetError::ParseTransactionResponseError(format!(
                    "effects field missing for txn {}",
                    res.digest
                ))
            })?
            .created()
            .to_vec();

        let mut address_coins_map: HashMap<MysAddress, Vec<OwnedObjectRef>> = HashMap::new();
        created.iter().for_each(|created_coin_owner_ref| {
            let owner = created_coin_owner_ref.owner.clone();
            let coin_obj_ref = created_coin_owner_ref.clone();

            // Insert the coins into the map based on the destination address
            address_coins_map
                .entry(owner.get_owner_address().unwrap())
                .or_default()
                .push(coin_obj_ref);
        });

        // Assert that the number of times a mys_address occurs is the number of times the coins
        // come up in the vector.
        let mut request_count: HashMap<MysAddress, u64> = HashMap::new();
        // Acquire lock and update all of the request Uuids
        let mut task_map = self.task_id_cache.lock().await;
        for (uuid, addy, amounts) in requests {
            let number_of_coins = amounts.len();
            // Get or insert mys_address into request count
            let index = *request_count.entry(addy).or_insert(0);

            // The address coin map should contain the coins transferred in the given request.
            let coins_created_for_address = address_coins_map.entry(addy).or_default();

            if number_of_coins as u64 + index > coins_created_for_address.len() as u64 {
                return Err(FaucetError::CoinAmountTransferredIncorrect(format!(
                    "PayMys Transaction should create exact {:?} new coins, but got {:?}",
                    number_of_coins as u64 + index,
                    coins_created_for_address.len()
                )));
            }
            let coins_slice =
                &mut coins_created_for_address[index as usize..(index as usize + number_of_coins)];

            request_count.insert(addy, number_of_coins as u64 + index);

            let transferred_gases = coins_slice
                .iter()
                .map(|coin| CoinInfo {
                    id: coin.object_id(),
                    transfer_tx_digest: res.digest,
                    amount: self.coin_amount,
                })
                .collect();

            task_map.insert(
                uuid,
                BatchSendStatus {
                    status: BatchSendStatusType::SUCCEEDED,
                    transferred_gas_objects: Some(FaucetReceipt {
                        sent: transferred_gases,
                    }),
                },
                Duration::from_secs(self.ttl_expiration),
            );
        }

        // We use a separate map to figure out which index should correlate to the
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn shutdown_batch_send_task(&self) {
        self.batch_transfer_shutdown
            .lock()
            .take()
            .unwrap()
            .send(())
            .unwrap();
    }

    #[cfg(test)]
    pub fn wallet_mut(&mut self) -> &mut WalletContext {
        &mut self.wallet
    }

    #[cfg(test)]
    pub fn teardown(self) -> WalletContext {
        self.wallet
    }

    #[cfg(test)]
    async fn drain_gas_queue(&mut self, expected_gas_count: usize) -> HashSet<ObjectID> {
        use tokio::sync::mpsc::error::TryRecvError;
        let mut consumer = self.consumer.lock().await;
        let mut candidates = HashSet::new();
        let mut i = 0;
        loop {
            let coin_id = consumer
                .try_recv()
                .unwrap_or_else(|e| panic!("Expect the {}th candidate but got {}", i, e));
            candidates.insert(coin_id);
            i += 1;
            if i == expected_gas_count {
                assert_eq!(consumer.try_recv().unwrap_err(), TryRecvError::Empty);
                break;
            }
        }
        candidates
    }
}

#[async_trait]
impl Faucet for SimpleFaucet {
    async fn send(
        &self,
        id: Uuid,
        recipient: MysAddress,
        amounts: &[u64],
    ) -> Result<FaucetReceipt, FaucetError> {
        info!(?recipient, uuid = ?id, ?amounts, "Getting faucet requests");

        let (digest, coin_ids) = self.transfer_gases(amounts, recipient, id).await?;

        info!(uuid = ?id, ?recipient, ?digest, "PayMys txn succeeded");
        let mut sent = Vec::with_capacity(coin_ids.len());
        let coin_results =
            futures::future::join_all(coin_ids.iter().map(|coin_id| self.get_coin(*coin_id))).await;
        for (coin_id, res) in coin_ids.into_iter().zip(coin_results) {
            let amount = if let Ok(Some((_, coin))) = res {
                coin.value()
            } else {
                info!(
                    ?recipient,
                    ?coin_id,
                    uuid = ?id,
                    "Could not find coin after successful transaction, error: {:?}",
                    &res,
                );
                0
            };
            sent.push(CoinInfo {
                transfer_tx_digest: digest,
                amount,
                id: coin_id,
            });
        }

        // Store into status map that the txn was successful for backwards compatibility
        let faucet_receipt = FaucetReceipt { sent };
        let mut task_map = self.task_id_cache.lock().await;
        task_map.insert(
            id,
            BatchSendStatus {
                status: BatchSendStatusType::SUCCEEDED,
                transferred_gas_objects: Some(faucet_receipt.clone()),
            },
            Duration::from_secs(self.ttl_expiration),
        );

        Ok(faucet_receipt)
    }

    async fn batch_send(
        &self,
        id: Uuid,
        recipient: MysAddress,
        amounts: &[u64],
    ) -> Result<BatchFaucetReceipt, FaucetError> {
        info!(?recipient, uuid = ?id, "Getting faucet request");
        if self
            .request_producer
            .try_send((id, recipient, amounts.to_vec()))
            .is_err()
        {
            return Err(FaucetError::BatchSendQueueFull);
        }
        let mut task_map = self.task_id_cache.lock().await;
        task_map.insert(
            id,
            BatchSendStatus {
                status: BatchSendStatusType::INPROGRESS,
                transferred_gas_objects: None,
            },
            Duration::from_secs(self.ttl_expiration),
        );
        Ok(BatchFaucetReceipt {
            task: id.to_string(),
        })
    }

    async fn get_batch_send_status(&self, task_id: Uuid) -> Result<BatchSendStatus, FaucetError> {
        let task_map = self.task_id_cache.lock().await;
        match task_map.get(&task_id) {
            Some(status) => Ok(status.clone()),
            None => Err(FaucetError::Internal("task id not found".to_string())),
        }
    }
}

pub async fn batch_gather(
    request_consumer: &mut Receiver<(Uuid, MysAddress, Vec<u64>)>,
    requests: &mut Vec<(Uuid, MysAddress, Vec<u64>)>,
    batch_request_size: u64,
) -> Result<(), FaucetError> {
    // Gather the rest of the batch after the first item has been taken.
    for _ in 1..batch_request_size {
        let Some(req) = request_consumer.recv().await else {
            error!("Request consumer queue closed");
            return Err(FaucetError::ChannelClosed);
        };

        requests.push(req);
    }

    Ok(())
}

// Function to process the batch send of the mcsp queue
pub async fn batch_transfer_gases(
    weak_faucet: &Weak<SimpleFaucet>,
    request_consumer: &mut Receiver<(Uuid, MysAddress, Vec<u64>)>,
    rx_batch_transfer_shutdown: &mut oneshot::Receiver<()>,
) -> Result<TransactionDigest, FaucetError> {
    let mut requests = Vec::new();

    tokio::select! {
        first_req = request_consumer.recv() => {
            if let Some((uuid, address, amounts)) = first_req {
                requests.push((uuid, address, amounts));
            } else {
                // Should only happen after the Faucet has shut down
                info!("No more faucet requests will be received. Exiting batch faucet task ...");
                return Ok(TransactionDigest::ZERO);
            };
        }
        _ = rx_batch_transfer_shutdown => {
            info!("Shutdown signal received. Exiting faucet ...");
            return Ok(TransactionDigest::ZERO);
        }
    };

    let Some(faucet) = weak_faucet.upgrade() else {
        info!("Faucet has shut down already. Exiting ...");
        return Ok(TransactionDigest::ZERO);
    };

    if timeout(
        BATCH_TIMEOUT,
        batch_gather(request_consumer, &mut requests, faucet.batch_request_size),
    )
    .await
    .is_err()
    {
        info!("Batch timeout elapsed while waiting.");
    };

    let total_requests = requests.len();
    let gas_cost = faucet.get_gas_cost().await?;
    // The UUID here is for the batched request
    let uuid = Uuid::new_v4();
    info!(
        ?uuid,
        "Batch transfer attempted of size: {:?}", total_requests
    );
    let total_mys_needed: u64 = requests.iter().flat_map(|(_, _, amounts)| amounts).sum();
    // This loop is utilized to grab a coin that is large enough for the request
    loop {
        let gas_coin_response = faucet
            .prepare_gas_coin(total_mys_needed + gas_cost, uuid, true, &faucet.config)
            .await;

        match gas_coin_response {
            GasCoinResponse::ValidGasCoin(coin_id) => {
                let tx_data = faucet
                    .build_batch_pay_mys_txn(
                        coin_id,
                        requests.clone(),
                        faucet.active_address,
                        gas_cost,
                    )
                    .await
                    .map_err(FaucetError::internal)?;

                // Because we are batching transactions to faucet, we will just not use a real recipient for
                // mys address, and instead just fill it with the ZERO address.
                let recipient = MysAddress::ZERO;
                {
                    // Register the intention to send this transaction before we send it, so that if
                    // faucet fails or we give up before we get a definite response, we have a
                    // chance to retry later.
                    let mut wal = faucet.wal.lock().await;
                    wal.reserve(uuid, coin_id, recipient, tx_data.clone())
                        .map_err(FaucetError::internal)?;
                }
                let response = faucet
                    .sign_and_execute_txn(uuid, recipient, coin_id, tx_data, true)
                    .await?;

                faucet
                    .metrics
                    .total_coin_requests_succeeded
                    .add(total_requests as i64);

                faucet
                    .check_and_map_batch_transfer_gas_result(response.clone(), requests)
                    .await?;

                return Ok(response.digest);
            }

            GasCoinResponse::UnknownGasCoin(coin_id) => {
                // Continue the loop to retry preparing the gas coin
                warn!(?uuid, ?coin_id, "unknown gas coin.");
                faucet.metrics.total_discarded_coins.inc();
                continue;
            }

            GasCoinResponse::GasCoinWithInsufficientBalance(coin_id) => {
                warn!(?uuid, ?coin_id, "Insufficient balance, removing from pool");
                faucet.metrics.total_discarded_coins.inc();
                // Continue the loop to retry preparing the gas coin
                continue;
            }

            GasCoinResponse::InvalidGasCoin(coin_id) => {
                // The coin does not exist, or does not belong to the current active address.
                warn!(?uuid, ?coin_id, "Invalid, removing from pool");
                faucet.metrics.total_discarded_coins.inc();
                // Continue the loop to retry preparing the gas coin
                continue;
            }

            GasCoinResponse::NoGasCoinAvailable => return Err(FaucetError::NoGasCoinAvailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::*;
    use shared_crypto::intent::Intent;
    use mys_json_rpc_types::MysExecutionStatus;
    use mys_json_rpc_types::MysTransactionBlockEffects;
    use mys_sdk::wallet_context::WalletContext;
    use mys_types::transaction::SenderSignedData;
    use mys_types::transaction::TransactionDataAPI;
    use test_cluster::TestClusterBuilder;

    async fn execute_tx(
        ctx: &mut WalletContext,
        tx_data: TransactionData,
    ) -> Result<MysTransactionBlockEffects, anyhow::Error> {
        let signature = ctx.config.keystore.sign_secure(
            &tx_data.sender(),
            &tx_data,
            Intent::mys_transaction(),
        )?;
        let sender_signed_data = SenderSignedData::new_from_sender_signature(tx_data, signature);
        let transaction = Transaction::new(sender_signed_data);
        let response = ctx.execute_transaction_may_fail(transaction).await?;
        let result_effects = response.clone().effects;

        if let Some(effects) = result_effects {
            if matches!(effects.status(), MysExecutionStatus::Failure { .. }) {
                Err(anyhow!(
                    "Error executing transaction: {:#?}",
                    effects.status()
                ))
            } else {
                Ok(effects)
            }
        } else {
            Err(anyhow!(
                "Effects from MysTransactionBlockResult should not be empty"
            ))
        }
    }

    #[tokio::test]
    async fn simple_faucet_basic_interface_should_work() {
        telemetry_subscribers::init_for_testing();
        let test_cluster = TestClusterBuilder::new().build().await;
        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();

        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(gas_coins.first().unwrap().0, None, Some(10))
            .await
            .unwrap();
        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();
        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();

        execute_tx(&mut context, tx_data).await.unwrap();

        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();
        // faucet.shutdown_batch_send_task();

        let faucet = Arc::try_unwrap(faucet).unwrap();

        let available = faucet.metrics.total_available_coins.get();
        let discarded = faucet.metrics.total_discarded_coins.get();

        test_basic_interface(&faucet).await;
        test_send_interface_has_success_status(&faucet).await;

        assert_eq!(available, faucet.metrics.total_available_coins.get());
        assert_eq!(discarded, faucet.metrics.total_discarded_coins.get());
    }

    #[tokio::test]
    async fn test_init_gas_queue() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let gas_coins = HashSet::from_iter(gas_coins.into_iter().map(|gas| gas.0));

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();
        faucet.shutdown_batch_send_task();
        let available = faucet.metrics.total_available_coins.get();
        let faucet_unwrapped = &mut Arc::try_unwrap(faucet).unwrap();

        let candidates = faucet_unwrapped.drain_gas_queue(gas_coins.len()).await;

        assert_eq!(available as usize, candidates.len());
        assert_eq!(
            candidates, gas_coins,
            "gases: {:?}, candidates: {:?}",
            gas_coins, candidates
        );
    }

    #[tokio::test]
    async fn test_transfer_state() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let gas_coins = HashSet::from_iter(gas_coins.into_iter().map(|gas| gas.0));

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let number_of_coins = gas_coins.len();
        let amounts = &vec![1; number_of_coins];
        let _ = futures::future::join_all((0..number_of_coins).map(|_| {
            faucet.send(
                Uuid::new_v4(),
                MysAddress::random_for_testing_only(),
                amounts,
            )
        }))
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect::<Vec<_>>();

        // After all transfer requests settle, we still have the original candidates gas in queue.
        let available = faucet.metrics.total_available_coins.get();
        faucet.shutdown_batch_send_task();

        let faucet_unwrapped: &mut SimpleFaucet = &mut Arc::try_unwrap(faucet).unwrap();
        let candidates = faucet_unwrapped.drain_gas_queue(gas_coins.len()).await;
        assert_eq!(available as usize, candidates.len());
        assert_eq!(
            candidates, gas_coins,
            "gases: {:?}, candidates: {:?}",
            gas_coins, candidates
        );
    }

    #[tokio::test]
    async fn test_batch_transfer_interface() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let config: FaucetConfig = Default::default();
        let coin_amount = config.amount;
        let prom_registry = Registry::new();
        let tmp = tempfile::tempdir().unwrap();
        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(gas_coins.first().unwrap().0, None, Some(10))
            .await
            .unwrap();
        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();
        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();

        execute_tx(&mut context, tx_data).await.unwrap();

        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let amounts = &[coin_amount];

        // Create a vector containing five randomly generated addresses
        let target_addresses: Vec<MysAddress> = (0..5)
            .map(|_| MysAddress::random_for_testing_only())
            .collect();

        let response = futures::future::join_all(
            target_addresses
                .iter()
                .map(|address| faucet.batch_send(Uuid::new_v4(), *address, amounts)),
        )
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect::<Vec<BatchFaucetReceipt>>();

        // Assert that all of these return in progress
        let status_results = futures::future::join_all(
            response
                .clone()
                .iter()
                .map(|task| faucet.get_batch_send_status(Uuid::parse_str(&task.task).unwrap())),
        )
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect::<Vec<BatchSendStatus>>();

        for status in status_results {
            assert_eq!(status.status, BatchSendStatusType::INPROGRESS);
        }

        let mut status_results;
        loop {
            // Assert that all of these are SUCCEEDED
            status_results =
                futures::future::join_all(response.clone().iter().map(|task| {
                    faucet.get_batch_send_status(Uuid::parse_str(&task.task).unwrap())
                }))
                .await
                .into_iter()
                .map(|res| res.unwrap())
                .collect::<Vec<BatchSendStatus>>();

            // All requests are submitted and picked up by the same batch, so one success in the test
            // will guarantee all success.
            if status_results[0].status == BatchSendStatusType::SUCCEEDED {
                break;
            }
            info!(
                "Trying to get status again... current is: {:?}",
                status_results[0].status
            );
        }
        for status in status_results {
            assert_eq!(status.status, BatchSendStatusType::SUCCEEDED);
        }
    }

    #[tokio::test]
    async fn test_ttl_cache_expires_after_duration() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let context = test_cluster.wallet;
        // We set it to a fast expiration for the purposes of testing and so these requests don't have time to pass
        // through the batch process.
        let config = FaucetConfig {
            ttl_expiration: 1,
            ..Default::default()
        };
        let prom_registry = Registry::new();
        let tmp = tempfile::tempdir().unwrap();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let amounts = &[1; 1];
        // Create a vector containing five randomly generated addresses
        let target_addresses: Vec<MysAddress> = (0..5)
            .map(|_| MysAddress::random_for_testing_only())
            .collect();

        let response = futures::future::join_all(
            target_addresses
                .iter()
                .map(|address| faucet.batch_send(Uuid::new_v4(), *address, amounts)),
        )
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect::<Vec<BatchFaucetReceipt>>();

        // Check that TTL cache expires
        tokio::time::sleep(Duration::from_secs(10)).await;
        let status_results = futures::future::join_all(
            response
                .clone()
                .iter()
                .map(|task| faucet.get_batch_send_status(Uuid::parse_str(&task.task).unwrap())),
        )
        .await;

        let all_errors = status_results.iter().all(Result::is_err);
        assert!(all_errors);
    }

    #[tokio::test]
    async fn test_discard_invalid_gas() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let context = test_cluster.wallet;
        let mut gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();

        let bad_gas = gas_coins.swap_remove(0);
        let gas_coins = HashSet::from_iter(gas_coins.into_iter().map(|gas| gas.0));

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();

        let client = context.get_client().await.unwrap();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();
        faucet.shutdown_batch_send_task();
        let faucet: &mut SimpleFaucet = &mut Arc::try_unwrap(faucet).unwrap();

        // Now we transfer one gas out
        let gas_budget = 50_000_000;
        let tx_data = client
            .transaction_builder()
            .pay_all_mys(
                address,
                vec![bad_gas.0],
                MysAddress::random_for_testing_only(),
                gas_budget,
            )
            .await
            .unwrap();
        execute_tx(faucet.wallet_mut(), tx_data).await.unwrap();

        let number_of_coins = gas_coins.len();
        let amounts = &vec![1; number_of_coins];
        // We traverse the list twice, which must trigger the transferred gas to be kicked out
        futures::future::join_all((0..2).map(|_| {
            faucet.send(
                Uuid::new_v4(),
                MysAddress::random_for_testing_only(),
                amounts,
            )
        }))
        .await;

        // Verify that the bad gas is no longer in the queue.
        // Note `gases` does not contain the bad gas.
        let available = faucet.metrics.total_available_coins.get();
        let discarded = faucet.metrics.total_discarded_coins.get();
        let candidates = faucet.drain_gas_queue(gas_coins.len()).await;
        assert_eq!(available as usize, candidates.len());
        assert_eq!(discarded, 1);
        assert_eq!(
            candidates, gas_coins,
            "gases: {:?}, candidates: {:?}",
            gas_coins, candidates
        );
    }

    #[tokio::test]
    async fn test_clear_wal() {
        telemetry_subscribers::init_for_testing();
        let test_cluster = TestClusterBuilder::new().build().await;
        let context = test_cluster.wallet;
        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let original_available = faucet.metrics.total_available_coins.get();
        let original_discarded = faucet.metrics.total_discarded_coins.get();

        let recipient = MysAddress::random_for_testing_only();
        let faucet_address = faucet.active_address;
        let uuid = Uuid::new_v4();

        let GasCoinResponse::ValidGasCoin(coin_id) =
            faucet.prepare_gas_coin(100, uuid, false, &faucet.config).await
        else {
            panic!("prepare_gas_coin did not give a valid coin.")
        };

        let tx_data = faucet
            .build_pay_mys_txn(coin_id, faucet_address, recipient, &[100], 200_000_000)
            .await
            .map_err(FaucetError::internal)
            .unwrap();

        let mut wal = faucet.wal.lock().await;

        // Check no WAL
        assert!(wal.log.is_empty());
        wal.reserve(Uuid::new_v4(), coin_id, recipient, tx_data)
            .map_err(FaucetError::internal)
            .ok();
        drop(wal);

        // Check WAL is not empty but will not clear because txn is in_flight
        faucet.retry_wal_coins().await.ok();
        let mut wal = faucet.wal.lock().await;
        assert!(!wal.log.is_empty());

        // Set in flight to false so WAL will clear
        wal.set_in_flight(coin_id, false)
            .expect("Unable to set in flight status to false.");
        drop(wal);

        faucet.retry_wal_coins().await.ok();
        let wal = faucet.wal.lock().await;
        assert!(wal.log.is_empty());

        let total_coins = faucet.metrics.total_available_coins.get();
        let discarded_coins = faucet.metrics.total_discarded_coins.get();
        assert_eq!(total_coins, original_available);
        assert_eq!(discarded_coins, original_discarded);
    }

    #[tokio::test]
    async fn test_discard_smaller_amount_gas() {
        telemetry_subscribers::init_for_testing();
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();

        // split out a coin that has a very small balance such that
        // this coin will be not used later on. This is the new default amount for faucet due to gas changes
        let config = FaucetConfig::default();
        let tiny_value = (config.num_coins as u64 * config.amount) + 1;
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(gas_coins.first().unwrap().0, Some(vec![tiny_value]), None)
            .await
            .unwrap();
        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();
        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();

        let effects = execute_tx(&mut context, tx_data).await.unwrap();

        let tiny_coin_id = effects.created()[0].reference.object_id;

        // Get the latest list of gas
        let gas_coins = context.gas_objects(address).await.unwrap();

        let tiny_amount = gas_coins
            .iter()
            .find(|gas| gas.1.object_id == tiny_coin_id)
            .unwrap()
            .0;
        assert_eq!(tiny_amount, tiny_value);

        let gas_coins: HashSet<ObjectID> =
            HashSet::from_iter(gas_coins.into_iter().map(|gas| gas.1.object_id));

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();
        faucet.shutdown_batch_send_task();

        let faucet: &mut SimpleFaucet = &mut Arc::try_unwrap(faucet).unwrap();

        // Ask for a value higher than tiny coin + DEFAULT_GAS_COMPUTATION_BUCKET
        let number_of_coins = gas_coins.len();
        let amounts = &vec![tiny_value + 1; number_of_coins];
        // We traverse the list ten times, which must trigger the tiny gas to be examined and then discarded
        futures::future::join_all((0..10).map(|_| {
            faucet.send(
                Uuid::new_v4(),
                MysAddress::random_for_testing_only(),
                amounts,
            )
        }))
        .await;
        info!(
            ?number_of_coins,
            "Sent to random addresses: {} {}",
            amounts[0],
            amounts.len(),
        );

        // Verify that the tiny gas is not in the queue.
        tokio::task::yield_now().await;
        let discarded = faucet.metrics.total_discarded_coins.get();

        info!("discarded: {:?}", discarded);
        let candidates = faucet.drain_gas_queue(gas_coins.len() - 1).await;

        assert_eq!(discarded, 1);
        assert!(!candidates.contains(&tiny_coin_id));
    }

    #[tokio::test]
    async fn test_insufficient_balance_will_retry_success() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let config = FaucetConfig::default();

        // The coin that is split off stays because we don't try to refresh the coin vector
        let reasonable_value = (config.num_coins as u64 * config.amount) * 10;
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(
                gas_coins.first().unwrap().0,
                Some(vec![reasonable_value]),
                None,
            )
            .await
            .unwrap();
        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();
        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();
        execute_tx(&mut context, tx_data).await.unwrap();

        let destination_address = MysAddress::random_for_testing_only();
        // Transfer all valid gases away except for 1
        for gas in gas_coins.iter().take(gas_coins.len() - 1) {
            let tx_data = client
                .transaction_builder()
                .transfer_mys(address, gas.0, gas_budget, destination_address, None)
                .await
                .unwrap();
            execute_tx(&mut context, tx_data).await.unwrap();
        }

        // Assert that the coins were transferred away successfully to destination address
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        assert!(!gas_coins.is_empty());

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        // We traverse the list twice, which must trigger the split gas to be kicked out
        futures::future::join_all((0..2).map(|_| {
            faucet.send(
                Uuid::new_v4(),
                MysAddress::random_for_testing_only(),
                &[30000000000],
            )
        }))
        .await;

        // Check that the gas was discarded for being too small
        let discarded = faucet.metrics.total_discarded_coins.get();
        assert_eq!(discarded, 1);

        // Check that the WAL is empty so we don't retry bad requests
        let wal = faucet.wal.lock().await;
        assert!(wal.log.is_empty());
    }

    #[tokio::test]
    async fn test_faucet_no_loop_forever() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let config = FaucetConfig::default();

        let tiny_value = (config.num_coins as u64 * config.amount) + 1;
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(gas_coins.first().unwrap().0, Some(vec![tiny_value]), None)
            .await
            .unwrap();

        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();

        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();

        execute_tx(&mut context, tx_data).await.unwrap();

        let destination_address = MysAddress::random_for_testing_only();

        // Transfer all valid gases away
        for gas in gas_coins {
            let tx_data = client
                .transaction_builder()
                .transfer_mys(address, gas.0, gas_budget, destination_address, None)
                .await
                .unwrap();
            execute_tx(&mut context, tx_data).await.unwrap();
        }

        // Assert that the coins were transferred away successfully to destination address
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(destination_address)
            .await
            .unwrap();
        assert!(!gas_coins.is_empty());

        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let destination_address = MysAddress::random_for_testing_only();
        // Assert that faucet will discard and also terminate
        let res = faucet
            .send(Uuid::new_v4(), destination_address, &[30000000000])
            .await;

        // Assert that the result is an Error
        assert!(matches!(res, Err(FaucetError::NoGasCoinAvailable)));
    }

    #[tokio::test]
    async fn test_faucet_restart_clears_wal() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let context = test_cluster.wallet;
        let tmp = tempfile::tempdir().unwrap();
        let prom_registry = Registry::new();
        let config = FaucetConfig::default();

        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        let recipient = MysAddress::random_for_testing_only();
        let faucet_address = faucet.active_address;
        let uuid = Uuid::new_v4();

        let GasCoinResponse::ValidGasCoin(coin_id) =
            faucet.prepare_gas_coin(100, uuid, false, &faucet.config).await
        else {
            panic!("prepare_gas_coin did not give a valid coin.")
        };

        let tx_data = faucet
            .build_pay_mys_txn(coin_id, faucet_address, recipient, &[100], 200_000_000)
            .await
            .map_err(FaucetError::internal)
            .unwrap();

        let mut wal = faucet.wal.lock().await;

        // Check no WAL
        assert!(wal.log.is_empty());
        wal.reserve(Uuid::new_v4(), coin_id, recipient, tx_data)
            .map_err(FaucetError::internal)
            .ok();
        drop(wal);

        // Check WAL is not empty but will not clear because txn is in_flight
        let mut wal = faucet.wal.lock().await;
        assert!(!wal.log.is_empty());

        // Set in flight to false so WAL will clear
        wal.set_in_flight(coin_id, false)
            .expect("Unable to set in flight status to false.");
        drop(wal);
        faucet.shutdown_batch_send_task();

        let faucet_unwrapped = Arc::try_unwrap(faucet).unwrap();

        let kept_context = faucet_unwrapped.teardown();

        // Simulate a faucet restart and check that it clears the WAL
        let prom_registry_new = Registry::new();

        let faucet_restarted = SimpleFaucet::new(
            kept_context,
            &prom_registry_new,
            &tmp.path().join("faucet.wal"),
            FaucetConfig::default(),
        )
        .await
        .unwrap();

        let restarted_wal = faucet_restarted.wal.lock().await;
        assert!(restarted_wal.log.is_empty())
    }

    #[tokio::test]
    async fn test_amounts_transferred_on_batch() {
        let test_cluster = TestClusterBuilder::new().build().await;
        let config: FaucetConfig = Default::default();
        let address = test_cluster.get_address_0();
        let mut context = test_cluster.wallet;
        let gas_coins = context
            .get_all_gas_objects_owned_by_address(address)
            .await
            .unwrap();
        let client = context.get_client().await.unwrap();
        let tx_kind = client
            .transaction_builder()
            .split_coin_tx_kind(gas_coins.first().unwrap().0, None, Some(10))
            .await
            .unwrap();
        let gas_budget = 50_000_000;
        let rgp = context.get_reference_gas_price().await.unwrap();
        let tx_data = client
            .transaction_builder()
            .tx_data(address, tx_kind, gas_budget, rgp, vec![], None)
            .await
            .unwrap();
        execute_tx(&mut context, tx_data).await.unwrap();

        let prom_registry = Registry::new();
        let tmp = tempfile::tempdir().unwrap();
        let amount_to_send = config.amount;

        let faucet = SimpleFaucet::new(
            context,
            &prom_registry,
            &tmp.path().join("faucet.wal"),
            config,
        )
        .await
        .unwrap();

        // Create a vector containing two randomly generated addresses
        let target_addresses: Vec<MysAddress> = (0..2)
            .map(|_| MysAddress::random_for_testing_only())
            .collect();

        // Send 2 coins of 1 mys each. We
        let coins_sent = 2;
        let amounts = &vec![amount_to_send; coins_sent];

        // Send a request
        let response = futures::future::join_all(
            target_addresses
                .iter()
                .map(|address| faucet.batch_send(Uuid::new_v4(), *address, amounts)),
        )
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect::<Vec<BatchFaucetReceipt>>();

        let mut status_results;
        loop {
            // Assert that all of these are SUCCEEDED
            status_results =
                futures::future::join_all(response.clone().iter().map(|task| {
                    faucet.get_batch_send_status(Uuid::parse_str(&task.task).unwrap())
                }))
                .await
                .into_iter()
                .map(|res| res.unwrap())
                .collect::<Vec<BatchSendStatus>>();

            // All requests are submitted and picked up by the same batch, so one success in the test
            // will guarantee all success.
            if status_results[0].status == BatchSendStatusType::SUCCEEDED {
                break;
            }
            info!(
                "Trying to get status again... current is: {:?}",
                status_results[0].status
            );
        }

        for status in status_results {
            assert_eq!(status.status, BatchSendStatusType::SUCCEEDED);
            let amounts = status.transferred_gas_objects.unwrap().sent;
            assert_eq!(amounts.len(), coins_sent);
            for amt in amounts {
                assert_eq!(amt.amount, amount_to_send);
            }
        }
    }

    async fn test_send_interface_has_success_status(faucet: &impl Faucet) {
        let recipient = MysAddress::random_for_testing_only();
        let amounts = vec![1, 2, 3];
        let uuid_test = Uuid::new_v4();

        faucet.send(uuid_test, recipient, &amounts).await.unwrap();

        let status = faucet.get_batch_send_status(uuid_test).await.unwrap();
        let mut actual_amounts: Vec<u64> = status
            .transferred_gas_objects
            .unwrap()
            .sent
            .iter()
            .map(|c| c.amount)
            .collect();
        actual_amounts.sort_unstable();

        assert_eq!(actual_amounts, amounts);
        assert_eq!(status.status, BatchSendStatusType::SUCCEEDED);
    }

    async fn test_basic_interface(faucet: &impl Faucet) {
        let recipient = MysAddress::random_for_testing_only();
        let amounts = vec![1, 2, 3];

        let FaucetReceipt { sent } = faucet
            .send(Uuid::new_v4(), recipient, &amounts)
            .await
            .unwrap();
        let mut actual_amounts: Vec<u64> = sent.iter().map(|c| c.amount).collect();
        actual_amounts.sort_unstable();
        assert_eq!(actual_amounts, amounts);
    }
}
