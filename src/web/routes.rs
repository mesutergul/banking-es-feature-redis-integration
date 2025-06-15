use crate::{application::AccountService, infrastructure::auth::AuthService, web::handlers::*};
use axum::{
    routing::{get, post, put},
    Router,
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

// New function that only sets up the router with routes, expecting services to be passed in
pub fn create_router(service: Arc<AccountService>, auth_service: Arc<AuthService>) -> Router {
    Router::new()
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/accounts", post(create_account))
        .route("/api/accounts/{id}", get(get_account))
        .route("/api/accounts/{id}/deposit", put(deposit_money))
        .route("/api/accounts/{id}/withdraw", put(withdraw_money))
        .route("/api/accounts", get(get_all_accounts))
        .route(
            "/api/accounts/{id}/transactions",
            get(get_account_transactions),
        )
        .route("/api/health", get(health_check))
        .route("/api/metrics", get(metrics))
        .route("/api/transactions/batch", post(batch_transactions))
        .with_state((service, auth_service))
        .fallback_service(ServeDir::new("static"))
        .layer(CorsLayer::permissive())
}
