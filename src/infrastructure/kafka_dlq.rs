use crate::domain::{Account, AccountEvent};
use crate::infrastructure::kafka_abstraction::{KafkaConfig, KafkaConsumer, KafkaProducer};
use crate::infrastructure::kafka_metrics::KafkaMetrics;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

#[async_trait]
pub trait DeadLetterQueueTrait: Send + Sync {
    async fn send_to_dlq(
        &self,
        account_id: Uuid,
        events: Vec<AccountEvent>,
        version: i64,
        failure_reason: String,
    ) -> Result<()>;
    async fn process_dlq(&self) -> Result<()>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeadLetterMessage {
    pub account_id: Uuid,
    pub events: Vec<AccountEvent>,
    pub version: i64,
    pub original_timestamp: DateTime<Utc>,
    pub failure_reason: String,
    pub retry_count: u32,
    pub last_retry: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct DeadLetterQueue {
    producer: KafkaProducer,
    consumer: KafkaConsumer,
    metrics: Arc<KafkaMetrics>,
    max_retries: u32,
    initial_retry_delay: Duration,
}

impl DeadLetterQueue {
    pub fn new(
        producer: KafkaProducer,
        consumer: KafkaConsumer,
        metrics: Arc<KafkaMetrics>,
        max_retries: u32,
        initial_retry_delay: Duration,
    ) -> Self {
        Self {
            producer,
            consumer,
            metrics,
            max_retries,
            initial_retry_delay,
        }
    }

    pub async fn send_to_dlq(
        &self,
        account_id: Uuid,
        events: Vec<AccountEvent>,
        version: i64,
        failure_reason: String,
    ) -> Result<()> {
        let dlq_message = DeadLetterMessage {
            account_id,
            events,
            version,
            original_timestamp: Utc::now(),
            failure_reason,
            retry_count: 0,
            last_retry: None,
        };

        self.producer.send_dlq_message(&dlq_message).await?;

        self.metrics
            .dlq_messages
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn process_dlq(&self) -> Result<()> {
        loop {
            if let Some(message) = self.consumer.poll_dlq_message().await? {
                if message.retry_count >= self.max_retries {
                    warn!(
                        "Message for account {} exceeded max retries ({}), giving up",
                        message.account_id, self.max_retries
                    );
                    continue;
                }

                self.metrics
                    .dlq_retries
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                // Exponential backoff
                let delay = self.initial_retry_delay * 2u32.pow(message.retry_count);
                sleep(delay).await;

                match self.retry_message(&message).await {
                    Ok(_) => {
                        self.metrics
                            .dlq_retry_success
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        info!(
                            "Successfully retried message for account {}",
                            message.account_id
                        );
                    }
                    Err(e) => {
                        self.metrics
                            .dlq_retry_failures
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        error!(
                            "Failed to retry message for account {}: {}",
                            message.account_id, e
                        );

                        // Update retry count and send back to DLQ
                        let mut updated_message = message;
                        updated_message.retry_count += 1;
                        updated_message.last_retry = Some(Utc::now());

                        self.producer.send_dlq_message(&updated_message).await?;
                    }
                }
            }
        }
    }

    async fn retry_message(&self, message: &DeadLetterMessage) -> Result<()> {
        // Attempt to reprocess the events
        self.producer
            .send_event_batch(message.account_id, message.events.clone(), message.version)
            .await?;

        Ok(())
    }
}

#[async_trait]
impl DeadLetterQueueTrait for DeadLetterQueue {
    async fn send_to_dlq(
        &self,
        account_id: Uuid,
        events: Vec<AccountEvent>,
        version: i64,
        failure_reason: String,
    ) -> Result<()> {
        let message = DeadLetterMessage {
            account_id,
            events,
            version,
            original_timestamp: Utc::now(),
            failure_reason,
            retry_count: 0,
            last_retry: None,
        };

        self.producer.send_dlq_message(&message).await?;
        Ok(())
    }

    async fn process_dlq(&self) -> Result<()> {
        if let Some(message) = self.consumer.poll_dlq_message().await? {
            if message.retry_count >= self.max_retries {
                warn!(
                    "Message for account {} exceeded max retries ({}), giving up",
                    message.account_id, self.max_retries
                );
                return Ok(());
            }

            // Calculate backoff delay
            let delay = self.initial_retry_delay * 2u32.pow(message.retry_count);
            sleep(delay).await;

            // Retry the message
            self.retry_message(&message).await?;
        }
        Ok(())
    }
}
