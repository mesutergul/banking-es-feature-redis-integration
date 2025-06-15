use crate::domain::{Account, AccountEvent};
use crate::infrastructure::event_store::{EventStore, EventStoreTrait};
use crate::infrastructure::kafka_abstraction::{KafkaConfig, KafkaConsumer, KafkaProducer};
use crate::infrastructure::kafka_dlq::DeadLetterQueue;
use crate::infrastructure::kafka_metrics::KafkaMetrics;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug)]
pub enum RecoveryStrategy {
    FullReplay,
    IncrementalReplay,
    SelectiveReplay,
    CacheOnly,
    DLQOnly,
}

pub struct RecoveryStrategies {
    event_store: Arc<dyn EventStoreTrait + Send + Sync>,
    producer: KafkaProducer,
    consumer: KafkaConsumer,
    dlq: Arc<DeadLetterQueue>,
    metrics: Arc<KafkaMetrics>,
}

impl RecoveryStrategies {
    pub fn new(
        event_store: Arc<dyn EventStoreTrait + Send + Sync>,
        producer: KafkaProducer,
        consumer: KafkaConsumer,
        dlq: Arc<DeadLetterQueue>,
        metrics: Arc<KafkaMetrics>,
    ) -> Self {
        Self {
            event_store,
            producer,
            consumer,
            dlq,
            metrics,
        }
    }

    pub async fn execute_recovery(
        &self,
        strategy: RecoveryStrategy,
        account_id: Option<Uuid>,
    ) -> Result<()> {
        match strategy {
            RecoveryStrategy::FullReplay => self.full_replay(account_id).await,
            RecoveryStrategy::IncrementalReplay => self.incremental_replay(account_id).await,
            RecoveryStrategy::SelectiveReplay => self.selective_replay(account_id).await,
            RecoveryStrategy::CacheOnly => self.cache_only_recovery(account_id).await,
            RecoveryStrategy::DLQOnly => self.dlq_recovery(account_id).await,
        }
    }

    async fn full_replay(&self, account_id: Option<Uuid>) -> Result<()> {
        info!("Starting full replay recovery");
        let accounts = if let Some(id) = account_id {
            vec![self
                .event_store
                .get_account(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account {} not found", id))?]
        } else {
            self.event_store.get_all_accounts().await?
        };

        for account in accounts {
            let events = self.event_store.get_events(account.id, None).await?;
            for event in events {
                let account_event: AccountEvent = serde_json::from_value(event.event_data)
                    .context("Failed to deserialize event")?;

                self.producer
                    .send_event_batch(account.id, vec![account_event], event.version)
                    .await?;

                sleep(Duration::from_millis(100)).await;
            }
        }

        info!("Full replay recovery completed");
        Ok(())
    }

    async fn incremental_replay(&self, account_id: Option<Uuid>) -> Result<()> {
        info!("Starting incremental replay recovery");
        let accounts = if let Some(id) = account_id {
            vec![self
                .event_store
                .get_account(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account {} not found", id))?]
        } else {
            self.event_store.get_all_accounts().await?
        };

        for account in accounts {
            let last_processed_version = self.get_last_processed_version(account.id).await?;
            let events = self
                .event_store
                .get_events(account.id, Some(last_processed_version))
                .await?;

            for event in events {
                let account_event: AccountEvent = serde_json::from_value(event.event_data)
                    .context("Failed to deserialize event")?;

                self.producer
                    .send_event_batch(account.id, vec![account_event], event.version)
                    .await?;

                sleep(Duration::from_millis(100)).await;
            }
        }

        info!("Incremental replay recovery completed");
        Ok(())
    }

    async fn selective_replay(&self, account_id: Option<Uuid>) -> Result<()> {
        info!("Starting selective replay recovery");
        let accounts = if let Some(id) = account_id {
            vec![self
                .event_store
                .get_account(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account {} not found", id))?]
        } else {
            self.event_store.get_all_accounts().await?
        };

        for account in accounts {
            let events = self.event_store.get_events(account.id, None).await?;
            let mut current_version = 0;
            let mut batch = Vec::new();

            for event in events {
                if event.version != current_version + 1 {
                    // Found a gap, replay the batch
                    if !batch.is_empty() {
                        self.producer
                            .send_event_batch(account.id, batch.clone(), current_version)
                            .await?;
                        batch.clear();
                    }
                }

                let account_event: AccountEvent = serde_json::from_value(event.event_data)
                    .context("Failed to deserialize event")?;
                batch.push(account_event);
                current_version = event.version;
            }

            // Replay any remaining events
            if !batch.is_empty() {
                self.producer
                    .send_event_batch(account.id, batch, current_version)
                    .await?;
            }
        }

        info!("Selective replay recovery completed");
        Ok(())
    }

    async fn cache_only_recovery(&self, account_id: Option<Uuid>) -> Result<()> {
        info!("Starting cache-only recovery");
        let accounts = if let Some(id) = account_id {
            vec![self
                .event_store
                .get_account(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account {} not found", id))?]
        } else {
            self.event_store.get_all_accounts().await?
        };

        for account in accounts {
            self.producer
                .send_cache_update(account.id, &account)
                .await?;
        }

        info!("Cache-only recovery completed");
        Ok(())
    }

    async fn dlq_recovery(&self, account_id: Option<Uuid>) -> Result<()> {
        info!("Starting DLQ recovery");
        // Process all messages in DLQ
        self.dlq.process_dlq().await?;
        info!("DLQ recovery completed");
        Ok(())
    }

    async fn get_last_processed_version(&self, account_id: Uuid) -> Result<i64> {
        // Get the last processed version from Kafka consumer
        self.consumer
            .get_last_processed_version(account_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get last processed version: {}", e))
    }
}
