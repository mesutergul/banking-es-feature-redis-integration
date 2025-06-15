use async_trait::async_trait;
use rdkafka::{
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord},
    Message,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::{Account, AccountEvent};
use crate::infrastructure::kafka_dlq::DeadLetterMessage;
use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use rdkafka::{
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    client::ClientContext,
    error::{KafkaError, KafkaResult, RDKafkaErrorCode},
    message::{Header, Headers, OwnedMessage},
    util::Timeout,
    Offset, TopicPartitionList,
};
use serde_json;
use thiserror::Error;
use tokio::time::timeout;
use tracing::{error, info};

#[derive(Debug, thiserror::Error)]
pub enum BankingKafkaError {
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("Producer error: {0}")]
    ProducerError(String),
    #[error("Consumer error: {0}")]
    ConsumerError(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Deserialization error: {0}")]
    DeserializationError(String),
    #[error("Cache invalidation error: {0}")]
    CacheInvalidationError(String),
    #[error("Event processing error: {0}")]
    EventProcessingError(String),
    #[error("Configuration error: {0}")]
    ConfigurationError(String),
    #[error("Timeout error: {0}")]
    TimeoutError(String),
    #[error("Unknown error: {0}")]
    Unknown(String),
}

impl From<rdkafka::error::KafkaError> for BankingKafkaError {
    fn from(error: rdkafka::error::KafkaError) -> Self {
        match error {
            rdkafka::error::KafkaError::ClientCreation(e) => {
                BankingKafkaError::ConnectionError(e.to_string())
            }
            rdkafka::error::KafkaError::MessageProduction(e) => {
                BankingKafkaError::ProducerError(e.to_string())
            }
            rdkafka::error::KafkaError::MessageConsumption(e) => {
                BankingKafkaError::ConsumerError(e.to_string())
            }
            _ => BankingKafkaError::Unknown(error.to_string()),
        }
    }
}

impl From<serde_json::Error> for BankingKafkaError {
    fn from(error: serde_json::Error) -> Self {
        BankingKafkaError::SerializationError(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct KafkaConfig {
    pub enabled: bool,
    pub bootstrap_servers: String,
    pub group_id: String,
    pub topic_prefix: String,
    pub producer_acks: i16,
    pub producer_retries: i32,
    pub consumer_max_poll_interval_ms: i32,
    pub consumer_session_timeout_ms: i32,
    pub consumer_max_poll_records: i32,
    pub security_protocol: String,
    pub sasl_mechanism: String,
    pub ssl_ca_location: Option<String>,
    pub auto_offset_reset: String,
    pub cache_invalidation_topic: String,
    pub event_topic: String,
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self {
            enabled: true, // Enable Kafka by default
            bootstrap_servers: "localhost:9092".to_string(),
            group_id: "banking-es-group".to_string(),
            topic_prefix: "banking-es".to_string(),
            producer_acks: 1,
            producer_retries: 3,
            consumer_max_poll_interval_ms: 300000,
            consumer_session_timeout_ms: 10000,
            consumer_max_poll_records: 500,
            security_protocol: "PLAINTEXT".to_string(),
            sasl_mechanism: "PLAIN".to_string(),
            ssl_ca_location: None,
            auto_offset_reset: "earliest".to_string(),
            cache_invalidation_topic: "banking-es-cache-invalidation".to_string(),
            event_topic: "banking-es-events".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct KafkaProducer {
    producer: Option<FutureProducer>,
    config: KafkaConfig,
}

impl KafkaProducer {
    pub fn new(config: KafkaConfig) -> Result<Self, BankingKafkaError> {
        if !config.enabled {
            return Ok(Self {
                producer: None,
                config,
            });
        }

        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &config.bootstrap_servers)
            .set("acks", config.producer_acks.to_string())
            .set("retries", config.producer_retries.to_string())
            .create()?;

        Ok(Self {
            producer: Some(producer),
            config,
        })
    }

    pub async fn send_event_batch(
        &self,
        account_id: Uuid,
        events: Vec<AccountEvent>,
        version: i64,
    ) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.producer.is_none() {
            return Ok(());
        }

        let topic = format!("{}-events", self.config.topic_prefix);
        let key = account_id.to_string();

        let batch = EventBatch {
            account_id,
            events,
            version,
            timestamp: Utc::now(),
        };

        let payload = serde_json::to_vec(&batch)
            .map_err(|e| BankingKafkaError::SerializationError(e.to_string()))?;

        self.producer
            .as_ref()
            .unwrap()
            .send(
                FutureRecord::to(&topic)
                    .key(&key)
                    .payload(&payload)
                    .partition(account_id.as_u128() as i32),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| BankingKafkaError::ProducerError(format!("{:?}", e)))?;

        Ok(())
    }

    pub async fn send_cache_update(
        &self,
        account_id: Uuid,
        account: &Account,
    ) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.producer.is_none() {
            return Ok(());
        }

        let topic = format!("{}-cache", self.config.topic_prefix);
        let key = account_id.to_string();

        let payload = serde_json::to_vec(account)
            .map_err(|e| BankingKafkaError::SerializationError(e.to_string()))?;

        self.producer
            .as_ref()
            .unwrap()
            .send(
                FutureRecord::to(&topic)
                    .key(&key)
                    .payload(&payload)
                    .partition(account_id.as_u128() as i32),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| BankingKafkaError::ProducerError(format!("{:?}", e)))?;

        Ok(())
    }

    pub async fn send_dlq_message(
        &self,
        message: &DeadLetterMessage,
    ) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.producer.is_none() {
            return Ok(());
        }

        let topic = format!("{}-dlq", self.config.topic_prefix);
        let key = message.account_id.to_string();

        let payload = serde_json::to_vec(message)
            .map_err(|e| BankingKafkaError::SerializationError(e.to_string()))?;

        self.producer
            .as_ref()
            .unwrap()
            .send(
                FutureRecord::to(&topic)
                    .key(&key)
                    .payload(&payload)
                    .partition(message.account_id.as_u128() as i32),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| BankingKafkaError::ProducerError(format!("{:?}", e)))?;

        Ok(())
    }

    pub async fn send_cache_invalidation(
        &self,
        account_id: Uuid,
        invalidation_type: CacheInvalidationType,
        reason: String,
    ) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.producer.is_none() {
            return Ok(());
        }

        let message = CacheInvalidationMessage {
            account_id,
            invalidation_type,
            timestamp: Utc::now(),
            reason,
        };

        let payload = serde_json::to_vec(&message)?;
        let topic = &self.config.cache_invalidation_topic;

        self.producer
            .as_ref()
            .unwrap()
            .send(
                FutureRecord::to(topic)
                    .payload(&payload)
                    .key(&account_id.to_string()),
                Timeout::After(Duration::from_secs(5)),
            )
            .await
            .map_err(|(e, _)| BankingKafkaError::ProducerError(format!("{:?}", e)))?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct KafkaConsumer {
    consumer: Option<Arc<StreamConsumer>>,
    config: KafkaConfig,
}

impl KafkaConsumer {
    pub fn new(config: KafkaConfig) -> Result<Self, BankingKafkaError> {
        if !config.enabled {
            return Ok(Self {
                consumer: None,
                config,
            });
        }

        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &config.bootstrap_servers)
            .set("group.id", &config.group_id)
            .set("enable.auto.commit", "false")
            .set(
                "max.poll.interval.ms",
                config.consumer_max_poll_interval_ms.to_string(),
            )
            .set(
                "session.timeout.ms",
                config.consumer_session_timeout_ms.to_string(),
            )
            .create()?;

        Ok(Self {
            consumer: Some(Arc::new(consumer)),
            config,
        })
    }

    pub async fn subscribe_to_events(&self) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(());
        }

        let topic = format!("{}-events", self.config.topic_prefix);
        self.consumer.as_ref().unwrap().subscribe(&[&topic])?;
        Ok(())
    }

    pub async fn subscribe_to_cache(&self) -> Result<(), BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(());
        }

        let topic = format!("{}-cache", self.config.topic_prefix);
        self.consumer.as_ref().unwrap().subscribe(&[&topic])?;
        Ok(())
    }

    pub async fn get_last_processed_version(
        &self,
        account_id: Uuid,
    ) -> Result<i64, BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(0);
        }

        let topic = format!("{}-events", self.config.topic_prefix);
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition(&topic, 0);
        self.consumer.as_ref().unwrap().assign(&tpl)?;

        let mut version = 0;
        let mut stream = self.consumer.as_ref().unwrap().stream();

        while let Some(msg) = stream.next().await {
            match msg {
                Ok(msg) => {
                    if let Some(key) = msg.key() {
                        if key == account_id.to_string().as_bytes() {
                            if let Some(payload) = msg.payload() {
                                if let Ok(batch) = serde_json::from_slice::<EventBatch>(payload) {
                                    version = batch.version;
                                }
                            }
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(version)
    }

    pub async fn poll_events(&self) -> Result<Option<EventBatch>, BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(None);
        }

        let mut stream = self.consumer.as_ref().unwrap().stream();
        match timeout(Duration::from_millis(100), stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let payload = msg.payload().ok_or_else(|| {
                    BankingKafkaError::ConsumerError("Empty message payload".to_string())
                })?;

                let batch: EventBatch = serde_json::from_slice(payload).map_err(|e| {
                    BankingKafkaError::ConsumerError("Failed to deserialize message".to_string())
                })?;

                Ok(Some(batch))
            }
            Ok(Some(Err(e))) => Err(e.into()),
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Timeout
        }
    }

    pub async fn poll_cache_updates(&self) -> Result<Option<Account>, BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(None);
        }

        let mut stream = self.consumer.as_ref().unwrap().stream();
        match timeout(Duration::from_millis(100), stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let payload = msg.payload().ok_or_else(|| {
                    BankingKafkaError::ConsumerError("Empty message payload".to_string())
                })?;

                let account: Account = serde_json::from_slice(payload).map_err(|e| {
                    BankingKafkaError::ConsumerError("Failed to deserialize message".to_string())
                })?;

                Ok(Some(account))
            }
            Ok(Some(Err(e))) => Err(BankingKafkaError::ConsumerError(e.to_string())),
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Timeout
        }
    }

    pub async fn poll_cache_invalidations(
        &self,
    ) -> Result<Option<CacheInvalidationMessage>, BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(None);
        }

        let mut stream = self.consumer.as_ref().unwrap().stream();
        match timeout(Duration::from_millis(100), stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let payload = msg.payload().ok_or_else(|| {
                    BankingKafkaError::ConsumerError("Empty message payload".to_string())
                })?;

                let message: CacheInvalidationMessage = serde_json::from_slice(payload)?;
                Ok(Some(message))
            }
            Ok(Some(Err(e))) => Err(BankingKafkaError::ConsumerError(e.to_string())),
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Timeout
        }
    }

    pub async fn poll_dlq_message(&self) -> Result<Option<DeadLetterMessage>, BankingKafkaError> {
        if !self.config.enabled || self.consumer.is_none() {
            return Ok(None);
        }

        let mut stream = self.consumer.as_ref().unwrap().stream();
        match timeout(Duration::from_millis(100), stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let payload = msg.payload().ok_or_else(|| {
                    BankingKafkaError::ConsumerError("Empty message payload".to_string())
                })?;

                let dlq_message: DeadLetterMessage = serde_json::from_slice(payload)?;
                Ok(Some(dlq_message))
            }
            Ok(Some(Err(e))) => Err(BankingKafkaError::ConsumerError(e.to_string())),
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Timeout
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EventBatch {
    pub account_id: Uuid,
    pub events: Vec<AccountEvent>,
    pub version: i64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheInvalidationMessage {
    pub account_id: Uuid,
    pub invalidation_type: CacheInvalidationType,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum CacheInvalidationType {
    AccountUpdate,
    TransactionUpdate,
    FullInvalidation,
    PartialInvalidation,
}
