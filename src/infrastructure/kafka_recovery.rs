use crate::domain::{Account, AccountEvent};
use crate::infrastructure::event_store::EventStoreTrait;
use crate::infrastructure::kafka_abstraction::{KafkaConfig, KafkaConsumer, KafkaProducer};
use crate::infrastructure::kafka_dlq::DeadLetterQueue;
use crate::infrastructure::kafka_metrics::KafkaMetrics;
use crate::infrastructure::projections::ProjectionStoreTrait;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

#[async_trait]
pub trait KafkaRecoveryTrait: Send + Sync {
    async fn start_recovery(&self) -> Result<()>;
}

pub struct KafkaRecovery {
    producer: KafkaProducer,
    consumer: KafkaConsumer,
    event_store: Arc<dyn EventStoreTrait + Send + Sync>,
    projections: Arc<dyn ProjectionStoreTrait + Send + Sync>,
    dlq: Arc<DeadLetterQueue>,
    metrics: Arc<KafkaMetrics>,
    recovery_state: Arc<RwLock<RecoveryState>>,
}

#[derive(Debug, Default)]
struct RecoveryState {
    is_recovering: bool,
    last_processed_offset: i64,
    recovery_start_time: Option<std::time::Instant>,
    accounts_in_recovery: Vec<Uuid>,
}

impl KafkaRecovery {
    pub fn new(
        producer: KafkaProducer,
        consumer: KafkaConsumer,
        event_store: Arc<dyn EventStoreTrait + Send + Sync>,
        projections: Arc<dyn ProjectionStoreTrait + Send + Sync>,
        dlq: Arc<DeadLetterQueue>,
        metrics: Arc<KafkaMetrics>,
    ) -> Self {
        Self {
            producer,
            consumer,
            event_store,
            projections,
            dlq,
            metrics,
            recovery_state: Arc::new(RwLock::new(RecoveryState {
                is_recovering: false,
                last_processed_offset: 0,
                recovery_start_time: None,
                accounts_in_recovery: Vec::new(),
            })),
        }
    }

    pub async fn start_recovery(&self) -> Result<()> {
        let mut state = self.recovery_state.write().await;
        if state.is_recovering {
            warn!("Recovery already in progress");
            return Ok(());
        }

        state.is_recovering = true;
        state.recovery_start_time = Some(std::time::Instant::now());
        drop(state);

        info!("Starting Kafka recovery process");
        self.perform_recovery().await?;

        let mut state = self.recovery_state.write().await;
        state.is_recovering = false;
        state.recovery_start_time = None;
        state.accounts_in_recovery.clear();
        info!("Kafka recovery completed successfully");

        Ok(())
    }

    async fn perform_recovery(&self) -> Result<()> {
        // 1. Get all accounts from projections store
        let account_projections = self.projections.get_all_accounts().await?;

        for projection in account_projections {
            let mut state = self.recovery_state.write().await;
            state.accounts_in_recovery.push(projection.id);
            drop(state);

            // Get the full account from event store
            let account = self.event_store.get_events(projection.id, None).await?;
            let mut reconstructed_account = Account::default();
            reconstructed_account.id = projection.id;

            for event in account {
                let account_event: AccountEvent = serde_json::from_value(event.event_data)
                    .context("Failed to deserialize event")?;
                reconstructed_account.apply_event(&account_event);
            }

            // 2. Verify event consistency
            if let Err(e) = self.verify_account_events(&reconstructed_account).await {
                error!(
                    "Event consistency check failed for account {}: {}",
                    reconstructed_account.id, e
                );
                self.handle_inconsistent_account(&reconstructed_account, e)
                    .await?;
                continue;
            }

            // 3. Replay events if necessary
            if let Err(e) = self.replay_account_events(&reconstructed_account).await {
                error!(
                    "Event replay failed for account {}: {}",
                    reconstructed_account.id, e
                );
                self.handle_replay_failure(&reconstructed_account, e)
                    .await?;
                continue;
            }

            // 4. Update cache
            if let Err(e) = self.update_account_cache(&reconstructed_account).await {
                error!(
                    "Cache update failed for account {}: {}",
                    reconstructed_account.id, e
                );
                self.handle_cache_update_failure(&reconstructed_account, e)
                    .await?;
                continue;
            }

            let mut state = self.recovery_state.write().await;
            state
                .accounts_in_recovery
                .retain(|&id| id != reconstructed_account.id);
        }

        Ok(())
    }

    async fn verify_account_events(&self, account: &Account) -> Result<()> {
        let stored_events = self.event_store.get_events(account.id, None).await?;
        let mut replayed_account = Account::default();
        replayed_account.id = account.id;

        for event in stored_events {
            let account_event: AccountEvent =
                serde_json::from_value(event.event_data).context("Failed to deserialize event")?;
            replayed_account.apply_event(&account_event);
        }

        if replayed_account != *account {
            return Err(anyhow::anyhow!("Account state mismatch after event replay"));
        }

        Ok(())
    }

    async fn replay_account_events(&self, account: &Account) -> Result<()> {
        let stored_events = self.event_store.get_events(account.id, None).await?;

        for event in stored_events {
            let account_event: AccountEvent =
                serde_json::from_value(event.event_data).context("Failed to deserialize event")?;

            // Send event to Kafka for reprocessing
            self.producer
                .send_event_batch(account.id, vec![account_event], event.version)
                .await?;

            // Wait for event to be processed
            sleep(Duration::from_millis(100)).await;
        }

        Ok(())
    }

    async fn update_account_cache(&self, account: &Account) -> Result<()> {
        self.producer.send_cache_update(account.id, account).await?;
        Ok(())
    }

    async fn handle_inconsistent_account(
        &self,
        account: &Account,
        error: anyhow::Error,
    ) -> Result<()> {
        // Send to DLQ for manual inspection
        self.dlq
            .send_to_dlq(
                account.id,
                vec![],
                account.version,
                format!("Event consistency check failed: {}", error),
            )
            .await?;

        Ok(())
    }

    async fn handle_replay_failure(&self, account: &Account, error: anyhow::Error) -> Result<()> {
        // Send to DLQ for retry
        self.dlq
            .send_to_dlq(
                account.id,
                vec![],
                account.version,
                format!("Event replay failed: {}", error),
            )
            .await?;

        Ok(())
    }

    async fn handle_cache_update_failure(
        &self,
        account: &Account,
        error: anyhow::Error,
    ) -> Result<()> {
        // Retry cache update with exponential backoff
        let mut retry_count = 0;
        let max_retries = 3;

        while retry_count < max_retries {
            if let Err(e) = self.update_account_cache(account).await {
                retry_count += 1;
                if retry_count == max_retries {
                    error!(
                        "Cache update failed after {} retries for account {}: {}",
                        max_retries, account.id, e
                    );
                    return Err(e.into());
                }
                sleep(Duration::from_millis(100 * 2u64.pow(retry_count))).await;
            } else {
                break;
            }
        }

        Ok(())
    }

    pub async fn get_recovery_status(&self) -> RecoveryStatus {
        let state = self.recovery_state.read().await;
        RecoveryStatus {
            is_recovering: state.is_recovering,
            accounts_in_recovery: state.accounts_in_recovery.clone(),
            recovery_duration: state
                .recovery_start_time
                .map(|start| start.elapsed())
                .unwrap_or(Duration::from_secs(0)),
        }
    }
}

#[async_trait]
impl KafkaRecoveryTrait for KafkaRecovery {
    async fn start_recovery(&self) -> Result<()> {
        let mut state = self.recovery_state.write().await;
        if state.is_recovering {
            return Ok(());
        }
        state.is_recovering = true;
        state.recovery_start_time = Some(std::time::Instant::now());
        drop(state);

        self.perform_recovery().await
    }
}

#[derive(Debug)]
pub struct RecoveryStatus {
    pub is_recovering: bool,
    pub accounts_in_recovery: Vec<Uuid>,
    pub recovery_duration: Duration,
}
