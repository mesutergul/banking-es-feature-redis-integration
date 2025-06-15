use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Request, Response, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rust_decimal::{
    prelude::{FromPrimitive, ToPrimitive},
    Decimal,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Semaphore;
use tower::{Layer, Service, ServiceBuilder, ServiceExt};
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

use crate::domain::{AccountCommand, AccountError};
use crate::infrastructure::{
    auth::{
        AuthConfig, AuthService, Claims, LoginRequest, LoginResponse, LogoutRequest,
        PasswordResetResponse, UserRole,
    },
    cache_service::{CacheConfig, CacheService, EvictionPolicy},
    event_store::{EventStore, EventStoreConfig, DB_POOL},
    kafka_abstraction::KafkaConfig,
    middleware::{
        AccountCreationValidator, RequestContext, RequestMiddleware, TransactionValidator,
    },
    projections::{AccountProjection, ProjectionConfig, ProjectionStore, TransactionProjection},
    rate_limiter::RateLimitConfig,
    redis_abstraction::{RealRedisClient, RedisClient, RedisPoolConfig},
    repository::{AccountRepository, AccountRepositoryTrait},
    scaling::{InstanceMetrics, ScalingConfig, ScalingManager, ServiceInstance},
    sharding::{LockManager, ShardConfig, ShardManager},
};
use crate::{application::AccountService, infrastructure::UserRepository};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateAccountRequest {
    pub owner_name: String,
    pub initial_balance: f64,
}

#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub account_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TransactionRequest {
    pub amount: Decimal,
}

#[derive(Debug, Deserialize)]
pub struct BatchTransactionRequest {
    pub transactions: Vec<SingleTransaction>,
}

#[derive(Debug, Deserialize)]
pub struct SingleTransaction {
    pub account_id: Uuid,
    pub amount: Decimal,
    pub transaction_type: String, // "deposit" or "withdraw"
}

#[derive(Debug, Serialize)]
pub struct BatchTransactionResponse {
    pub successful: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct AccountResponse {
    pub id: String,
    pub balance: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub roles: Vec<UserRole>,
}

pub async fn create_account(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Json(payload): Json<CreateAccountRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let ctx = create_request_context(
        get_client_id(&HeaderMap::new()),
        "create_account".to_string(),
        serde_json::to_value(&payload).unwrap(),
        HeaderMap::new(),
    );
    let validation = service.middleware.process_request(ctx).await;
    if let Ok(result) = validation {
        if !result.is_valid {
            return Err((StatusCode::BAD_REQUEST, result.errors.join(", ")));
        }
    }
    let account = service
        .create_account(
            payload.owner_name,
            Decimal::from_f64(payload.initial_balance).unwrap_or(Decimal::ZERO),
        )
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(CreateAccountResponse {
        account_id: account,
    }))
}

pub async fn get_account(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let account = service
        .get_account(id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    match account {
        Some(acc) => Ok(Json(AccountResponse {
            id: acc.id.to_string(),
            balance: acc.balance.to_f64().unwrap_or(0.0),
        })),
        None => Err((StatusCode::NOT_FOUND, "Account not found".to_string())),
    }
}

pub async fn deposit_money(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Path(id): Path<Uuid>,
    Json(payload): Json<TransactionRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    service
        .deposit_money(id, payload.amount)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(StatusCode::OK)
}

pub async fn withdraw_money(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Path(id): Path<Uuid>,
    Json(payload): Json<TransactionRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    service
        .withdraw_money(id, payload.amount)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(StatusCode::OK)
}

pub async fn get_all_accounts(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
) -> Result<Json<Vec<AccountProjection>>, (StatusCode, Json<ErrorResponse>)> {
    match service.get_all_accounts().await {
        Ok(accounts) => Ok(Json(accounts)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

pub async fn get_account_transactions(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Path(account_id): Path<Uuid>,
) -> Result<Json<Vec<TransactionProjection>>, (StatusCode, Json<ErrorResponse>)> {
    match service.get_account_transactions(account_id).await {
        Ok(transactions) => Ok(Json(transactions)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

pub async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

pub async fn metrics(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let metrics = serde_json::json!({
        "available_request_permits": service.semaphore.available_permits(),
        "max_requests_per_second": service.max_requests_per_second,
    });

    Ok(Json(metrics))
}

pub async fn login(
    State((_, auth_service)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Json(payload): Json<LoginRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    match auth_service
        .login(&payload.username, &payload.password)
        .await
    {
        Ok(token) => Ok(Json(token)),
        Err(e) => Err((StatusCode::UNAUTHORIZED, e.to_string())),
    }
}

pub async fn logout(
    State((_, auth_service)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Json(payload): Json<LogoutRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    match auth_service.blacklist_token(&payload.token).await {
        Ok(_) => Ok(StatusCode::OK),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

pub async fn batch_transactions(
    State((service, _)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Json(request): Json<BatchTransactionRequest>,
) -> Result<Json<BatchTransactionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _permit = service.semaphore.acquire().await.unwrap();

    let mut successful = 0;
    let mut failed = 0;
    let mut errors = Vec::new();

    for transaction in request.transactions {
        let result = match transaction.transaction_type.as_str() {
            "deposit" => {
                service
                    .deposit_money(transaction.account_id, transaction.amount)
                    .await
            }
            "withdraw" => {
                service
                    .withdraw_money(transaction.account_id, transaction.amount)
                    .await
            }
            _ => Err(AccountError::InvalidAmount(transaction.amount)),
        };

        match result {
            Ok(()) => successful += 1,
            Err(e) => {
                failed += 1;
                errors.push(format!(
                    "Account {}: {}",
                    transaction.account_id,
                    e.to_string()
                ));
            }
        }
    }

    Ok(Json(BatchTransactionResponse {
        successful,
        failed,
        errors,
    }))
}

pub async fn register(
    State((_, auth_service)): State<(Arc<AccountService>, Arc<AuthService>)>,
    Json(payload): Json<RegisterRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    match auth_service
        .register_user(
            &payload.username,
            &payload.email,
            &payload.password,
            payload.roles,
        )
        .await
    {
        Ok(user) => Ok(Json(user)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

fn get_client_id(headers: &HeaderMap) -> String {
    headers
        .get("X-Client-ID")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

fn create_request_context(
    client_id: String,
    request_type: String,
    payload: serde_json::Value,
    headers: HeaderMap,
) -> RequestContext {
    RequestContext {
        client_id,
        request_type,
        payload,
        headers,
    }
}
