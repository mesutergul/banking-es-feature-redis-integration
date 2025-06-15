use banking_es::web;
use banking_es::{
    application::services::AccountService,
    domain::AccountError,
    infrastructure::{
        cache_service::{CacheConfig, CacheService, CacheServiceTrait, EvictionPolicy},
        event_store::{EventStore, EventStoreTrait},
        projections::{
            AccountProjection, ProjectionConfig, ProjectionStore, ProjectionStoreTrait,
            TransactionProjection,
        },
        redis_abstraction::RealRedisClient,
        repository::AccountRepository,
    },
};
use rand;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use redis;
use rust_decimal::Decimal;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use std::time::Duration;
use tokio;
use tokio::sync::mpsc;
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

struct TestContext {
    account_service: Arc<AccountService>,
    account_repository: Arc<AccountRepository>,
    db_pool: PgPool,
    _shutdown_tx: mpsc::Sender<()>,
    _background_tasks: Vec<JoinHandle<()>>,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        // Send shutdown signal
        let _ = self._shutdown_tx.try_send(());

        // Wait for background tasks to complete
        for handle in self._background_tasks.drain(..) {
            let _ = handle.abort();
        }
    }
}

// Test-specific ProjectionStore that doesn't use background tasks
#[derive(Clone)]
struct TestProjectionStore {
    pool: PgPool,
    account_cache: Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, AccountProjection>>>,
    transaction_cache:
        Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, Vec<TransactionProjection>>>>,
}

impl TestProjectionStore {
    fn new(pool: PgPool) -> Self {
        Self {
            pool,
            account_cache: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            transaction_cache: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    async fn get_account(
        &self,
        account_id: Uuid,
    ) -> Result<Option<AccountProjection>, anyhow::Error> {
        // Try cache first
        {
            let cache = self.account_cache.read().await;
            if let Some(account) = cache.get(&account_id) {
                return Ok(Some(account.clone()));
            }
        }

        // Cache miss - fetch from database
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
            cache.insert(account_id, account.clone());
        }

        Ok(account)
    }

    async fn get_account_transactions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TransactionProjection>, anyhow::Error> {
        // Try cache first
        {
            let cache = self.transaction_cache.read().await;
            if let Some(transactions) = cache.get(&account_id) {
                return Ok(transactions.clone());
            }
        }

        // Cache miss - fetch from database
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
            cache.insert(account_id, transactions.clone());
        }

        Ok(transactions)
    }

    async fn upsert_accounts_batch(
        &self,
        accounts: Vec<AccountProjection>,
    ) -> Result<(), anyhow::Error> {
        for account in accounts {
            let mut cache = self.account_cache.write().await;
            cache.insert(account.id, account);
        }
        Ok(())
    }

    async fn insert_transactions_batch(
        &self,
        transactions: Vec<TransactionProjection>,
    ) -> Result<(), anyhow::Error> {
        for transaction in transactions {
            let mut cache = self.transaction_cache.write().await;
            let account_transactions = cache.entry(transaction.account_id).or_insert_with(Vec::new);
            account_transactions.push(transaction);
        }
        Ok(())
    }
}

// Implement the same traits as ProjectionStore
impl std::ops::Deref for TestProjectionStore {
    type Target = ProjectionStore;
    fn deref(&self) -> &Self::Target {
        // This is a hack to make it work with the existing code
        // In a real implementation, we would need to implement all the methods
        unimplemented!("TestProjectionStore should not be dereferenced")
    }
}

async fn setup_test_environment() -> Result<TestContext, Box<dyn std::error::Error>> {
    // Create shutdown channel for cleanup
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    let mut background_tasks = Vec::new();

    // Initialize database pool with minimal connections for tests
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://postgres:Francisco1@localhost:5432/banking_es".to_string()
    });

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    // Initialize Redis client with test-specific config
    let redis_client = Arc::new(redis::Client::open("redis://127.0.0.1/")?);
    let redis_client_trait = RealRedisClient::new(redis_client.as_ref().clone(), None);

    // Initialize services with test-specific configs
    let event_store = Arc::new(EventStore::new(pool.clone())) as Arc<dyn EventStoreTrait + 'static>;
    let projection_store = Arc::new(ProjectionStore::new_test(pool.clone()))
        as Arc<dyn ProjectionStoreTrait + 'static>;
    let cache_service = Arc::new(CacheService::new_test(redis_client_trait))
        as Arc<dyn CacheServiceTrait + 'static>;
    let repository: Arc<AccountRepository> = Arc::new(AccountRepository::new(event_store));
    let repository_clone = repository.clone();

    let service = Arc::new(AccountService::new(
        repository,
        projection_store,
        cache_service,
        Arc::new(Default::default()),
        10, // Lower concurrency for tests
    ));

    // Start cleanup task
    let service_clone = service.clone();
    let cleanup_handle = tokio::spawn(async move {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                // Cleanup code here
                // No cleanup needed as AccountService handles its own cleanup
            }
        }
    });
    background_tasks.push(cleanup_handle);

    Ok(TestContext {
        account_service: service,
        account_repository: repository_clone,
        db_pool: pool,
        _shutdown_tx: shutdown_tx,
        _background_tasks: background_tasks,
    })
}

// Helper function to run async operations with timeout
async fn with_timeout<F, T>(
    future: F,
    timeout_duration: Duration,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = Result<T, Box<dyn std::error::Error>>>,
{
    match timeout(timeout_duration, future).await {
        Ok(result) => result,
        Err(_) => Err("Operation timed out".into()),
    }
}

#[tokio::test]
async fn test_basic_account_operations() {
    let ctx = setup_test_environment()
        .await
        .expect("Failed to setup test environment");

    // Create account
    let account_id = ctx
        .account_service
        .create_account("Test User".to_string(), Decimal::new(1000, 0))
        .await
        .expect("Failed to create account");

    // Get account
    let account = ctx
        .account_service
        .get_account(account_id)
        .await
        .expect("Failed to get account")
        .expect("Account not found");

    assert_eq!(account.owner_name, "Test User");
    assert_eq!(account.balance, Decimal::new(1000, 0));

    // Test deposit
    ctx.account_service
        .deposit_money(account_id, Decimal::new(500, 0))
        .await
        .expect("Failed to deposit money");

    // Verify balance after deposit
    let account = ctx
        .account_service
        .get_account(account_id)
        .await
        .expect("Failed to get account")
        .expect("Account not found");
    assert_eq!(account.balance, Decimal::new(1500, 0));

    // Test withdrawal
    ctx.account_service
        .withdraw_money(account_id, Decimal::new(300, 0))
        .await
        .expect("Failed to withdraw money");

    // Verify balance after withdrawal
    let account = ctx
        .account_service
        .get_account(account_id)
        .await
        .expect("Failed to get account")
        .expect("Account not found");
    assert_eq!(account.balance, Decimal::new(1200, 0));

    // Test transaction history
    let transactions = ctx
        .account_service
        .get_account_transactions(account_id)
        .await
        .expect("Failed to get transactions");

    assert_eq!(transactions.len(), 3); // Create + Deposit + Withdraw
    assert_eq!(transactions[0].transaction_type, "MoneyWithdrawn");
    assert_eq!(transactions[0].amount, Decimal::new(300, 0));
    assert_eq!(transactions[1].transaction_type, "MoneyDeposited");
    assert_eq!(transactions[1].amount, Decimal::new(500, 0));
    assert_eq!(transactions[2].transaction_type, "AccountCreated");
    assert_eq!(transactions[2].amount, Decimal::new(1000, 0));

    // Verify metrics
    let metrics = ctx.account_service.get_metrics();
    assert!(
        metrics
            .commands_processed
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    assert!(
        metrics
            .projection_updates
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    assert!(
        metrics
            .cache_hits
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 0
    );
    assert!(
        metrics
            .cache_misses
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 0
    );

    // Test duplicate command handling
    let result = ctx
        .account_service
        .create_account("Test User 2".to_string(), Decimal::new(1000, 0))
        .await;
    assert!(result.is_ok());

    let duplicate_result = ctx
        .account_service
        .create_account("Test User 2".to_string(), Decimal::new(1000, 0))
        .await;
    assert!(duplicate_result.is_err());
    assert!(duplicate_result
        .unwrap_err()
        .to_string()
        .contains("Duplicate"));

    // Test concurrent operations
    let account_id = result.unwrap();
    let mut handles = Vec::new();
    for i in 0..5 {
        let service = ctx.account_service.clone();
        let amount = Decimal::new(100 * (i + 1), 0);
        handles.push(tokio::spawn(async move {
            service.deposit_money(account_id, amount).await
        }));
    }

    for handle in handles {
        assert!(handle.await.unwrap().is_ok());
    }

    // Verify final balance
    let account = ctx
        .account_service
        .get_account(account_id)
        .await
        .expect("Failed to get account")
        .expect("Account not found");
    assert_eq!(account.balance, Decimal::new(1500, 0)); // Initial 1000 + 100 + 200 + 300 + 400 + 500
}

#[tokio::test]
async fn test_cache_behavior() {
    let ctx = setup_test_environment().await.unwrap();
    let account_id = ctx
        .account_service
        .create_account("Cache Test".to_string(), Decimal::from(1000))
        .await
        .unwrap();

    // First read (should miss cache)
    let start = std::time::Instant::now();
    let _ = ctx
        .account_service
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap();
    let first_read_time = start.elapsed();

    // Second read (should hit cache)
    let start = std::time::Instant::now();
    let _ = ctx
        .account_service
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap();
    let second_read_time = start.elapsed();

    // Cache hit should be faster
    assert!(second_read_time < first_read_time);
}

#[tokio::test]
async fn test_error_handling() {
    let ctx = setup_test_environment().await.unwrap();

    // Test non-existent account
    let non_existent_id = Uuid::new_v4();
    let result = ctx.account_service.get_account(non_existent_id).await;
    assert!(matches!(result, Ok(None)));

    // Test withdrawal with insufficient funds
    let account_id = ctx
        .account_service
        .create_account("Error Test".to_string(), Decimal::from(100))
        .await
        .unwrap();

    let result = ctx
        .account_service
        .withdraw_money(account_id, Decimal::from(200))
        .await;

    assert!(matches!(
        result,
        Err(AccountError::InsufficientFunds { .. })
    ));
}

#[tokio::test]
async fn test_performance_metrics() {
    let ctx = setup_test_environment().await.unwrap();
    let account_id = ctx
        .account_service
        .create_account("Metrics Test".to_string(), Decimal::from(1000))
        .await
        .unwrap();

    // Perform multiple operations to generate metrics
    for _ in 0..5 {
        ctx.account_service
            .deposit_money(account_id, Decimal::from(100))
            .await
            .unwrap();

        ctx.account_service
            .withdraw_money(account_id, Decimal::from(50))
            .await
            .unwrap();
    }

    // Get account to check cache metrics
    let _ = ctx
        .account_service
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap();

    // Verify metrics are being recorded
    let metrics = ctx.account_service.get_metrics();
    assert!(
        metrics
            .commands_processed
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    assert!(
        metrics
            .cache_hits
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 0
    );
    assert!(
        metrics
            .cache_misses
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 0
    );
}

#[tokio::test]
async fn test_transaction_history() {
    let ctx = setup_test_environment().await.unwrap();
    let account_id = ctx
        .account_service
        .create_account("History Test".to_string(), Decimal::from(1000))
        .await
        .unwrap();

    // Perform some transactions
    ctx.account_service
        .deposit_money(account_id, Decimal::from(500))
        .await
        .unwrap();

    ctx.account_service
        .withdraw_money(account_id, Decimal::from(200))
        .await
        .unwrap();

    // Get transaction history
    let transactions = ctx
        .account_service
        .get_account_transactions(account_id)
        .await
        .unwrap();

    // Verify transaction history
    assert_eq!(transactions.len(), 3); // Create + Deposit + Withdraw
    assert!(transactions
        .iter()
        .any(|t| t.transaction_type == "MoneyDeposited"));
    assert!(transactions
        .iter()
        .any(|t| t.transaction_type == "MoneyWithdrawn"));
}

#[tokio::test]
async fn test_duplicate_command_handling() {
    let ctx = setup_test_environment().await.unwrap();
    let account_id = ctx
        .account_service
        .create_account("Duplicate Test".to_string(), Decimal::from(1000))
        .await
        .unwrap();

    // Try to process the same command twice
    let result1 = ctx
        .account_service
        .deposit_money(account_id, Decimal::from(100))
        .await;

    let result2 = ctx
        .account_service
        .deposit_money(account_id, Decimal::from(100))
        .await;

    assert!(result1.is_ok());
    assert!(matches!(result2, Err(AccountError::InfrastructureError(_))));
}

#[tokio::test]
async fn test_high_throughput_performance() {
    let ctx = setup_test_environment()
        .await
        .expect("Failed to setup test environment");

    // Create test accounts
    let num_accounts = 100;
    let mut account_ids = Vec::with_capacity(num_accounts);

    for i in 0..num_accounts {
        let account_id = ctx
            .account_service
            .create_account(format!("Load Test User {}", i), Decimal::new(10000, 0))
            .await
            .expect("Failed to create account");
        account_ids.push(account_id);
    }

    // Setup metrics collection
    let start_time = std::time::Instant::now();
    let mut successful_ops = 0;
    let mut failed_ops = 0;
    let target_eps = 10_000; // Target events per second
    let test_duration = std::time::Duration::from_secs(30); // 30 second test
    let end_time = start_time + test_duration;

    // Create a channel for operation results
    let (tx, mut rx) = mpsc::channel(1000);

    // Spawn worker tasks
    let num_workers = 16; // Adjust based on your CPU cores
    let mut handles = Vec::new();

    for worker_id in 0..num_workers {
        let service = ctx.account_service.clone();
        let account_ids = account_ids.clone();
        let tx = tx.clone();

        handles.push(tokio::spawn(async move {
            let mut rng = StdRng::from_entropy();
            while std::time::Instant::now() < end_time {
                let account_idx = rng.gen_range(0..account_ids.len());
                let account_id = account_ids[account_idx];
                let amount = Decimal::new(rng.gen_range(1..1000), 0);

                let result = if rng.gen_bool(0.5) {
                    service.deposit_money(account_id, amount).await
                } else {
                    service.withdraw_money(account_id, amount).await
                };

                let _ = tx.send(result).await;
            }
        }));
    }

    // Collect results
    while let Some(result) = rx.recv().await {
        match result {
            Ok(_) => successful_ops += 1,
            Err(_) => failed_ops += 1,
        }
    }

    // Wait for all workers to complete
    for handle in handles {
        handle.await.expect("Worker task failed");
    }

    // Calculate metrics
    let duration = start_time.elapsed();
    let total_ops = successful_ops + failed_ops;
    let eps = total_ops as f64 / duration.as_secs_f64();
    let success_rate = (successful_ops as f64 / total_ops as f64) * 100.0;

    // Print performance metrics
    println!("\nPerformance Test Results:");
    println!("Duration: {:.2}s", duration.as_secs_f64());
    println!("Total Operations: {}", total_ops);
    println!("Successful Operations: {}", successful_ops);
    println!("Failed Operations: {}", failed_ops);
    println!("Events Per Second: {:.2}", eps);
    println!("Success Rate: {:.2}%", success_rate);
    println!(
        "Average Latency: {:.2}ms",
        (duration.as_millis() as f64 / total_ops as f64)
    );

    // Verify system metrics
    let metrics = ctx.account_service.get_metrics();
    println!("\nSystem Metrics:");
    println!(
        "Commands Processed: {}",
        metrics
            .commands_processed
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    println!(
        "Commands Failed: {}",
        metrics
            .commands_failed
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    println!(
        "Projection Updates: {}",
        metrics
            .projection_updates
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    println!(
        "Cache Hits: {}",
        metrics
            .cache_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    println!(
        "Cache Misses: {}",
        metrics
            .cache_misses
            .load(std::sync::atomic::Ordering::Relaxed)
    );

    // Assertions
    assert!(
        eps >= target_eps as f64 * 0.8,
        "Failed to maintain target EPS. Achieved: {:.2}, Target: {}",
        eps,
        target_eps
    );
    assert!(
        success_rate >= 99.0,
        "Success rate too low: {:.2}%",
        success_rate
    );
    assert!(
        failed_ops < total_ops / 100,
        "Too many failed operations: {} out of {}",
        failed_ops,
        total_ops
    );
}
