# Banking ES (Event Sourcing) Project

A sample banking application demonstrating Event Sourcing principles with Rust.

## Overview

This project implements basic banking functionalities such as account creation, deposits, and withdrawals using an event-sourced approach. It showcases how to build a system where all changes to application state are stored as a sequence of events.

## Architecture & Services

The system is composed of several key parts:

- **Domain Model:** Contains the core business logic and event definitions.
- **Application Layer:** Orchestrates operations, handles commands, and uses services.
- **Infrastructure Layer:** Manages data persistence (event store, projections) and external service integrations.
- **Web API:** Exposes functionalities through an HTTP API (Axum).

### Key Dependencies & Services:

- **PostgreSQL:** Used as the primary data store for events and projections.
- **Redis:** Integrated for performance enhancement and specific functionalities:
  - **Account Data Caching:** Acts as a read-through cache (L1 cache) for frequently accessed account data to reduce database load.
  - **Event Batching:** Events are temporarily queued in Redis lists before being flushed to the PostgreSQL event store by a background worker. This can help in smoothing out write loads.
  - **Command De-duplication:** Used to prevent processing of duplicate commands within a certain time window, ensuring idempotency for critical operations.

## Running the Project

### Prerequisites

- Docker and Docker Compose
- Rust toolchain (if building/running locally without Docker for the application)

### Using Docker Compose (Recommended for local development)

1.  **Environment Variables:**

    - Ensure you have a `.env` file based on `.env.example` (if provided) or create one.
    - The application requires `DATABASE_URL` for PostgreSQL and `REDIS_URL` for Redis.
    - Default for Docker Compose:
      - `DATABASE_URL=postgresql://postgres:password@postgres:5432/banking_es`
      - `REDIS_URL=redis://redis:6379` (The `redis` hostname refers to the Redis service in `docker-compose.yml`)

2.  **Start Services:**

    ```bash
    docker-compose up --build
    ```

    This command will build the application container and start all necessary services, including PostgreSQL and Redis, as defined in `docker-compose.yml`.

3.  The application will typically be available at `http://localhost:3000`.

### Local Development (without Docker for the application)

If you run the application directly using `cargo run`:

1.  Ensure PostgreSQL and Redis instances are running and accessible.
2.  Set the `DATABASE_URL` and `REDIS_URL` environment variables to point to your instances. For example:
    ```bash
    export DATABASE_URL="postgresql://user:pass@host:port/dbname"
    export REDIS_URL="redis://your_redis_host:6379"
    cargo run
    ```

## API Endpoints

(Details about API endpoints would go here - e.g., create account, deposit, withdraw, get account)

## Rate Limiting and Request Validation

This system implements a robust rate limiting and request validation middleware to ensure secure and controlled access to the banking API endpoints.

### Rate Limiting

The rate limiting system provides per-client request throttling with the following features:

- **Per-Client Rate Limits**: Each client is limited to a configurable number of requests per minute
- **Burst Control**: Configurable burst size to handle sudden spikes in traffic
- **Sliding Window**: Rate limits are enforced using a sliding window approach
- **Automatic Cleanup**: Expired rate limits are automatically cleaned up

#### Configuration

Rate limits can be configured through the `RateLimitConfig` struct:

```rust
pub struct RateLimitConfig {
    pub requests_per_minute: u32,  // Default: 60
    pub burst_size: u32,          // Default: 10
    pub window_size: Duration,     // Default: 60 seconds
    pub max_clients: usize,        // Default: 10000
}
```

### Request Validation

The request validation system provides a flexible and extensible way to validate incoming requests:

- **Pluggable Validators**: Custom validation rules can be implemented for different request types
- **Detailed Validation Results**: Includes both errors and warnings
- **Type-Specific Validation**: Different validation rules for different endpoints
- **Extensible Design**: Easy to add new validation rules

#### Available Validators

1. **AccountCreationValidator**

   - Validates account creation requests
   - Checks for required fields: `owner_name`, `initial_balance`
   - Ensures `initial_balance` is non-negative
   - Validates `owner_name` is non-empty

2. **TransactionValidator**
   - Validates transaction requests (deposits and withdrawals)
   - Checks for required fields: `account_id`, `amount`
   - Validates `account_id` is a valid UUID
   - Ensures `amount` is greater than zero

### Usage

#### Client Requirements

Clients must include the following headers in their requests:

```http
x-client-id: <unique-client-identifier>
x-user-id: <optional-user-identifier>
```

#### Example Requests

1. **Create Account**

```http
POST /accounts
x-client-id: client123
x-user-id: user456
Content-Type: application/json

{
    "owner_name": "John Doe",
    "initial_balance": 1000.00
}
```

2. **Deposit Money**

```http
POST /accounts/{account_id}/deposit
x-client-id: client123
x-user-id: user456
Content-Type: application/json

{
    "amount": 500.00,
    "expected_version": 1
}
```

3. **Withdraw Money**

```http
POST /accounts/{account_id}/withdraw
x-client-id: client123
x-user-id: user456
Content-Type: application/json

{
    "amount": 200.00,
    "expected_version": 2
}
```

### Response Codes

The system returns the following HTTP status codes:

- `200 OK`: Request processed successfully
- `201 Created`: Account created successfully
- `400 Bad Request`: Invalid request payload
- `429 Too Many Requests`: Rate limit exceeded
- `500 Internal Server Error`: Server-side error

### Error Responses

Rate limit and validation errors return detailed error messages:

```json
{
  "error": "Rate limit exceeded"
}
```

or

```json
{
  "error": "owner_name is required, initial_balance must be non-negative"
}
```

### Implementation Details

#### Rate Limiting Implementation

The rate limiter uses a combination of:

- DashMap for thread-safe storage of rate limit information
- Semaphores for burst control
- Sliding window algorithm for accurate rate limiting

#### Request Validation Implementation

The validation system uses:

- Trait-based design for extensibility
- Async validation support
- Detailed validation results with errors and warnings
- Type-safe validation rules

### Adding Custom Validators

To add a custom validator:

1. Implement the `RequestValidationRule` trait:

```rust
#[async_trait::async_trait]
impl RequestValidationRule for CustomValidator {
    async fn validate(&self, context: &RequestContext) -> ValidationResult {
        // Implement validation logic
    }
}
```

2. Register the validator:

```rust
middleware.register_validator(
    "custom_request_type".to_string(),
    Box::new(CustomValidator),
);
```

### Monitoring and Metrics

The system includes built-in monitoring capabilities:

- Rate limit violations are logged
- Validation failures are logged
- Performance metrics are tracked
- Client usage patterns can be analyzed

### Best Practices

1. **Client ID Management**

   - Use consistent client IDs
   - Implement proper client ID rotation
   - Monitor client usage patterns

2. **Rate Limit Configuration**

   - Adjust limits based on client needs
   - Monitor rate limit effectiveness
   - Implement gradual limit increases

3. **Validation Rules**
   - Keep validation rules up to date
   - Add new rules as needed
   - Monitor validation failure patterns

### Security Considerations

1. **Client Identification**

   - Use secure client ID generation
   - Implement client ID validation
   - Monitor for client ID spoofing

2. **Rate Limit Security**

   - Implement IP-based rate limiting
   - Monitor for rate limit bypass attempts
   - Implement rate limit alerts

3. **Request Validation**
   - Validate all input data
   - Implement strict type checking
   - Monitor for validation bypass attempts

### Performance Considerations

1. **Rate Limiting**

   - Use efficient data structures
   - Implement proper cleanup
   - Monitor memory usage

2. **Request Validation**
   - Optimize validation rules
   - Use async validation where possible
   - Monitor validation performance

### Future Improvements

1. **Enhanced Rate Limiting**

   - IP-based rate limiting
   - Geographic rate limiting
   - Dynamic rate limit adjustment

2. **Advanced Validation**

   - Schema-based validation
   - Custom validation rules
   - Validation rule versioning

3. **Monitoring and Analytics**
   - Real-time rate limit monitoring
   - Validation failure analytics
   - Client usage analytics

## Caching System

The system implements a multi-level caching strategy to optimize performance and reduce database load.

### Cache Architecture

#### 1. In-Memory Cache (L1)

- **Purpose**: Fast access to frequently used data
- **Implementation**: DashMap for thread-safe concurrent access
- **Features**:
  - Sharded storage for better concurrency
  - LRU eviction policy
  - Configurable TTL
  - Automatic cleanup

#### 2. Redis Cache (L2)

- **Purpose**: Distributed caching for multi-instance deployments
- **Implementation**: Redis with connection pooling
- **Features**:
  - Circuit breaker for fault tolerance
  - Load shedding for overload protection
  - Automatic retry mechanism
  - Configurable TTL

### Cache Configuration

```rust
pub struct CacheConfig {
    pub max_size: usize,           // Maximum number of items in cache
    pub ttl: Duration,             // Time-to-live for cache entries
    pub shard_count: usize,        // Number of shards for in-memory cache
    pub eviction_policy: EvictionPolicy,
    pub redis_config: RedisConfig,
}

pub enum EvictionPolicy {
    LRU,    // Least Recently Used
    LFU,    // Least Frequently Used
    TTL,    // Time To Live
}
```

### Cache Operations

1. **Account Caching**

```rust
// Cache an account
cache_service.set_account(&account, Some(Duration::from_secs(3600))).await?;

// Retrieve an account
let account = cache_service.get_account(account_id).await?;
```

2. **Event Caching**

```rust
// Cache events
cache_service.set_account_events(account_id, &events, None).await?;

// Retrieve events
let events = cache_service.get_account_events(account_id).await?;
```

### Cache Warming

The system includes automatic cache warming capabilities:

- Preloads frequently accessed data
- Background warming process
- Configurable warming strategies
- Progress tracking

### Cache Metrics

The system tracks various cache metrics:

- Hit/miss ratios
- Eviction counts
- Memory usage
- Response times
- Error rates

## Event Processing System

The event processing system is designed for high throughput and reliability.

### Event Store

#### Features

- **Batch Processing**: Efficient handling of multiple events
- **Snapshot Caching**: Optimized event replay
- **Version Control**: Ensures event consistency
- **Event Validation**: Pluggable validation rules
- **Event Handlers**: Extensible event processing

#### Configuration

```rust
pub struct EventStoreConfig {
    pub batch_size: usize,
    pub batch_timeout_ms: u64,
    pub max_batch_queue_size: usize,
    pub batch_processor_count: usize,
    pub snapshot_threshold: usize,
    pub snapshot_interval_secs: u64,
    pub snapshot_cache_ttl_secs: u64,
    pub max_snapshots_per_run: usize,
}
```

### Event Processing Pipeline

1. **Event Validation**

   - Schema validation
   - Business rule validation
   - Custom validation rules

2. **Event Storage**

   - Batch processing
   - Transaction support
   - Version control
   - Snapshot management

3. **Event Handling**
   - Async processing
   - Error handling
   - Retry mechanism
   - Dead letter queue

### Event Types

```rust
pub enum AccountEvent {
    AccountCreated {
        owner_name: String,
        initial_balance: Decimal,
    },
    MoneyDeposited {
        amount: Decimal,
    },
    MoneyWithdrawn {
        amount: Decimal,
    },
    AccountClosed {
        reason: String,
    },
}
```

### Event Processing Flow

1. **Event Creation**

```rust
let event = AccountEvent::MoneyDeposited { amount: 100.00 };
```

2. **Event Validation**

```rust
let validation = event_store.validate_event(&event).await?;
```

3. **Event Storage**

```rust
event_store.save_events(account_id, vec![event], expected_version).await?;
```

4. **Event Handling**

```rust
event_store.handle_event(&event).await?;
```

### Event Recovery

The system includes comprehensive event recovery mechanisms:

1. **Full Replay**

   - Replay all events from the beginning
   - Verify event consistency
   - Update projections

2. **Incremental Replay**

   - Replay events from a specific version
   - Handle version conflicts
   - Update affected projections

3. **Selective Replay**
   - Replay specific event types
   - Handle dependencies
   - Update related projections

### Event Monitoring

The system provides detailed monitoring capabilities:

1. **Performance Metrics**

   - Processing latency
   - Batch sizes
   - Queue lengths
   - Error rates

2. **Health Checks**

   - Event store status
   - Processing pipeline health
   - Recovery status
   - Cache status

3. **Alerting**
   - Processing delays
   - Error thresholds
   - Recovery failures
   - Cache issues

### Best Practices

1. **Event Design**

   - Keep events small and focused
   - Use meaningful event names
   - Include necessary metadata
   - Version event schemas

2. **Event Processing**

   - Implement idempotency
   - Handle failures gracefully
   - Monitor processing latency
   - Implement proper error handling

3. **Caching Strategy**

   - Cache frequently accessed data
   - Implement proper invalidation
   - Monitor cache performance
   - Use appropriate TTLs

4. **Recovery Planning**
   - Regular backup testing
   - Document recovery procedures
   - Monitor recovery performance
   - Implement automated recovery

### Performance Optimization

1. **Event Processing**

   - Batch processing
   - Parallel processing
   - Efficient storage
   - Optimized queries

2. **Caching**

   - Multi-level caching
   - Efficient eviction
   - Proper sharding
   - Memory management

3. **Monitoring**
   - Real-time metrics
   - Performance profiling
   - Resource utilization
   - Bottleneck identification

### Security Considerations

1. **Event Security**

   - Event validation
   - Access control
   - Audit logging
   - Data encryption

2. **Cache Security**
   - Cache isolation
   - Access control
   - Data encryption
   - Secure cleanup

### Future Improvements

1. **Event Processing**

   - Stream processing
   - Event sourcing patterns
   - Advanced recovery
   - Enhanced monitoring

2. **Caching**

   - Predictive caching
   - Dynamic TTLs
   - Advanced eviction
   - Cache warming

3. **Monitoring**
   - AI-powered analytics
   - Predictive alerts
   - Advanced metrics
   - Real-time dashboards

## Sharding and Distributed Locking

### Sharding System

The system implements a robust sharding mechanism to distribute accounts across multiple service instances, enabling horizontal scaling and improved performance.

#### Key Features

1. **Shard Management**

   - Automatic shard distribution across service instances
   - Configurable number of shards (default: 16)
   - Consistent hashing for account-to-shard mapping
   - Automatic shard rebalancing when load is imbalanced

2. **Shard Configuration**

   ```rust
   pub struct ShardConfig {
       pub shard_count: usize,          // Number of shards
       pub rebalance_threshold: f64,    // Threshold for rebalancing (default: 0.2)
       pub rebalance_interval: Duration, // Interval between rebalancing checks
       pub lock_timeout: Duration,      // Lock timeout duration
       pub lock_retry_interval: Duration, // Interval between lock retries
       pub max_retries: u32,            // Maximum number of lock retries
   }
   ```

3. **Shard Assignment**

   - Accounts are assigned to shards based on their ID
   - Each shard is managed by a specific service instance
   - Shard status tracking (Available, Assigned, Rebalancing, Failed)
   - Automatic shard reassignment during instance failures

4. **Shard Rebalancing**
   - Automatic detection of load imbalance
   - Redistribution of shards between instances
   - Configurable rebalancing threshold
   - Safe rebalancing with distributed locking

### Distributed Locking

The system implements distributed locking to ensure data consistency and prevent race conditions in a distributed environment.

#### Key Features

1. **Lock Management**

   - Redis-based distributed locks
   - Automatic lock expiration
   - Retry mechanism for lock acquisition
   - Safe lock release using Lua scripts

2. **Lock Usage**

   ```rust
   // Acquiring a lock
   let lock = lock_manager.acquire_lock("resource:key", Duration::from_secs(30)).await?;

   // Using the lock
   // ... perform operations ...

   // Releasing the lock
   lock.release_lock().await?;
   ```

3. **Lock Types**

   - Account creation locks
   - Transaction locks
   - Shard rebalancing locks
   - Cache update locks

4. **Lock Safety Features**
   - Automatic lock expiration
   - Lock ownership verification
   - Deadlock prevention
   - Graceful lock release

### Implementation Details

#### Shard Manager

The `ShardManager` handles shard distribution and management:

```rust
pub struct ShardManager {
    redis_client: Arc<dyn RedisClientTrait>,
    shards: Arc<DashMap<String, ShardInfo>>,
    lock_manager: Arc<LockManager>,
    config: ShardConfig,
}
```

Key operations:

- `initialize_shards()`: Creates and initializes shards
- `assign_shard()`: Assigns a shard to a service instance
- `get_shard_for_key()`: Retrieves the shard for a given key
- `rebalance_shards()`: Performs shard rebalancing

#### Lock Manager

The `LockManager` handles distributed locking:

```rust
pub struct LockManager {
    redis_client: Arc<dyn RedisClientTrait>,
}
```

Key operations:

- `acquire_lock()`: Acquires a distributed lock
- `release_lock()`: Releases a distributed lock

### Usage Examples

1. **Account Creation with Sharding**

   ```rust
   // Create account and assign to shard
   let account_id = repository.create_account(owner_name, initial_balance).await?;
   shard_manager.assign_shard(
       &format!("shard-{}", account_id.as_u128() % 16),
       &instance_id,
   ).await?;
   ```

2. **Transaction with Distributed Locking**

   ```rust
   // Acquire lock for transaction
   let lock = lock_manager.acquire_lock(
       &format!("account:transaction:{}", account_id),
       Duration::from_secs(30),
   ).await?;

   // Perform transaction
   repository.deposit_money(account_id, amount).await?;

   // Release lock
   lock.release_lock().await?;
   ```

3. **Shard Rebalancing**
   ```rust
   // Check and perform rebalancing
   shard_manager.rebalance_shards().await?;
   ```

### Best Practices

1. **Sharding**

   - Monitor shard distribution regularly
   - Configure appropriate shard count based on data size
   - Implement proper error handling for shard operations
   - Use consistent hashing for predictable shard assignment

2. **Distributed Locking**

   - Keep lock timeouts short to prevent deadlocks
   - Always release locks in a finally block
   - Implement proper error handling for lock operations
   - Use appropriate lock granularity

3. **Performance Considerations**

   - Monitor shard load distribution
   - Adjust rebalancing thresholds based on load patterns
   - Implement caching for frequently accessed shard information
   - Use appropriate lock timeouts based on operation duration

4. **Error Handling**
   - Implement retry mechanisms for lock acquisition
   - Handle shard assignment failures gracefully
   - Implement proper cleanup for failed operations
   - Monitor and log lock and shard-related errors

### Monitoring and Maintenance

1. **Key Metrics to Track**

   - Shard distribution across instances
   - Lock acquisition success/failure rates
   - Rebalancing frequency and duration
   - Operation latency per shard

2. **Health Checks**

   - Shard availability and status
   - Lock manager health
   - Redis connection status
   - Instance-shard mapping consistency

3. **Maintenance Tasks**
   - Regular shard rebalancing
   - Lock timeout cleanup
   - Failed shard recovery
   - Instance health monitoring

### Future Improvements

1. **Planned Enhancements**

   - Dynamic shard count adjustment
   - Advanced rebalancing strategies
   - Lock lease renewal mechanism
   - Shard migration capabilities

2. **Potential Optimizations**
   - Improved lock granularity
   - Enhanced shard distribution algorithms
   - Better failure detection and recovery
   - Advanced monitoring and alerting

## Authentication and Authorization

The system implements a robust authentication and authorization system with the following features:

### JWT-based Authentication

- Secure token-based authentication using JWT
- Access and refresh token support
- Token blacklisting for revoked tokens
- Configurable token expiration times
- Role-based access control (RBAC)

### Security Features

- Password hashing using Argon2
- Account lockout after failed login attempts
- Rate limiting for authentication endpoints
- Secure password reset flow
- Token rotation on refresh

### Authentication Endpoints

```bash
# Login
POST /auth/login
{
    "username": "user@example.com",
    "password": "password123"
}

# Refresh Token
POST /auth/refresh
{
    "refresh_token": "your-refresh-token"
}

# Change Password
POST /auth/change-password
{
    "current_password": "oldpass",
    "new_password": "newpass"
}

# Request Password Reset
POST /auth/reset-password
{
    "email": "user@example.com"
}

# Logout
POST /auth/logout
Authorization: Bearer your-access-token
```

### Configuration

```rust
let auth_config = AuthConfig {
    jwt_secret: std::env::var("JWT_SECRET").unwrap_or_else(|_| "default-secret".to_string()),
    refresh_token_secret: std::env::var("REFRESH_TOKEN_SECRET").unwrap_or_else(|_| "default-refresh-secret".to_string()),
    access_token_expiry: Duration::from_secs(3600), // 1 hour
    refresh_token_expiry: Duration::from_secs(604800), // 7 days
    rate_limit_requests: 100,
    rate_limit_window: Duration::from_secs(60),
    max_login_attempts: 5,
    lockout_duration: Duration::from_secs(300), // 5 minutes
};
```

## Sharding System

The application implements a sharding system for horizontal scaling and load distribution:

### Shard Management

- Dynamic shard assignment and rebalancing
- Consistent hashing for shard distribution
- Automatic shard rebalancing for load imbalances
- Shard status monitoring and health checks

### Shard Configuration

```rust
let shard_config = ShardConfig {
    num_shards: 16,
    rebalance_threshold: 0.2, // 20% imbalance triggers rebalancing
    rebalance_interval: Duration::from_secs(300), // 5 minutes
    min_shards_per_instance: 2,
    max_shards_per_instance: 8,
};
```

### Shard Operations

- Automatic shard assignment for new accounts
- Shard rebalancing based on load metrics
- Shard health monitoring
- Instance-aware request routing

## Distributed Locking

The system implements distributed locking using Redis for ensuring consistency in distributed operations:

### Lock Features

- Redis-based distributed locks
- Automatic lock expiration
- Lock acquisition with retry mechanism
- Safe lock release using Lua scripts
- Lock timeout configuration

### Lock Usage

```rust
// Acquire a lock
let lock = lock_manager.acquire_lock(
    &format!("account:{}", account_id),
    Duration::from_secs(30)
).await?;

// Perform operations with lock
// Lock is automatically released when dropped
```

### Lock Configuration

```rust
let lock_config = LockConfig {
    lock_timeout: Duration::from_secs(30),
    retry_attempts: 3,
    retry_delay: Duration::from_millis(100),
    lock_prefix: "banking:lock:".to_string(),
};
```

## Best Practices

### Authentication

1. Always use HTTPS in production
2. Implement proper password policies
3. Monitor failed login attempts
4. Regularly rotate secrets
5. Implement proper token revocation

### Sharding

1. Monitor shard distribution
2. Implement proper rebalancing strategies
3. Handle shard migration gracefully
4. Maintain shard health metrics
5. Implement proper error handling for shard operations

### Distributed Locking

1. Use appropriate lock timeouts
2. Implement proper error handling
3. Monitor lock acquisition failures
4. Use lock prefixes for different operations
5. Implement proper cleanup for abandoned locks

## Monitoring and Metrics

### Authentication Metrics

- Login success/failure rates
- Token refresh rates
- Password reset requests
- Account lockouts
- Rate limit hits

### Sharding Metrics

- Shard distribution
- Rebalancing operations
- Shard health status
- Instance load distribution
- Migration operations

### Lock Metrics

- Lock acquisition success/failure
- Lock timeout rates
- Lock contention
- Lock duration
- Abandoned locks

---

This is a simplified README. A real project would include more details on API documentation, testing, contributing, etc.

# Banking Event Sourcing System

A robust event sourcing implementation for a banking system with Redis integration.

## Key Features

### Optimistic Concurrency Control (OCC)

The system implements a robust OCC mechanism with the following features:

1. **Strong Consistency**

   - Uses SERIALIZABLE transaction isolation level
   - Implements row-level locking with `FOR UPDATE`
   - Validates version sequences for multi-event operations

2. **Version Management**

   - Maintains version cache for performance
   - Invalidates cache on conflicts
   - Ensures strict version sequence validation

3. **Conflict Handling**

   - Detects and handles concurrent modifications
   - Provides detailed error messages with version information
   - Implements retry logic for transient failures

4. **Performance Optimizations**
   - Uses batch processing for better throughput
   - Implements connection pooling
   - Caches frequently accessed data

### Event Store Features

1. **Event Processing**

   - Batch processing with configurable size
   - Priority-based event handling
   - Event validation and handling pipeline

2. **Snapshot Management**

   - Automatic snapshot creation
   - Configurable snapshot thresholds
   - Cache-based snapshot storage

3. **Error Handling**

   - Retry mechanism for transient failures
   - Detailed error types and messages
   - Metrics tracking for failures

4. **Monitoring**
   - Health check endpoints
   - Performance metrics
   - Connection pool monitoring

## Configuration

Key configuration parameters:

```rust
pub struct EventStoreConfig {
    pub max_connections: u32,        // Connection pool size
    pub batch_size: usize,          // Events per batch
    pub batch_timeout_ms: u64,      // Batch processing timeout
    pub snapshot_threshold: usize,   // Events before snapshot
    pub snapshot_interval_secs: u64, // Snapshot creation interval
}
```

## Error Handling

The system handles various error scenarios:

1. **Concurrency Conflicts**

   ```rust
   OptimisticConcurrencyConflict {
       aggregate_id: Uuid,
       expected: i64,
       actual: Option<i64>,
   }
   ```

2. **Transient Failures**

   - Automatic retry with exponential backoff
   - Maximum retry attempts: 3
   - Initial delay: 100ms

3. **Validation Errors**
   - Event sequence validation
   - Version consistency checks
   - Data integrity verification

## Performance Considerations

1. **Caching Strategy**

   - Version cache for OCC
   - Snapshot cache for quick recovery
   - Configurable TTL for cache entries

2. **Batch Processing**

   - Configurable batch sizes
   - Priority-based processing
   - Timeout-based flushing

3. **Connection Management**
   - Connection pooling
   - Automatic connection recycling
   - Health monitoring

## Usage Example

```rust
// Create event store
let event_store = EventStore::new_with_pool_size(10).await?;

// Save events with OCC
event_store.save_events(
    aggregate_id,
    events,
    expected_version
).await?;

// Get events with version
let events = event_store.get_events(
    aggregate_id,
    from_version
).await?;
```

## Monitoring and Metrics

The system provides comprehensive metrics:

1. **Performance Metrics**

   - Events processed
   - Processing latency
   - Cache hit rates

2. **Error Metrics**

   - OCC failures
   - Processing errors
   - Retry attempts

3. **Resource Metrics**
   - Connection pool usage
   - Cache utilization
   - Batch processing stats
