use crate::infrastructure::event_store::DB_POOL;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OnceCell, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Enhanced error types for projections
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),
    #[error("Cache error: {0}")]
    CacheError(String),
    #[error("Batch processing error: {0}")]
    BatchError(String),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

// Projection metrics
#[derive(Debug, Default)]
struct ProjectionMetrics {
    cache_hits: std::sync::atomic::AtomicU64,
    cache_misses: std::sync::atomic::AtomicU64,
    batch_updates: std::sync::atomic::AtomicU64,
    events_processed: std::sync::atomic::AtomicU64,
    errors: std::sync::atomic::AtomicU64,
    query_duration: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountProjection {
    pub id: Uuid,
    pub owner_name: String,
    pub balance: Decimal,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionProjection {
    pub id: Uuid,
    pub account_id: Uuid,
    pub transaction_type: String,
    pub amount: Decimal,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone)]
struct CacheEntry<T> {
    data: T,
    last_accessed: Instant,
    version: u64,
    ttl: Duration,
}

#[derive(Clone)]
pub struct ProjectionStore {
    pool: PgPool,
    account_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<AccountProjection>>>>,
    transaction_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<Vec<TransactionProjection>>>>>,
    update_sender: mpsc::UnboundedSender<ProjectionUpdate>,
    cache_version: Arc<std::sync::atomic::AtomicU64>,
    metrics: Arc<ProjectionMetrics>,
    config: ProjectionConfig,
}

#[derive(Debug, Clone)]
pub struct ProjectionConfig {
    pub cache_ttl_secs: u64,
    pub batch_size: usize,
    pub batch_timeout_ms: u64,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub max_lifetime_secs: u64,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            cache_ttl_secs: 300, // 5 minutes
            batch_size: 5000,
            batch_timeout_ms: 20,
            max_connections: 100,
            min_connections: 20,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 600,
            max_lifetime_secs: 1800,
        }
    }
}

#[derive(Debug)]
enum ProjectionUpdate {
    AccountBatch(Vec<AccountProjection>),
    TransactionBatch(Vec<TransactionProjection>),
}

// Global connection pool
static PROJECTION_POOL: OnceCell<Arc<PgPool>> = OnceCell::const_new();

#[async_trait]
pub trait ProjectionStoreTrait: Send + Sync {
    async fn get_account(&self, account_id: Uuid) -> Result<Option<AccountProjection>>;
    async fn get_all_accounts(&self) -> Result<Vec<AccountProjection>>;
    async fn get_account_transactions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TransactionProjection>>;
    async fn upsert_accounts_batch(&self, accounts: Vec<AccountProjection>) -> Result<()>;
    async fn insert_transactions_batch(
        &self,
        transactions: Vec<TransactionProjection>,
    ) -> Result<()>;
}

#[async_trait]
impl ProjectionStoreTrait for ProjectionStore {
    async fn get_account(&self, account_id: Uuid) -> Result<Option<AccountProjection>> {
        self.get_account(account_id).await
    }

    async fn get_all_accounts(&self) -> Result<Vec<AccountProjection>> {
        self.get_all_accounts().await
    }

    async fn get_account_transactions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TransactionProjection>> {
        self.get_account_transactions(account_id).await
    }

    async fn upsert_accounts_batch(&self, accounts: Vec<AccountProjection>) -> Result<()> {
        self.upsert_accounts_batch(accounts).await
    }

    async fn insert_transactions_batch(
        &self,
        transactions: Vec<TransactionProjection>,
    ) -> Result<()> {
        self.insert_transactions_batch(transactions).await
    }
}

impl ProjectionStore {
    pub fn new(pool: PgPool) -> Self {
        Self::from_pool_with_config(pool, ProjectionConfig::default())
    }

    pub fn new_test(pool: PgPool) -> Self {
        let mut config = ProjectionConfig::default();
        config.batch_size = 1; // Process immediately in test mode
        config.batch_timeout_ms = 0; // No batching in test mode
        let store = Self::from_pool_with_config(pool, config);
        store
    }

    pub fn from_pool_with_config(pool: PgPool, config: ProjectionConfig) -> Self {
        let account_cache = Arc::new(RwLock::new(HashMap::new()));
        let transaction_cache = Arc::new(RwLock::new(HashMap::new()));
        let (update_sender, update_receiver) = mpsc::unbounded_channel();
        let cache_version = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let metrics = Arc::new(ProjectionMetrics::default());

        let store = Self {
            pool: pool.clone(),
            account_cache: account_cache.clone(),
            transaction_cache: transaction_cache.clone(),
            update_sender,
            cache_version,
            metrics,
            config: config.clone(),
        };

        // Only start background processor if not in test mode
        if std::env::var("RUST_TEST").is_err() {
            tokio::spawn(Self::update_processor(
                pool,
                update_receiver,
                account_cache,
                transaction_cache,
                config,
            ));
        }

        store
    }

    pub async fn new_with_config(config: ProjectionConfig) -> Result<Self> {
        // Get or initialize the global connection pool
        let pool = PROJECTION_POOL
            .get_or_try_init(|| async {
                let database_url = std::env::var("DATABASE_URL").map_err(|_| {
                    anyhow::anyhow!("DATABASE_URL environment variable is required")
                })?;

                let pool = PgPoolOptions::new()
                    .max_connections(config.max_connections)
                    .min_connections(config.min_connections)
                    .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
                    .idle_timeout(Duration::from_secs(config.idle_timeout_secs))
                    .max_lifetime(Duration::from_secs(config.max_lifetime_secs))
                    .after_connect(|conn, _meta| {
                        Box::pin(async move {
                            // Set only essential and well-supported PostgreSQL parameters
                            sqlx::query("SET SESSION synchronous_commit = 'off'")
                                .execute(&mut *conn)
                                .await?;
                            sqlx::query("SET SESSION work_mem = '32MB'")
                                .execute(&mut *conn)
                                .await?;
                            sqlx::query("SET SESSION maintenance_work_mem = '128MB'")
                                .execute(&mut *conn)
                                .await?;
                            sqlx::query("SET SESSION effective_cache_size = '2GB'")
                                .execute(&mut *conn)
                                .await?;
                            sqlx::query("SET SESSION random_page_cost = 1.1")
                                .execute(&mut *conn)
                                .await?;
                            // sqlx::query("SET SESSION effective_io_concurrency = 2")
                            //     .execute(&mut *conn)
                            //     .await?;
                            sqlx::query("SET SESSION statement_timeout = '5s'")
                                .execute(&mut *conn)
                                .await?;
                            Ok(())
                        })
                    })
                    .connect(&database_url)
                    .await?;

                Ok::<_, anyhow::Error>(Arc::new(pool))
            })
            .await?;

        Ok(Self::from_pool_with_config(pool.as_ref().clone(), config))
    }

    pub async fn get_account(&self, account_id: Uuid) -> Result<Option<AccountProjection>> {
        let start_time = Instant::now();

        // Try cache first
        {
            let cache = self.account_cache.read().await;
            if let Some(entry) = cache.get(&account_id) {
                if entry.last_accessed.elapsed() < entry.ttl {
                    self.metrics
                        .cache_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(Some(entry.data.clone()));
                }
            }
        }

        self.metrics
            .cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Cache miss - fetch from database with prepared statement
        let account: Option<AccountProjection> = sqlx::query_as!(
            AccountProjection,
            r#"
            SELECT id, owner_name, balance, is_active, created_at, updated_at
            FROM account_projections
            WHERE id = $1
            "#,
            account_id
        )
        .fetch_optional(&self.pool)
        .await?;

        // Update cache if found
        if let Some(ref account) = account {
            let mut cache = self.account_cache.write().await;
            cache.insert(
                account_id,
                CacheEntry {
                    data: account.clone(),
                    last_accessed: Instant::now(),
                    version: self
                        .cache_version
                        .load(std::sync::atomic::Ordering::Relaxed),
                    ttl: Duration::from_secs(self.config.cache_ttl_secs),
                },
            );
        }

        self.metrics.query_duration.fetch_add(
            start_time.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok(account)
    }

    pub async fn get_account_transactions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TransactionProjection>> {
        let start_time = Instant::now();

        // Try cache first
        {
            let cache = self.transaction_cache.read().await;
            if let Some(entry) = cache.get(&account_id) {
                if entry.last_accessed.elapsed() < entry.ttl {
                    self.metrics
                        .cache_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(entry.data.clone());
                }
            }
        }

        self.metrics
            .cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Cache miss - fetch from database with prepared statement
        let transactions = sqlx::query_as!(
            TransactionProjection,
            r#"
            SELECT id, account_id, transaction_type, amount, timestamp
            FROM transaction_projections
            WHERE account_id = $1
            ORDER BY timestamp DESC
            LIMIT 1000
            "#,
            account_id
        )
        .fetch_all(&self.pool)
        .await?;

        // Update cache
        {
            let mut cache = self.transaction_cache.write().await;
            cache.insert(
                account_id,
                CacheEntry {
                    data: transactions.clone(),
                    last_accessed: Instant::now(),
                    version: self
                        .cache_version
                        .load(std::sync::atomic::Ordering::Relaxed),
                    ttl: Duration::from_secs(self.config.cache_ttl_secs),
                },
            );
        }

        self.metrics.query_duration.fetch_add(
            start_time.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok(transactions)
    }

    pub async fn get_all_accounts(&self) -> Result<Vec<AccountProjection>> {
        let start_time = Instant::now();

        let accounts = sqlx::query_as!(
            AccountProjection,
            r#"
            SELECT id, owner_name, balance, is_active, created_at, updated_at
            FROM account_projections
            WHERE is_active = true
            ORDER BY created_at DESC
            LIMIT 10000
            "#
        )
        .fetch_all(&self.pool)
        .await?;

        self.metrics.query_duration.fetch_add(
            start_time.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok(accounts)
    }

    async fn update_processor(
        pool: PgPool,
        mut receiver: mpsc::UnboundedReceiver<ProjectionUpdate>,
        account_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<AccountProjection>>>>,
        transaction_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<Vec<TransactionProjection>>>>>,
        config: ProjectionConfig,
    ) {
        let mut account_batch = Vec::with_capacity(config.batch_size);
        let mut transaction_batch = Vec::with_capacity(config.batch_size);
        let mut last_flush = Instant::now();
        let batch_timeout = Duration::from_millis(config.batch_timeout_ms);
        let cache_version = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let metrics = Arc::new(ProjectionMetrics::default());

        while let Some(update) = receiver.recv().await {
            match update {
                ProjectionUpdate::AccountBatch(accounts) => {
                    account_batch.extend(accounts);
                }
                ProjectionUpdate::TransactionBatch(transactions) => {
                    transaction_batch.extend(transactions);
                }
            }

            // Flush if batch size reached or timeout exceeded
            if account_batch.len() >= config.batch_size
                || transaction_batch.len() >= config.batch_size
                || last_flush.elapsed() >= batch_timeout
            {
                if let Err(e) = Self::flush_batches(
                    &pool,
                    &mut account_batch,
                    &mut transaction_batch,
                    &account_cache,
                    &transaction_cache,
                    &cache_version,
                    &metrics,
                )
                .await
                {
                    error!("Failed to flush batches: {}", e);
                }

                last_flush = Instant::now();
            }
        }
    }

    async fn flush_batches(
        pool: &PgPool,
        account_batch: &mut Vec<AccountProjection>,
        transaction_batch: &mut Vec<TransactionProjection>,
        account_cache: &Arc<RwLock<HashMap<Uuid, CacheEntry<AccountProjection>>>>,
        transaction_cache: &Arc<RwLock<HashMap<Uuid, CacheEntry<Vec<TransactionProjection>>>>>,
        cache_version: &Arc<std::sync::atomic::AtomicU64>,
        metrics: &Arc<ProjectionMetrics>,
    ) -> Result<()> {
        let mut tx = pool.begin().await?;

        // Process account updates
        if !account_batch.is_empty() {
            if let Err(e) = Self::bulk_upsert_accounts(&mut tx, account_batch).await {
                let _ = tx.rollback().await;
                return Err(e.into());
            }

            // Update cache
            {
                let mut cache = account_cache.write().await;
                let version = cache_version.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for account in account_batch.drain(..) {
                    cache.insert(
                        account.id,
                        CacheEntry {
                            data: account,
                            last_accessed: Instant::now(),
                            version,
                            ttl: Duration::from_secs(300), // 5 minutes TTL
                        },
                    );
                }
            }

            metrics
                .batch_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // Process transaction updates
        if !transaction_batch.is_empty() {
            if let Err(e) = Self::bulk_insert_transactions(&mut tx, transaction_batch).await {
                let _ = tx.rollback().await;
                return Err(e.into());
            }

            // Invalidate transaction cache for affected accounts
            {
                let mut cache = transaction_cache.write().await;
                for transaction in transaction_batch.drain(..) {
                    cache.remove(&transaction.account_id);
                }
            }

            metrics
                .batch_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        tx.commit().await?;
        Ok(())
    }

    async fn bulk_upsert_accounts(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        accounts: &[AccountProjection],
    ) -> Result<()> {
        if accounts.is_empty() {
            return Ok(());
        }

        let ids: Vec<Uuid> = accounts.iter().map(|a| a.id).collect();
        let owner_names: Vec<String> = accounts.iter().map(|a| a.owner_name.clone()).collect();
        let balances: Vec<Decimal> = accounts.iter().map(|a| a.balance).collect();
        let is_actives: Vec<bool> = accounts.iter().map(|a| a.is_active).collect();
        let created_ats: Vec<DateTime<Utc>> = accounts.iter().map(|a| a.created_at).collect();
        let updated_ats: Vec<DateTime<Utc>> = accounts.iter().map(|a| a.updated_at).collect();

        sqlx::query!(
            r#"
            INSERT INTO account_projections (id, owner_name, balance, is_active, created_at, updated_at)
            SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::decimal[], $4::boolean[], $5::timestamptz[], $6::timestamptz[])
            ON CONFLICT (id) DO UPDATE SET
                owner_name = EXCLUDED.owner_name,
                balance = EXCLUDED.balance,
                is_active = EXCLUDED.is_active,
                updated_at = EXCLUDED.updated_at
            "#,
            &ids,
            &owner_names,
            &balances,
            &is_actives,
            &created_ats,
            &updated_ats
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    async fn bulk_insert_transactions(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        transactions: &[TransactionProjection],
    ) -> Result<()> {
        if transactions.is_empty() {
            return Ok(());
        }

        let ids: Vec<Uuid> = transactions.iter().map(|t| t.id).collect();
        let account_ids: Vec<Uuid> = transactions.iter().map(|t| t.account_id).collect();
        let types: Vec<String> = transactions
            .iter()
            .map(|t| t.transaction_type.clone())
            .collect();
        let amounts: Vec<Decimal> = transactions.iter().map(|t| t.amount).collect();
        let timestamps: Vec<DateTime<Utc>> = transactions.iter().map(|t| t.timestamp).collect();

        sqlx::query!(
            r#"
            INSERT INTO transaction_projections (id, account_id, transaction_type, amount, timestamp)
            SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::decimal[], $5::timestamptz[])
            ON CONFLICT (id) DO NOTHING
            "#,
            &ids,
            &account_ids,
            &types,
            &amounts,
            &timestamps
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    async fn cache_cleanup_worker(
        account_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<AccountProjection>>>>,
        transaction_cache: Arc<RwLock<HashMap<Uuid, CacheEntry<Vec<TransactionProjection>>>>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(300)); // Every 5 minutes

        loop {
            interval.tick().await;

            let cutoff = Instant::now() - Duration::from_secs(1800); // 30 minutes

            // Clean account cache
            {
                let mut cache = account_cache.write().await;
                cache.retain(|_, entry| entry.last_accessed > cutoff);
            }

            // Clean transaction cache
            {
                let mut cache = transaction_cache.write().await;
                cache.retain(|_, entry| entry.last_accessed > cutoff);
            }
        }
    }

    async fn metrics_reporter(metrics: Arc<ProjectionMetrics>) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            interval.tick().await;

            let hits = metrics
                .cache_hits
                .load(std::sync::atomic::Ordering::Relaxed);
            let misses = metrics
                .cache_misses
                .load(std::sync::atomic::Ordering::Relaxed);
            let batches = metrics
                .batch_updates
                .load(std::sync::atomic::Ordering::Relaxed);
            let errors = metrics.errors.load(std::sync::atomic::Ordering::Relaxed);
            let avg_query_time = metrics
                .query_duration
                .load(std::sync::atomic::Ordering::Relaxed) as f64
                / 1000.0; // Convert to milliseconds

            let hit_rate = if hits + misses > 0 {
                (hits as f64 / (hits + misses) as f64) * 100.0
            } else {
                0.0
            };

            info!(
                "Projection Metrics - Cache Hit Rate: {:.1}%, Batch Updates: {}, Errors: {}, Avg Query Time: {:.2}ms",
                hit_rate, batches, errors, avg_query_time
            );
        }
    }
}

// Add Default implementation for ProjectionStore
impl Default for ProjectionStore {
    fn default() -> Self {
        let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://postgres:Francisco1@localhost:5432/banking_es".to_string()
        });
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_lazy(&database_url)
            .expect("Failed to connect to database");
        ProjectionStore::new(pool)
    }
}
