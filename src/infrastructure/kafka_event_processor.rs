use crate::domain::{Account, AccountEvent};
use crate::infrastructure::cache_service::{CacheService, CacheServiceTrait};
use crate::infrastructure::event_store::{EventStore, EventStoreTrait};
use crate::infrastructure::kafka_abstraction::{
    EventBatch, KafkaConfig, KafkaConsumer, KafkaProducer,
};
use crate::infrastructure::kafka_dlq::{DeadLetterQueue, DeadLetterQueueTrait};
use crate::infrastructure::kafka_metrics::KafkaMetrics;
use crate::infrastructure::kafka_monitoring::{MonitoringDashboard, MonitoringDashboardTrait};
use crate::infrastructure::kafka_recovery::{KafkaRecovery, KafkaRecoveryTrait};
use crate::infrastructure::kafka_recovery_strategies::{RecoveryStrategies, RecoveryStrategy};
use crate::infrastructure::kafka_tracing::{KafkaTracing, KafkaTracingTrait};
use crate::infrastructure::projections::{ProjectionStore, ProjectionStoreTrait};
use anyhow::{Context, Result};
use async_trait::async_trait;
use rdkafka::error::KafkaError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Default)]
struct ProcessingState {
    is_processing: bool,
    current_batch: Option<EventBatch>,
    last_processed_offset: i64,
}

#[derive(Clone)]
pub struct KafkaEventProcessor {
    producer: KafkaProducer,
    consumer: KafkaConsumer,
    event_store: Arc<dyn EventStoreTrait + Send + Sync>,
    projections: Arc<dyn ProjectionStoreTrait + Send + Sync>,
    dlq: Arc<dyn DeadLetterQueueTrait + Send + Sync>,
    recovery: Arc<dyn KafkaRecoveryTrait + Send + Sync>,
    recovery_strategies: Arc<RecoveryStrategies>,
    metrics: Arc<KafkaMetrics>,
    monitoring: Arc<dyn MonitoringDashboardTrait + Send + Sync>,
    tracing: Arc<dyn KafkaTracingTrait + Send + Sync>,
    cache_service: Arc<dyn CacheServiceTrait + Send + Sync>,
    processing_state: Arc<RwLock<ProcessingState>>,
}

impl KafkaEventProcessor {
    pub fn new(
        config: KafkaConfig,
        event_store: Arc<dyn EventStoreTrait + Send + Sync>,
        projections: Arc<dyn ProjectionStoreTrait + Send + Sync>,
        cache_service: Arc<dyn CacheServiceTrait + Send + Sync>,
    ) -> Result<Self> {
        let metrics = Arc::new(KafkaMetrics::default());
        let producer = KafkaProducer::new(config.clone())?;
        let consumer = KafkaConsumer::new(config.clone())?;

        let dlq = Arc::new(DeadLetterQueue::new(
            producer.clone(),
            consumer.clone(),
            metrics.clone(),
            3,
            Duration::from_secs(1),
        ));

        let recovery = Arc::new(KafkaRecovery::new(
            producer.clone(),
            consumer.clone(),
            event_store.clone(),
            projections.clone(),
            dlq.clone(),
            metrics.clone(),
        ));

        let recovery_strategies = Arc::new(RecoveryStrategies::new(
            event_store.clone(),
            producer.clone(),
            consumer.clone(),
            dlq.clone(),
            metrics.clone(),
        ));

        let monitoring = Arc::new(MonitoringDashboard::new(metrics.clone()));
        let tracing = Arc::new(KafkaTracing::new(metrics.clone()));

        Ok(Self {
            producer,
            consumer,
            event_store,
            projections,
            dlq,
            recovery,
            recovery_strategies,
            metrics,
            monitoring,
            tracing,
            cache_service,
            processing_state: Arc::new(RwLock::new(ProcessingState::default())),
        })
    }

    pub async fn start_processing(&self) -> Result<()> {
        // Initialize tracing
        self.tracing
            .init_tracing()
            .map_err(|e| anyhow::anyhow!("Failed to initialize tracing: {}", e))?;

        self.consumer.subscribe_to_events().await?;

        // Start DLQ processing in background
        let dlq = self.dlq.clone();
        tokio::spawn(async move {
            if let Err(e) = dlq.process_dlq().await {
                error!("DLQ processing failed: {}", e);
            }
        });

        // Start monitoring in background
        let mut monitoring = self.monitoring.clone();
        tokio::spawn(async move {
            loop {
                monitoring.record_metrics();
                monitoring.check_alerts();
                monitoring.update_health_status();
                sleep(Duration::from_secs(60)).await;
            }
        });

        loop {
            let start_time = std::time::Instant::now();

            match self.consumer.poll_events().await {
                Ok(Some(batch)) => {
                    self.metrics
                        .messages_consumed
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.metrics.consume_latency.fetch_add(
                        start_time.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );

                    self.tracing.trace_event_processing(
                        batch.account_id,
                        &batch.events,
                        batch.version,
                    );

                    match self.process_batch(batch.clone()).await {
                        Ok(_) => {
                            self.metrics.events_processed.fetch_add(
                                batch.events.len() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        Err(e) => {
                            error!("Failed to process event batch: {}", e);
                            self.metrics
                                .processing_errors
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            self.tracing
                                .trace_error(&anyhow::anyhow!("{}", e), "batch_processing");

                            // Send to DLQ
                            self.dlq
                                .send_to_dlq(
                                    batch.account_id,
                                    batch.events,
                                    batch.version,
                                    format!("Batch processing failed: {}", e),
                                )
                                .await?;
                        }
                    }
                }
                Ok(None) => {
                    // No messages available, continue polling
                    continue;
                }
                Err(e) => {
                    error!("Error polling Kafka: {}", e);
                    self.metrics
                        .consume_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.tracing
                        .trace_error(&anyhow::anyhow!("{}", e), "kafka_polling");

                    // If we encounter persistent errors, trigger recovery
                    if self.should_trigger_recovery().await {
                        if let Err(e) = self.recovery.start_recovery().await {
                            error!("Recovery failed: {}", e);
                            self.tracing
                                .trace_error(&anyhow::anyhow!("{}", e), "recovery");
                        }
                    }
                }
            }

            // Record metrics periodically
            self.tracing.trace_metrics();
            self.tracing.trace_performance_metrics();
        }
    }

    async fn process_batch(&self, batch: EventBatch) -> Result<()> {
        let start_time = std::time::Instant::now();

        // Save events to event store
        self.event_store
            .save_events(batch.account_id, batch.events.clone(), batch.version)
            .await?;

        // Convert events to versioned format
        let versioned_events: Vec<(i64, AccountEvent)> = batch
            .events
            .iter()
            .enumerate()
            .map(|(i, event)| (batch.version + i as i64, event.clone()))
            .collect();

        // Cache the events
        self.cache_service
            .set_account_events(batch.account_id, &versioned_events, None)
            .await?;

        // Get account from projections
        let account = self.projections.get_account(batch.account_id).await?;
        if let Some(account_proj) = account {
            // Convert projection to account
            let account = Account {
                id: account_proj.id,
                owner_name: account_proj.owner_name,
                balance: account_proj.balance,
                is_active: account_proj.is_active,
                version: batch.version,
            };

            // Cache the account
            self.cache_service
                .set_account(&account, Some(Duration::from_secs(3600)))
                .await?;

            self.producer
                .send_cache_update(batch.account_id, &account)
                .await?;
            self.metrics
                .cache_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.metrics.processing_latency.fetch_add(
            start_time.elapsed().as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok(())
    }

    async fn should_trigger_recovery(&self) -> bool {
        let error_rate = self.metrics.get_error_rate();
        let consumer_lag = self
            .metrics
            .consumer_lag
            .load(std::sync::atomic::Ordering::Relaxed);

        // Trigger recovery if error rate is high or consumer lag is significant
        error_rate > 0.1 || consumer_lag > 1000
    }

    pub async fn get_processing_metrics(&self) -> ProcessingMetrics {
        ProcessingMetrics {
            error_rate: self.metrics.get_error_rate(),
            average_processing_latency: self.metrics.get_average_processing_latency(),
            average_consume_latency: self.metrics.get_average_consume_latency(),
            dlq_retry_success_rate: self.metrics.get_dlq_retry_success_rate(),
            consumer_lag: self
                .metrics
                .consumer_lag
                .load(std::sync::atomic::Ordering::Relaxed),
            events_processed: self
                .metrics
                .events_processed
                .load(std::sync::atomic::Ordering::Relaxed),
            processing_errors: self
                .metrics
                .processing_errors
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub async fn execute_recovery_strategy(
        &self,
        strategy: RecoveryStrategy,
        account_id: Option<Uuid>,
    ) -> Result<()> {
        let strategy_str = format!("{:?}", strategy);
        self.tracing
            .trace_recovery_operation(&strategy_str, account_id, "started");

        match self
            .recovery_strategies
            .execute_recovery(strategy, account_id)
            .await
        {
            Ok(_) => {
                self.tracing
                    .trace_recovery_operation(&strategy_str, account_id, "completed");
                Ok(())
            }
            Err(e) => {
                self.tracing.trace_error(
                    &anyhow::anyhow!("{}", e),
                    &format!("recovery_strategy_{}", strategy_str),
                );
                Err(e)
            }
        }
    }
}

#[derive(Debug)]
pub struct ProcessingMetrics {
    pub error_rate: f64,
    pub average_processing_latency: f64,
    pub average_consume_latency: f64,
    pub dlq_retry_success_rate: f64,
    pub consumer_lag: u64,
    pub events_processed: u64,
    pub processing_errors: u64,
}
