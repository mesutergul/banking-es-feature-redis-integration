use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::domain::{Account, AccountCommand, AccountError, AccountEvent};
use crate::infrastructure::cache_service::{CacheService, CacheServiceTrait};
use crate::infrastructure::middleware::RequestMiddleware;
use crate::infrastructure::projections::ProjectionStoreTrait;
use crate::infrastructure::projections::{AccountProjection, TransactionProjection};
use crate::infrastructure::repository::AccountRepositoryTrait;
use crate::infrastructure::{AccountRepository, EventStore, EventStoreConfig, ProjectionStore};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Service metrics
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    pub commands_processed: std::sync::atomic::AtomicU64,
    pub commands_failed: std::sync::atomic::AtomicU64,
    pub projection_updates: std::sync::atomic::AtomicU64,
    pub projection_errors: std::sync::atomic::AtomicU64,
    pub cache_hits: std::sync::atomic::AtomicU64,
    pub cache_misses: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct AccountService {
    repository: Arc<dyn AccountRepositoryTrait + 'static>,
    projections: Arc<dyn ProjectionStoreTrait + 'static>,
    cache_service: Arc<dyn CacheServiceTrait + 'static>,
    metrics: Arc<ServiceMetrics>,
    command_cache: Arc<RwLock<std::collections::HashMap<Uuid, Instant>>>,
    pub middleware: Arc<RequestMiddleware>,
    pub semaphore: Arc<Semaphore>,
    pub max_requests_per_second: usize,
}

impl AccountService {
    /// Creates a new `AccountService`.
    ///
    /// # Arguments
    ///
    /// * `repository`: The account repository for data access.
    /// * `projections`: The projection store for querying denormalized views.
    /// * `cache_service`: The cache service for caching account data.
    /// * `middleware`: The middleware for handling request-specific logic.
    /// * `max_requests_per_second`: The maximum number of requests per second allowed.
    pub fn new(
        repository: Arc<dyn AccountRepositoryTrait + 'static>,
        projections: Arc<dyn ProjectionStoreTrait + 'static>,
        cache_service: Arc<dyn CacheServiceTrait + 'static>,
        middleware: Arc<RequestMiddleware>,
        max_requests_per_second: usize,
    ) -> Self {
        let service = Self {
            repository,
            projections,
            cache_service,
            metrics: Arc::new(ServiceMetrics::default()),
            command_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            middleware,
            semaphore: Arc::new(Semaphore::new(max_requests_per_second)),
            max_requests_per_second,
        };

        // Start metrics reporter
        let metrics = service.metrics.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let commands_processed = metrics
                    .commands_processed
                    .load(std::sync::atomic::Ordering::Relaxed);
                let commands_failed = metrics
                    .commands_failed
                    .load(std::sync::atomic::Ordering::Relaxed);
                let total_commands = commands_processed + commands_failed;
                let success_rate = if total_commands > 0 {
                    (commands_processed as f64 / total_commands as f64) * 100.0
                } else {
                    100.0
                };
                info!(
                    "Service metrics: processed={}, failed={}, success_rate={:.2}%",
                    commands_processed, commands_failed, success_rate
                );
            }
        });

        service
    }

    pub async fn create_account(
        &self,
        owner_name: String,
        initial_balance: Decimal,
    ) -> Result<Uuid, AccountError> {
        let start_time = Instant::now();
        let account_id = Uuid::new_v4();

        // Check for duplicate command
        if self.is_duplicate_command(account_id).await {
            return Err(AccountError::InfrastructureError(
                "Duplicate create account command".to_string(),
            ));
        }

        let command = AccountCommand::CreateAccount {
            account_id,
            owner_name: owner_name.clone(),
            initial_balance,
        };

        let account = Account::default();
        let events = account.handle_command(&command)?;

        // Save events with retry logic
        self.repository
            .save(&account, events.clone())
            .await
            .map_err(|e| {
                self.metrics
                    .commands_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                AccountError::InfrastructureError(e.to_string())
            })?;

        // Update projections
        let projection = AccountProjection {
            id: account_id,
            owner_name,
            balance: initial_balance,
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        if let Err(e) = self
            .projections
            .upsert_accounts_batch(vec![projection])
            .await
        {
            self.metrics
                .projection_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!(
                "Failed to update projection for account {}: {}",
                account_id, e
            );
        } else {
            self.metrics
                .projection_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.metrics
            .commands_processed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(account_id)
    }

    pub async fn deposit_money(
        &self,
        account_id: Uuid,
        amount: Decimal,
    ) -> Result<(), AccountError> {
        let start_time = Instant::now();

        // Check for duplicate command
        if self.is_duplicate_command(account_id).await {
            return Err(AccountError::InfrastructureError(
                "Duplicate deposit command".to_string(),
            ));
        }

        let mut account = self
            .repository
            .get_by_id(account_id)
            .await?
            .ok_or(AccountError::NotFound)?;

        let command = AccountCommand::DepositMoney { account_id, amount };
        let events = account.handle_command(&command)?;

        // Apply events to account
        for event in &events {
            account.apply_event(event);
        }

        // Save events with retry logic
        self.repository
            .save(&account, events.clone())
            .await
            .map_err(|e| {
                self.metrics
                    .commands_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                AccountError::InfrastructureError(e.to_string())
            })?;

        // Update projections
        if let Err(e) = self.update_projections_from_events(&events).await {
            self.metrics
                .projection_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!("Failed to update projections for deposit: {}", e);
        } else {
            self.metrics
                .projection_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.metrics
            .commands_processed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    pub async fn withdraw_money(
        &self,
        account_id: Uuid,
        amount: Decimal,
    ) -> Result<(), AccountError> {
        let start_time = Instant::now();

        // Check for duplicate command
        if self.is_duplicate_command(account_id).await {
            return Err(AccountError::InfrastructureError(
                "Duplicate withdraw command".to_string(),
            ));
        }

        let mut account = self
            .repository
            .get_by_id(account_id)
            .await?
            .ok_or(AccountError::NotFound)?;

        let command = AccountCommand::WithdrawMoney { account_id, amount };
        let events = account.handle_command(&command)?;

        // Apply events to account
        for event in &events {
            account.apply_event(event);
        }

        // Save events with retry logic
        self.repository
            .save(&account, events.clone())
            .await
            .map_err(|e| {
                self.metrics
                    .commands_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                AccountError::InfrastructureError(e.to_string())
            })?;

        // Update projections
        if let Err(e) = self.update_projections_from_events(&events).await {
            self.metrics
                .projection_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!("Failed to update projections for withdrawal: {}", e);
        } else {
            self.metrics
                .projection_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.metrics
            .commands_processed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    pub async fn get_account(
        &self,
        account_id: Uuid,
    ) -> Result<Option<AccountProjection>, AccountError> {
        // Try cache first
        if let Some(account) = self
            .cache_service
            .get_account(account_id)
            .await
            .map_err(|e| AccountError::InfrastructureError(e.to_string()))?
        {
            self.metrics
                .cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(Some(AccountProjection {
                id: account.id,
                owner_name: account.owner_name,
                balance: account.balance,
                is_active: account.is_active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }));
        }
        self.metrics
            .cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Fall back to projections
        self.projections
            .get_account(account_id)
            .await
            .map_err(|e| AccountError::InfrastructureError(e.to_string()))
    }

    pub async fn get_all_accounts(&self) -> Result<Vec<AccountProjection>, AccountError> {
        self.projections
            .get_all_accounts()
            .await
            .map_err(|e| AccountError::InfrastructureError(e.to_string()))
    }

    pub async fn get_account_transactions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TransactionProjection>, AccountError> {
        // Try cache first
        if let Some(events) = self
            .cache_service
            .get_account_events(account_id)
            .await
            .map_err(|e| AccountError::InfrastructureError(e.to_string()))?
        {
            self.metrics
                .cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(events.into_iter().map(|e| e.into()).collect());
        }
        self.metrics
            .cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Fall back to projections
        self.projections
            .get_account_transactions(account_id)
            .await
            .map_err(|e| AccountError::InfrastructureError(e.to_string()))
    }

    async fn update_projections_from_events(
        &self,
        events: &[crate::domain::AccountEvent],
    ) -> Result<(), AccountError> {
        let start_time = Instant::now();

        for event in events {
            let transaction = TransactionProjection {
                id: Uuid::new_v4(),
                account_id: event.aggregate_id(),
                amount: match event {
                    AccountEvent::MoneyDeposited { amount, .. } => amount.clone(),
                    AccountEvent::MoneyWithdrawn { amount, .. } => amount.clone(),
                    _ => Decimal::ZERO,
                },
                transaction_type: event.event_type().to_string(),
                timestamp: Utc::now(),
            };

            match self
                .projections
                .insert_transactions_batch(vec![transaction])
                .await
            {
                Ok(_) => {
                    self.metrics
                        .projection_updates
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    // Update cache with new account state
                    if let Some(account) = self
                        .projections
                        .get_account(event.aggregate_id())
                        .await
                        .map_err(|e| AccountError::InfrastructureError(e.to_string()))?
                    {
                        let account = Account {
                            id: account.id,
                            owner_name: account.owner_name,
                            balance: account.balance,
                            is_active: account.is_active,
                            version: 0, // Default version
                        };
                        self.cache_service
                            .set_account(&account, Some(Duration::from_secs(3600)))
                            .await
                            .map_err(|e| AccountError::InfrastructureError(e.to_string()))?;
                    }
                }
                Err(e) => {
                    self.metrics
                        .projection_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    error!("Failed to update projection: {}", e);
                    return Err(AccountError::InfrastructureError(e.to_string()));
                }
            }
        }

        let duration = start_time.elapsed();
        debug!("Projection update took {:?}", duration);
        Ok(())
    }

    pub async fn is_duplicate_command(&self, command_id: Uuid) -> bool {
        let mut cache = self.command_cache.write().await;
        if cache.contains_key(&command_id) {
            true
        } else {
            cache.insert(command_id, Instant::now());
            false
        }
    }

    pub fn get_metrics(&self) -> &ServiceMetrics {
        &self.metrics
    }
}

impl From<AccountEvent> for TransactionProjection {
    fn from(event: AccountEvent) -> Self {
        TransactionProjection {
            id: Uuid::new_v4(),
            account_id: event.aggregate_id(),
            amount: match event {
                AccountEvent::MoneyDeposited { amount, .. } => amount.clone(),
                AccountEvent::MoneyWithdrawn { amount, .. } => amount.clone(),
                _ => Decimal::ZERO,
            },
            transaction_type: event.event_type().to_string(),
            timestamp: Utc::now(),
        }
    }
}

// Add Default implementation for AccountService
impl Default for AccountService {
    fn default() -> Self {
        let repository = Arc::new(AccountRepository::default());
        let projection_store =
            Arc::new(ProjectionStore::default()) as Arc<dyn ProjectionStoreTrait + 'static>;
        let cache_service =
            Arc::new(CacheService::default()) as Arc<dyn CacheServiceTrait + 'static>;
        let middleware = Arc::new(RequestMiddleware::default());
        AccountService::new(repository, projection_store, cache_service, middleware, 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AccountError;
    use crate::infrastructure::repository::AccountRepositoryTrait;
    use crate::infrastructure::{AccountRepository, EventStore, EventStoreConfig, ProjectionStore};
    use async_trait::async_trait;
    use uuid::Uuid;

    // Manual mock for AccountRepositoryTrait
    #[derive(Clone)]
    struct ManualMockAccountRepository {
        pub save_batched_result: Arc<Result<(), AccountError>>,
        pub get_by_id_result: Arc<Result<Option<Account>, AccountError>>,
    }

    #[async_trait]
    impl AccountRepositoryTrait for ManualMockAccountRepository {
        async fn save(&self, _account: &Account, _events: Vec<AccountEvent>) -> Result<()> {
            match &*self.save_batched_result {
                Ok(_) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }

        async fn get_by_id(&self, _id: Uuid) -> Result<Option<Account>, AccountError> {
            match &*self.get_by_id_result {
                Ok(account) => Ok(account.clone()),
                Err(e) => Err(e.clone()),
            }
        }

        async fn save_batched(
            &self,
            _account_id: Uuid,
            _expected_version: i64,
            _events: Vec<AccountEvent>,
        ) -> Result<()> {
            match &*self.save_batched_result {
                Ok(_) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }

        async fn save_immediate(
            &self,
            _account: &Account,
            _events: Vec<AccountEvent>,
        ) -> Result<()> {
            match &*self.save_batched_result {
                Ok(_) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }

        async fn flush_all(&self) -> Result<()> {
            Ok(())
        }

        fn start_batch_flush_task(&self) {}

        async fn create_account(
            &self,
            _owner_name: String,
            _initial_balance: Decimal,
        ) -> Result<Account> {
            Ok(Account::default())
        }

        async fn get_account(&self, _account_id: Uuid) -> Result<Option<Account>> {
            Ok(None)
        }

        async fn deposit_money(&self, _account_id: Uuid, _amount: Decimal) -> Result<Account> {
            Ok(Account::default())
        }

        async fn withdraw_money(&self, _account_id: Uuid, _amount: Decimal) -> Result<Account> {
            Ok(Account::default())
        }
    }

    fn account_service_with_mock_repo(
        mock_repo: Arc<dyn AccountRepositoryTrait + 'static>,
    ) -> AccountService {
        let projection_store =
            Arc::new(ProjectionStore::default()) as Arc<dyn ProjectionStoreTrait + 'static>;
        let cache_service =
            Arc::new(CacheService::default()) as Arc<dyn CacheServiceTrait + 'static>;
        AccountService::new(
            mock_repo,
            projection_store,
            cache_service,
            Arc::new(RequestMiddleware::default()),
            10,
        )
    }

    #[tokio::test]
    async fn test_is_duplicate_command_new_command() {
        let account_id = Uuid::new_v4();
        let mock_repo = ManualMockAccountRepository {
            save_batched_result: Arc::new(Ok(())),
            get_by_id_result: Arc::new(Ok(None)),
        };

        let service = account_service_with_mock_repo(Arc::new(mock_repo));

        let result = service.is_duplicate_command(account_id).await;
        assert!(!result, "New command should not be detected as duplicate");
    }

    #[tokio::test]
    async fn test_is_duplicate_command_duplicate_detected() {
        let account_id_for_deposit = Uuid::new_v4();

        let mock_repo = ManualMockAccountRepository {
            save_batched_result: Arc::new(Ok(())),
            get_by_id_result: Arc::new(Ok(None)),
        };

        let service = account_service_with_mock_repo(Arc::new(mock_repo));

        // First call should not be a duplicate
        let first_result = service.is_duplicate_command(account_id_for_deposit).await;
        assert!(
            !first_result,
            "First call should not be detected as duplicate"
        );

        // Second call should be a duplicate
        let second_result = service.is_duplicate_command(account_id_for_deposit).await;
        assert!(second_result, "Second call should be detected as duplicate");
    }
}
