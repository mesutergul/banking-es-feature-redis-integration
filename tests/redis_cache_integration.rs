use ::banking_es::domain::{Account, AccountEvent};
use ::banking_es::infrastructure::cache_service::CacheService;
use ::banking_es::infrastructure::middleware::RequestMiddleware;
use ::banking_es::{
    AccountError, AccountRepository, AccountRepositoryTrait, AccountService, EventStore,
    EventStoreConfig, ProjectionStore,
};
use anyhow::Result;
use banking_es::infrastructure::cache_service::CacheServiceTrait;
use banking_es::infrastructure::event_store::EventStoreTrait;
use banking_es::infrastructure::projections::ProjectionStoreTrait;
use dotenv;
use redis;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

// Helper structure to hold common test dependencies
struct TestContext {
    account_service: Arc<AccountService>,
    account_repository: Arc<AccountRepository>,
    db_pool: PgPool,
}

async fn setup_test_environment() -> Result<TestContext, Box<dyn std::error::Error>> {
    dotenv::dotenv().ok(); // Load .env file if present

    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");

    // Setup Database
    let db_pool = PgPool::connect(&database_url).await?;

    // Setup EventStore and ProjectionStore
    let event_store = EventStore::new_with_config(EventStoreConfig::default()).await?;
    let projection_store = ProjectionStore::new(db_pool.clone());

    // Setup Repository and Service
    let event_store = Arc::new(event_store) as Arc<dyn EventStoreTrait + 'static>;
    let account_repository: Arc<AccountRepository> = Arc::new(AccountRepository::new(event_store));
    let account_repository_clone = account_repository.clone();

    let service = Arc::new(AccountService::new(
        account_repository,
        Arc::new(projection_store) as Arc<dyn ProjectionStoreTrait + 'static>,
        Arc::new(CacheService::default()) as Arc<dyn CacheServiceTrait + 'static>,
        Arc::new(RequestMiddleware::default()),
        100,
    ));

    Ok(TestContext {
        account_service: service.clone(),
        account_repository: account_repository_clone,
        db_pool,
    })
}

// Helper to get current account version from event store DB
async fn get_account_current_version(account_id: Uuid, pool: &PgPool) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT MAX(sequence_number) FROM events WHERE aggregate_id = $1")
            .bind(account_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map_or(0, |(max_seq,)| max_seq)) // Version is max sequence number, or 0 if no events
}

#[tokio::test]
async fn test_account_data_caching_and_invalidation() -> Result<(), Box<dyn std::error::Error>> {
    let context = setup_test_environment().await?;

    let owner_name = format!("TestUser_{}", Uuid::new_v4());
    let initial_balance = Decimal::new(100, 2);

    // 1. Create an account using AccountService
    let account_id = context
        .account_service
        .create_account(owner_name.clone(), initial_balance)
        .await?;

    // 2. Fetch the account using AccountRepository::get_by_id (first fetch)
    let account_v1_opt = context.account_repository.get_by_id(account_id).await?;
    assert!(account_v1_opt.is_some(), "Account not found after creation");
    let account_v1 = account_v1_opt.unwrap();
    assert_eq!(account_v1.owner_name, owner_name);
    assert_eq!(account_v1.balance, initial_balance);

    // 3. Fetch the same account again
    let account_v2_opt = context.account_repository.get_by_id(account_id).await?;
    assert!(account_v2_opt.is_some());
    let account_v2 = account_v2_opt.unwrap();
    assert_eq!(account_v2.id, account_v1.id);
    assert_eq!(account_v2.balance, account_v1.balance);

    // 4. Perform an operation that updates the account (e.g., deposit)
    let deposit_amount = Decimal::new(50, 2);
    context
        .account_service
        .deposit_money(account_id, deposit_amount)
        .await?;

    // 5. Fetch the account again using AccountRepository::get_by_id
    let account_v3_opt = context.account_repository.get_by_id(account_id).await?;
    assert!(account_v3_opt.is_some(), "Account not found after deposit");
    let account_v3 = account_v3_opt.unwrap();

    assert_eq!(account_v3.id, account_id);
    assert_eq!(
        account_v3.balance,
        initial_balance + deposit_amount,
        "Balance not updated after deposit and re-fetch"
    );

    Ok(())
}

#[tokio::test]
async fn test_command_deduplication_integration() -> Result<(), Box<dyn std::error::Error>> {
    let context = setup_test_environment().await?;

    let owner_name_base = format!("DedupeTest_{}", Uuid::new_v4());
    let initial_balance = Decimal::new(200, 0);

    // First, create an account to deposit into.
    let account_id = context
        .account_service
        .create_account(format!("{}_Account", owner_name_base), initial_balance)
        .await?;

    let deposit_amount = Decimal::new(10, 0);

    // 1. First call to deposit_money (should succeed)
    let res1 = context
        .account_service
        .deposit_money(account_id, deposit_amount)
        .await;
    assert!(res1.is_ok(), "First deposit call failed: {:?}", res1.err());

    // 2. Second call (immediate duplicate, should fail)
    let res2 = context
        .account_service
        .deposit_money(account_id, deposit_amount)
        .await;
    assert!(
        res2.is_err(),
        "Second deposit call should have failed as duplicate"
    );
    if let Err(AccountError::InfrastructureError(msg)) = res2 {
        assert!(
            msg.contains("Duplicate deposit command"),
            "Error message mismatch for duplicate: {}",
            msg
        );
    } else {
        panic!("Expected InfrastructureError for duplicate, got {:?}", res2);
    }

    // 3. Wait for TTL to expire
    sleep(Duration::from_secs(61)).await;

    // 4. Third call (after TTL, should succeed)
    let res3 = context
        .account_service
        .deposit_money(account_id, deposit_amount)
        .await;
    assert!(
        res3.is_ok(),
        "Third deposit call failed after TTL: {:?}",
        res3.err()
    );

    Ok(())
}
