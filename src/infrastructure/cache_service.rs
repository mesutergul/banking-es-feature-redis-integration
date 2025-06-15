use crate::domain::{Account, AccountEvent};
use crate::infrastructure::redis_abstraction::RealRedisClient;
use crate::infrastructure::redis_abstraction::RedisConnectionCommands;
use crate::infrastructure::redis_abstraction::{
    CircuitBreakerConfig, LoadShedderConfig, RedisClientTrait, RedisPipeline, RedisPoolConfig,
};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use redis::Client;
use redis::RedisError;
use redis::{aio::MultiplexedConnection, ConnectionInfo, Value as RedisValue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{duplex, AsyncRead, AsyncWrite};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub default_ttl: Duration,
    pub max_size: usize,
    pub shard_count: usize,
    pub warmup_batch_size: usize,
    pub warmup_interval: Duration,
    pub eviction_policy: EvictionPolicy,
}

#[derive(Debug, Clone)]
pub enum EvictionPolicy {
    LRU,
    LFU,
    TTL,
}

#[derive(Debug)]
pub struct CacheMetrics {
    pub hits: std::sync::atomic::AtomicU64,
    pub misses: std::sync::atomic::AtomicU64,
    pub evictions: std::sync::atomic::AtomicU64,
    pub errors: std::sync::atomic::AtomicU64,
    pub warmups: std::sync::atomic::AtomicU64,
    pub shard_hits: std::sync::atomic::AtomicU64,
    pub shard_misses: std::sync::atomic::AtomicU64,
}

impl Default for CacheMetrics {
    fn default() -> Self {
        Self {
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            evictions: std::sync::atomic::AtomicU64::new(0),
            errors: std::sync::atomic::AtomicU64::new(0),
            warmups: std::sync::atomic::AtomicU64::new(0),
            shard_hits: std::sync::atomic::AtomicU64::new(0),
            shard_misses: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    pub value: T,
    pub created_at: Instant,
    pub last_accessed: Instant,
    pub access_count: u64,
    pub ttl: Duration,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self) -> bool {
        self.last_accessed.elapsed() > self.ttl
    }
}

#[derive(Clone)]
pub struct CacheService {
    redis_client: Arc<dyn RedisClientTrait>,
    config: CacheConfig,
    metrics: Arc<CacheMetrics>,
    shards: Arc<Vec<DashMap<Uuid, CacheEntry<Account>>>>,
    event_cache: DashMap<Uuid, Vec<(i64, AccountEvent)>>,
    warming_state: Arc<RwLock<WarmingState>>,
}

#[derive(Debug, Default)]
struct WarmingState {
    is_warming: bool,
    last_warmup: Option<Instant>,
    accounts_to_warm: Vec<Uuid>,
}

#[async_trait]
pub trait CacheServiceTrait: Send + Sync {
    async fn get_account(&self, account_id: Uuid) -> Result<Option<Account>>;
    async fn set_account(&self, account: &Account, ttl: Option<Duration>) -> Result<()>;
    async fn delete_account(&self, account_id: Uuid) -> Result<()>;
    async fn get_account_events(&self, account_id: Uuid) -> Result<Option<Vec<AccountEvent>>>;
    async fn set_account_events(
        &self,
        account_id: Uuid,
        events: &[(i64, AccountEvent)],
        ttl: Option<Duration>,
    ) -> Result<()>;
    async fn delete_account_events(&self, account_id: Uuid) -> Result<()>;
    async fn invalidate_account(&self, account_id: Uuid) -> Result<()>;
    async fn warmup_cache(&self, account_ids: Vec<Uuid>) -> Result<()>;
    fn get_metrics(&self) -> &CacheMetrics;
}

#[async_trait]
impl CacheServiceTrait for CacheService {
    async fn get_account(&self, account_id: Uuid) -> Result<Option<Account>> {
        self.get_account(account_id).await
    }

    async fn set_account(&self, account: &Account, ttl: Option<Duration>) -> Result<()> {
        self.set_account(account, ttl).await
    }

    async fn delete_account(&self, account_id: Uuid) -> Result<()> {
        self.delete_account(account_id).await
    }

    async fn get_account_events(&self, account_id: Uuid) -> Result<Option<Vec<AccountEvent>>> {
        self.get_account_events(account_id).await
    }

    async fn set_account_events(
        &self,
        account_id: Uuid,
        events: &[(i64, AccountEvent)],
        ttl: Option<Duration>,
    ) -> Result<()> {
        self.set_account_events(account_id, events, ttl).await
    }

    async fn delete_account_events(&self, account_id: Uuid) -> Result<()> {
        self.delete_account_events(account_id).await
    }

    async fn invalidate_account(&self, account_id: Uuid) -> Result<()> {
        self.invalidate_account(account_id).await
    }

    async fn warmup_cache(&self, account_ids: Vec<Uuid>) -> Result<()> {
        self.warmup_cache(account_ids).await
    }

    fn get_metrics(&self) -> &CacheMetrics {
        self.get_metrics()
    }
}

impl CacheService {
    pub fn new(redis_client: Arc<dyn RedisClientTrait>, config: CacheConfig) -> Self {
        let shards = Arc::new(
            (0..config.shard_count)
                .map(|_| DashMap::new())
                .collect::<Vec<_>>(),
        );
        let event_cache = DashMap::new();
        let warming_state = Arc::new(RwLock::new(WarmingState {
            is_warming: false,
            last_warmup: None,
            accounts_to_warm: Vec::new(),
        }));
        let metrics = Arc::new(CacheMetrics::default());

        let service = Self {
            redis_client,
            config,
            metrics,
            shards,
            event_cache,
            warming_state,
        };

        // Only start background tasks if not in test mode
        if std::env::var("RUST_TEST").is_err() {
            // Start cache cleanup task
            let shards_clone = service.shards.clone();
            let config_clone = service.config.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(300)); // Every 5 minutes
                loop {
                    interval.tick().await;
                    for shard in shards_clone.iter() {
                        shard.retain(|_, entry| !entry.is_expired());
                    }
                }
            });

            // Start metrics reporter
            let metrics_clone = service.metrics.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let hits = metrics_clone
                        .hits
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let misses = metrics_clone
                        .misses
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let evictions = metrics_clone
                        .evictions
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let errors = metrics_clone
                        .errors
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let warmups = metrics_clone
                        .warmups
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let hit_rate = if hits + misses > 0 {
                        (hits as f64 / (hits + misses) as f64) * 100.0
                    } else {
                        0.0
                    };
                    info!(
                        "Cache Metrics - Hit Rate: {:.1}%, Evictions: {}, Errors: {}, Warmups: {}",
                        hit_rate, evictions, errors, warmups
                    );
                }
            });
        }

        service
    }

    pub fn new_test(redis_client: Arc<dyn RedisClientTrait>) -> Self {
        let mut config = CacheConfig::default();
        config.default_ttl = Duration::from_secs(60); // Shorter TTL for tests
        config.max_size = 1000;
        config.shard_count = 4;
        config.warmup_batch_size = 50;
        config.warmup_interval = Duration::from_secs(60);
        Self::new(redis_client, config)
    }

    pub async fn initialize(&self) -> Result<()> {
        let mut state = self.warming_state.write().await;
        if state.is_warming {
            return Ok(());
        }

        // Verify Redis connection
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let _: Option<String> = conn.get("test").await.ok();

        state.is_warming = true;
        state.accounts_to_warm = Vec::new();
        drop(state);

        info!("Cache service initialized successfully");
        Ok(())
    }

    pub async fn get_account(&self, account_id: Uuid) -> Result<Option<Account>> {
        // Try in-memory cache first
        let shard_index = self.get_shard_index(account_id);
        if let Some(entry) = self.shards[shard_index].get(&account_id) {
            if !self.is_expired(&entry) {
                self.metrics
                    .shard_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Some(entry.value.clone()));
            }
            // Remove expired entry
            self.shards[shard_index].remove(&account_id);
        }
        self.metrics
            .shard_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Try Redis cache
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("account:{}", account_id);

        match conn.get(key.as_bytes()).await {
            Ok(redis::Value::Data(data)) => {
                match serde_json::from_slice::<Account>(&data) {
                    Ok(account) => {
                        self.metrics
                            .hits
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Update in-memory cache
                        self.update_in_memory_cache(account_id, account.clone());
                        Ok(Some(account))
                    }
                    Err(e) => {
                        self.metrics
                            .errors
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        error!("Failed to deserialize account from cache: {}", e);
                        Ok(None)
                    }
                }
            }
            Ok(_) => {
                self.metrics
                    .misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => {
                self.metrics
                    .errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                error!("Redis error while getting account: {}", e);
                Err(e.into())
            }
        }
    }

    pub async fn set_account(&self, account: &Account, ttl: Option<Duration>) -> Result<()> {
        let ttl = ttl.unwrap_or(self.config.default_ttl);
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("account:{}", account.id);
        let value = serde_json::to_vec(account)?;

        conn.set_ex(key.as_bytes(), &value, ttl.as_secs()).await?;

        // Update in-memory cache
        self.update_in_memory_cache(account.id, account.clone());

        Ok(())
    }

    pub async fn delete_account(&self, account_id: Uuid) -> Result<()> {
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("account:{}", account_id);

        conn.del(key.as_bytes()).await?;
        self.metrics
            .evictions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn get_account_events(&self, account_id: Uuid) -> Result<Option<Vec<AccountEvent>>> {
        // Try in-memory event cache first
        if let Some(events) = self.event_cache.get(&account_id) {
            return Ok(Some(
                events.iter().map(|(_, event)| event.clone()).collect(),
            ));
        }

        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("events:{}", account_id);

        match conn.get(key.as_bytes()).await {
            Ok(redis::Value::Data(data)) => {
                match serde_json::from_slice::<Vec<(i64, AccountEvent)>>(&data) {
                    Ok(events) => {
                        self.metrics
                            .hits
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Update in-memory cache
                        self.event_cache.insert(account_id, events.clone());
                        Ok(Some(events.into_iter().map(|(_, event)| event).collect()))
                    }
                    Err(e) => {
                        self.metrics
                            .errors
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        error!("Failed to deserialize events from cache: {}", e);
                        Ok(None)
                    }
                }
            }
            Ok(_) => {
                self.metrics
                    .misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => {
                self.metrics
                    .errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                error!("Redis error while getting events: {}", e);
                Err(e.into())
            }
        }
    }

    pub async fn set_account_events(
        &self,
        account_id: Uuid,
        events: &[(i64, AccountEvent)],
        ttl: Option<Duration>,
    ) -> Result<()> {
        let ttl = ttl.unwrap_or(self.config.default_ttl);
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("events:{}", account_id);
        let value = serde_json::to_vec(events)?;

        conn.set_ex(key.as_bytes(), &value, ttl.as_secs()).await?;

        // Update in-memory cache
        self.event_cache.insert(account_id, events.to_vec());

        Ok(())
    }

    pub async fn delete_account_events(&self, account_id: Uuid) -> Result<()> {
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let key = format!("events:{}", account_id);

        conn.del(key.as_bytes()).await?;
        self.metrics
            .evictions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn invalidate_account(&self, account_id: Uuid) -> Result<()> {
        // Invalidate in-memory cache
        let shard_index = self.get_shard_index(account_id);
        self.shards[shard_index].remove(&account_id);
        self.event_cache.remove(&account_id);

        // Invalidate Redis cache
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;
        let account_key = format!("account:{}", account_id);
        let events_key = format!("events:{}", account_id);

        conn.del(account_key.as_bytes()).await?;
        conn.del(events_key.as_bytes()).await?;

        self.metrics
            .evictions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn warmup_cache(&self, account_ids: Vec<Uuid>) -> Result<()> {
        let mut state = self.warming_state.write().await;
        if state.is_warming {
            return Ok(());
        }

        state.is_warming = true;
        state.accounts_to_warm = account_ids.clone();
        drop(state);

        let batch_size = self.config.warmup_batch_size;
        let mut conn = self.redis_client.get_connection().await?;
        use redis::AsyncCommands;

        for chunk in account_ids.chunks(batch_size) {
            let mut pipeline = redis::pipe();

            for &account_id in chunk {
                let account_key = format!("account:{}", account_id);
                let events_key = format!("events:{}", account_id);
                pipeline.get(account_key);
                pipeline.get(events_key);
            }

            let results: Vec<redis::Value> = pipeline.query_async(&mut conn).await?;

            for (i, &account_id) in chunk.iter().enumerate() {
                let account_bytes = match &results[i * 2] {
                    redis::Value::Data(bytes) => bytes.as_slice(),
                    _ => &[],
                };
                let events_bytes = match &results[i * 2 + 1] {
                    redis::Value::Data(bytes) => bytes.as_slice(),
                    _ => &[],
                };
                if let (Ok(account_data), Ok(events_data)) = (
                    serde_json::from_slice::<Account>(account_bytes),
                    serde_json::from_slice::<Vec<(i64, AccountEvent)>>(events_bytes),
                ) {
                    self.update_in_memory_cache(account_id, account_data);
                    self.event_cache.insert(account_id, events_data);
                    self.metrics
                        .warmups
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        let mut state = self.warming_state.write().await;
        state.is_warming = false;
        state.last_warmup = Some(Instant::now());
        state.accounts_to_warm.clear();

        Ok(())
    }

    fn get_shard_index(&self, account_id: Uuid) -> usize {
        (account_id.as_u128() % self.config.shard_count as u128) as usize
    }

    fn is_expired(&self, entry: &CacheEntry<Account>) -> bool {
        entry.last_accessed.elapsed() >= entry.ttl
    }

    fn update_in_memory_cache(&self, account_id: Uuid, account: Account) {
        let shard_index = self.get_shard_index(account_id);
        let entry = CacheEntry {
            value: account,
            created_at: Instant::now(),
            last_accessed: Instant::now(),
            access_count: 1,
            ttl: self.config.default_ttl,
        };

        // Apply eviction policy if needed
        if self.shards[shard_index].len() >= self.config.max_size {
            self.evict_entries(shard_index);
        }

        self.shards[shard_index].insert(account_id, entry);
    }

    fn evict_entries(&self, shard_index: usize) {
        match self.config.eviction_policy {
            EvictionPolicy::LRU => {
                if let Some(entry) = self.shards[shard_index]
                    .iter()
                    .min_by_key(|entry| entry.last_accessed)
                {
                    self.shards[shard_index].remove(entry.key());
                }
            }
            EvictionPolicy::LFU => {
                if let Some(entry) = self.shards[shard_index]
                    .iter()
                    .min_by_key(|entry| entry.access_count)
                {
                    self.shards[shard_index].remove(entry.key());
                }
            }
            EvictionPolicy::TTL => {
                if let Some(entry) = self.shards[shard_index]
                    .iter()
                    .min_by_key(|entry| entry.created_at)
                {
                    self.shards[shard_index].remove(entry.key());
                }
            }
        }
    }

    pub fn get_metrics(&self) -> &CacheMetrics {
        self.metrics.as_ref()
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(3600),
            max_size: 10000,
            shard_count: 16,
            warmup_batch_size: 100,
            warmup_interval: Duration::from_secs(300),
            eviction_policy: EvictionPolicy::LRU,
        }
    }
}

impl Default for CacheService {
    fn default() -> Self {
        let redis_client = RealRedisClient::new(
            redis::Client::open("redis://localhost:6379").expect("Failed to connect to Redis"),
            None,
        );
        let cache_config = CacheConfig::default();
        CacheService::new(redis_client, cache_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Client;

    struct RedisConnection {
        conn: redis::aio::Connection,
    }

    impl RedisConnection {
        fn new(conn: redis::aio::Connection) -> Self {
            Self { conn }
        }
    }

    #[async_trait]
    impl RedisConnectionCommands for RedisConnection {
        async fn execute_pipeline(
            &mut self,
            pipeline: &RedisPipeline,
        ) -> Result<Vec<RedisValue>, RedisError> {
            pipeline.execute_pipeline(self).await
        }
    }

    struct TestRedisClient {
        client: Client,
    }

    #[async_trait]
    impl RedisClientTrait for TestRedisClient {
        async fn get_connection(&self) -> Result<MultiplexedConnection, RedisError> {
            self.client.get_multiplexed_async_connection().await
        }

        fn clone_client(&self) -> Arc<dyn RedisClientTrait> {
            Arc::new(TestRedisClient {
                client: self.client.clone(),
            })
        }

        async fn get_pooled_connection(
            &self,
        ) -> Result<Box<dyn RedisConnectionCommands + Send>, RedisError> {
            let conn = self.client.get_async_connection().await?;
            Ok(Box::new(RedisConnection::new(conn)))
        }

        fn get_pool_config(&self) -> RedisPoolConfig {
            RedisPoolConfig::default()
        }

        async fn get(&self, key: &str) -> Result<Option<String>, RedisError> {
            let mut conn = self.client.get_async_connection().await?;
            redis::cmd("GET").arg(key).query_async(&mut conn).await
        }

        async fn set(&self, key: &str, value: &str) -> Result<(), RedisError> {
            let mut conn = self.client.get_async_connection().await?;
            redis::cmd("SET")
                .arg(key)
                .arg(value)
                .query_async(&mut conn)
                .await
        }

        async fn del(&self, key: &str) -> Result<(), RedisError> {
            let mut conn = self.client.get_async_connection().await?;
            redis::cmd("DEL").arg(key).query_async(&mut conn).await
        }
    }

    #[tokio::test]
    async fn test_cache_service_initialization() {
        let client = Client::open("redis://127.0.0.1/").unwrap();
        let redis_client = TestRedisClient { client };
        let cache_service = CacheService::new(Arc::new(redis_client), CacheConfig::default());
        assert!(cache_service.initialize().await.is_ok());
    }

    #[tokio::test]
    async fn test_get_account_cache_hit() {
        let client = Client::open("redis://127.0.0.1/").unwrap();
        let redis_client = TestRedisClient { client };
        let account_id = Uuid::new_v4();
        let account = Account {
            id: account_id,
            owner_name: "Test User".to_string(),
            balance: 1000.into(),
            is_active: true,
            version: 1,
        };

        let cache_service = CacheService::new(Arc::new(redis_client), CacheConfig::default());
        cache_service.set_account(&account, None).await.unwrap();

        let result = cache_service.get_account(account_id).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap().id, account_id);
    }
}
