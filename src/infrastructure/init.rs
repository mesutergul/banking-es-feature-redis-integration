use crate::application::services::AccountService;
use crate::infrastructure::auth::{AuthConfig, AuthService};
use crate::infrastructure::cache_service::{
    CacheConfig, CacheService, CacheServiceTrait, EvictionPolicy,
};
use crate::infrastructure::event_store::{EventStore, EventStoreConfig, EventStoreTrait};
use crate::infrastructure::kafka_abstraction::{KafkaConfig, KafkaConsumer, KafkaProducer};
use crate::infrastructure::kafka_event_processor::KafkaEventProcessor;
use crate::infrastructure::middleware::{
    AccountCreationValidator, RequestMiddleware, TransactionValidator,
};
use crate::infrastructure::projections::{ProjectionConfig, ProjectionStore, ProjectionStoreTrait};
use crate::infrastructure::redis_abstraction::{RealRedisClient, RedisClientTrait};
use crate::infrastructure::repository::{AccountRepository, AccountRepositoryTrait};
use crate::infrastructure::scaling::{ScalingConfig, ScalingManager};
use crate::infrastructure::user_repository::UserRepository;
use anyhow::Result;
use redis;
use std::sync::Arc;
use std::time::Duration;

pub struct ServiceContext {
    pub account_service: Arc<AccountService>,
    pub auth_service: Arc<AuthService>,
    pub scaling_manager: Arc<ScalingManager>,
    pub kafka_processor: Arc<KafkaEventProcessor>,
}

pub async fn init_all_services() -> Result<ServiceContext> {
    // Initialize Redis client
    let redis_client = Arc::new(redis::Client::open("redis://127.0.0.1/")?);
    let redis_client_trait = RealRedisClient::new(redis_client.as_ref().clone(), None);

    // Initialize EventStore
    let event_store: Arc<dyn EventStoreTrait> = Arc::new(EventStore::new_with_pool_size(10).await?);
    let event_store_for_kafka = event_store.clone();

    // Initialize UserRepository
    let user_repository = Arc::new(UserRepository::new(event_store.get_pool().clone()));

    // Initialize AuthService
    let auth_config = AuthConfig {
        jwt_secret: std::env::var("JWT_SECRET").unwrap_or_else(|_| "default_secret".to_string()),
        refresh_token_secret: std::env::var("REFRESH_TOKEN_SECRET")
            .unwrap_or_else(|_| "default_refresh_secret".to_string()),
        access_token_expiry: 3600,
        refresh_token_expiry: 604800,
        rate_limit_requests: 1000,
        rate_limit_window: 60,
        max_failed_attempts: 5,
        lockout_duration_minutes: 30,
    };
    let auth_service = Arc::new(AuthService::new(
        redis_client.clone(),
        auth_config,
        user_repository,
    ));

    // Initialize ProjectionStore
    let projection_config = ProjectionConfig::default();
    let projection_store: Arc<dyn ProjectionStoreTrait> =
        Arc::new(ProjectionStore::new_with_config(projection_config).await?);
    let projection_store_for_kafka = projection_store.clone();

    // Initialize CacheService
    let cache_config = CacheConfig {
        default_ttl: Duration::from_secs(3600),
        max_size: 10000,
        shard_count: 16,
        warmup_batch_size: 100,
        warmup_interval: Duration::from_secs(300),
        eviction_policy: EvictionPolicy::LRU,
    };
    let cache_service: Arc<dyn CacheServiceTrait> =
        Arc::new(CacheService::new(redis_client_trait.clone(), cache_config));
    let cache_service_for_kafka = cache_service.clone();

    // Initialize AccountRepository
    let account_repository: Arc<dyn AccountRepositoryTrait> =
        Arc::new(AccountRepository::new(event_store));

    // Initialize RequestMiddleware
    let rate_limit_config = crate::infrastructure::middleware::RateLimitConfig {
        requests_per_minute: 100,
        burst_size: 20,
        window_size: Duration::from_secs(60),
        max_clients: 1000,
    };
    let middleware = Arc::new(RequestMiddleware::new(rate_limit_config));
    middleware.register_validator(
        "create_account".to_string(),
        Box::new(AccountCreationValidator),
    );
    middleware.register_validator("deposit_money".to_string(), Box::new(TransactionValidator));
    middleware.register_validator("withdraw_money".to_string(), Box::new(TransactionValidator));

    // Initialize ScalingManager
    let scaling_config = ScalingConfig {
        min_instances: 1,
        max_instances: 10,
        scale_up_threshold: 0.8,
        scale_down_threshold: 0.2,
        cooldown_period: Duration::from_secs(300),
        health_check_interval: Duration::from_secs(30),
        instance_timeout: Duration::from_secs(60),
    };
    let scaling_manager = Arc::new(ScalingManager::new(
        redis_client_trait.clone(),
        scaling_config,
    ));

    // Initialize AccountService
    let account_service = Arc::new(AccountService::new(
        account_repository,
        projection_store,
        cache_service,
        middleware,
        100,
    ));

    // Initialize Kafka
    let kafka_config = KafkaConfig {
        enabled: true,
        bootstrap_servers: std::env::var("KAFKA_BOOTSTRAP_SERVERS")
            .unwrap_or_else(|_| "localhost:9092".to_string()),
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
    };

    // Initialize KafkaEventProcessor
    let kafka_processor = Arc::new(KafkaEventProcessor::new(
        kafka_config,
        event_store_for_kafka,
        projection_store_for_kafka,
        cache_service_for_kafka,
    )?);

    // Create ServiceContext
    let service_context = ServiceContext {
        account_service,
        auth_service,
        scaling_manager,
        kafka_processor,
    };

    Ok(service_context)
}
