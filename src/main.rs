use crate::infrastructure::auth::{AuthConfig, AuthService};
use crate::infrastructure::cache_service::{CacheConfig, CacheService, EvictionPolicy};
use crate::infrastructure::event_store::{EventStore, DB_POOL};
use crate::infrastructure::kafka_abstraction::KafkaConfig;
use crate::infrastructure::projections::ProjectionStore;
use crate::infrastructure::redis_abstraction::RealRedisClient;
use crate::infrastructure::redis_abstraction::RedisClient;
use crate::infrastructure::scaling::{ScalingConfig, ScalingManager, ServiceInstance};
use crate::web::routes::create_router;
use anyhow::Result;
use axum::{http::Method, routing::IntoMakeService, Router};
use chrono::Utc;
use dotenv;
use redis;
use sqlx::PgPool;
use std::time::Duration;
use std::{net::SocketAddr, sync::Arc};
use tokio::signal;
use tower_http::cors::CorsLayer;
use tracing::{info, Level};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

mod application;
mod domain;
mod infrastructure;
mod web;

use crate::application::AccountService;
use crate::infrastructure::middleware::RequestMiddleware;
use crate::infrastructure::{AccountRepository, EventStoreConfig, UserRepository};

use opentelemetry::sdk::export::trace::SpanExporter;
use opentelemetry::trace::TracerProvider;

use crate::infrastructure::init::{self, ServiceContext};
use infrastructure::config::AppConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    // Initialize tracing with OpenTelemetry
    let tracer = opentelemetry_jaeger::new_agent_pipeline()
        .with_service_name("banking-es")
        .with_endpoint("localhost:6831")
        .with_trace_config(
            opentelemetry::sdk::trace::config()
                .with_sampler(opentelemetry::sdk::trace::Sampler::AlwaysOn)
                .with_id_generator(opentelemetry::sdk::trace::RandomIdGenerator::default())
                .with_resource(opentelemetry::sdk::Resource::new(vec![
                    opentelemetry::KeyValue::new("service.name", "banking-es"),
                    opentelemetry::KeyValue::new("deployment.environment", "production"),
                ])),
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    let opentelemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,banking_es=debug"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(opentelemetry_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Initialize all services
    let services = init::init_all_services().await?;

    // Register this instance
    let instance = ServiceInstance {
        id: Uuid::new_v4().to_string(),
        host: "localhost".to_string(),
        port: 8080,
        status: crate::infrastructure::scaling::InstanceStatus::Active,
        metrics: crate::infrastructure::scaling::InstanceMetrics {
            cpu_usage: 0.0,
            memory_usage: 0.0,
            request_count: 0,
            error_count: 0,
            latency_ms: 0,
        },
        shard_assignments: vec![],
        last_heartbeat: Utc::now(),
    };
    services.scaling_manager.register_instance(instance).await?;

    // Start scaling manager
    let scaling_manager_clone = services.scaling_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = scaling_manager_clone.start_scaling_manager().await {
            eprintln!("Scaling manager error: {}", e);
        }
    });

    // Create router
    let app = create_router(
        services.account_service.clone(),
        services.auth_service.clone(),
    );

    // Start server
    let config = AppConfig::default();
    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    info!("Starting server on {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutting down gracefully...");
}
