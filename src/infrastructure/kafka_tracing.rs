use crate::domain::{Account, AccountEvent};
use crate::infrastructure::kafka_metrics::KafkaMetrics;
use anyhow::Result;
use async_trait::async_trait;
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::{self, IdGenerator, RandomIdGenerator, Sampler};
use opentelemetry_sdk::Resource;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, Level};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use uuid::Uuid;

#[async_trait]
pub trait KafkaTracingTrait: Send + Sync {
    fn init_tracing(&self) -> Result<()>;
    fn trace_event_processing(&self, account_id: Uuid, events: &[AccountEvent], version: i64);
    fn trace_error(&self, error: &anyhow::Error, context: &str);
    fn trace_metrics(&self);
    fn trace_performance_metrics(&self);
    fn trace_recovery_operation(&self, strategy: &str, account_id: Option<Uuid>, status: &str);
}

#[derive(Clone)]
pub struct KafkaTracing {
    metrics: Arc<KafkaMetrics>,
}

impl KafkaTracing {
    pub fn new(metrics: Arc<KafkaMetrics>) -> Self {
        Self { metrics }
    }

    pub fn init_tracing(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Configure OpenTelemetry
        let tracer = opentelemetry_jaeger::new_agent_pipeline()
            .with_service_name("banking-es-kafka")
            .with_endpoint("localhost:6831")
            .with_trace_config(
                opentelemetry_sdk::trace::config()
                    .with_sampler(opentelemetry_sdk::trace::Sampler::AlwaysOn)
                    .with_id_generator(opentelemetry_sdk::trace::RandomIdGenerator::default())
                    .with_resource(opentelemetry_sdk::Resource::new(vec![
                        opentelemetry::KeyValue::new("service.name", "banking-es-kafka"),
                        opentelemetry::KeyValue::new("deployment.environment", "production"),
                    ])),
            )
            .install_batch(opentelemetry_sdk::runtime::Tokio)?;

        // Create OpenTelemetry layer
        let opentelemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        // Configure logging
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,banking_es=debug"));

        // Initialize subscriber
        tracing_subscriber::registry()
            .with(env_filter)
            .with(opentelemetry_layer)
            .init();

        Ok(())
    }

    pub fn trace_event_processing(&self, account_id: Uuid, events: &[AccountEvent], version: i64) {
        let span = tracing::info_span!(
            "process_events",
            account_id = %account_id,
            event_count = events.len(),
            version = version
        );

        let _guard = span.enter();
        info!("Processing events for account {}", account_id);

        for event in events {
            match event {
                AccountEvent::AccountCreated { .. } => {
                    info!("Processing AccountCreated event");
                }
                AccountEvent::MoneyDeposited { amount, .. } => {
                    info!("Processing MoneyDeposited event: {}", amount);
                }
                AccountEvent::MoneyWithdrawn { amount, .. } => {
                    info!("Processing MoneyWithdrawn event: {}", amount);
                }
                AccountEvent::AccountClosed { reason, .. } => {
                    info!("Processing AccountClosed event: {}", reason);
                }
            }
        }
    }

    pub fn trace_dlq_operation(&self, account_id: Uuid, operation: &str, retry_count: u32) {
        let span = tracing::info_span!(
            "dlq_operation",
            account_id = %account_id,
            operation = operation,
            retry_count = retry_count
        );

        let _guard = span.enter();
        info!(
            "DLQ operation '{}' for account {} (retry {})",
            operation, account_id, retry_count
        );
    }

    pub fn trace_recovery_operation(&self, strategy: &str, account_id: Option<Uuid>, status: &str) {
        let span = tracing::info_span!(
            "recovery_operation",
            strategy = strategy,
            account_id = ?account_id,
            status = status
        );

        let _guard = span.enter();
        info!(
            "Recovery operation '{}' for account {:?}: {}",
            strategy, account_id, status
        );
    }

    pub fn trace_metrics(&self) {
        let span = tracing::info_span!("metrics_snapshot");
        let _guard = span.enter();

        let error_rate = self.metrics.get_error_rate();
        let processing_latency = self.metrics.get_average_processing_latency();
        let consumer_lag = self
            .metrics
            .consumer_lag
            .load(std::sync::atomic::Ordering::Relaxed);

        info!(
            "Metrics snapshot: error_rate={:.2}%, processing_latency={:.2}ms, consumer_lag={}",
            error_rate * 100.0,
            processing_latency,
            consumer_lag
        );

        if error_rate > 0.1 {
            warn!("High error rate detected: {:.2}%", error_rate * 100.0);
        }

        if consumer_lag > 1000 {
            warn!("High consumer lag detected: {}", consumer_lag);
        }
    }

    pub fn trace_performance_metrics(&self) {
        let span = tracing::info_span!("performance_metrics");
        let _guard = span.enter();

        let memory_usage = self
            .metrics
            .memory_usage
            .load(std::sync::atomic::Ordering::Relaxed);
        let cpu_usage = self
            .metrics
            .cpu_usage
            .load(std::sync::atomic::Ordering::Relaxed);
        let thread_count = self
            .metrics
            .thread_count
            .load(std::sync::atomic::Ordering::Relaxed);

        info!(
            "Performance metrics: memory={:.2}MB, cpu={:.1}%, threads={}",
            memory_usage as f64 / 1_000_000.0,
            cpu_usage as f64 / 100.0,
            thread_count
        );

        if memory_usage > 1_000_000_000 {
            warn!("High memory usage: {:.2}GB", memory_usage as f64 / 1e9);
        }

        if cpu_usage > 80 {
            warn!("High CPU usage: {:.1}%", cpu_usage as f64 / 100.0);
        }
    }

    pub fn trace_error(&self, error: &anyhow::Error, context: &str) {
        let span = tracing::error_span!(
            "error",
            context = context,
            error = %error
        );

        let _guard = span.enter();
        error!("Error in {}: {}", context, error);
    }
}

impl Drop for KafkaTracing {
    fn drop(&mut self) {
        // Shutdown OpenTelemetry tracer
        opentelemetry::global::shutdown_tracer_provider();
    }
}

impl KafkaTracingTrait for KafkaTracing {
    fn init_tracing(&self) -> Result<()> {
        // Implementation
        Ok(())
    }

    fn trace_event_processing(&self, account_id: Uuid, events: &[AccountEvent], version: i64) {
        // Implementation
    }

    fn trace_error(&self, error: &anyhow::Error, context: &str) {
        // Implementation
    }

    fn trace_metrics(&self) {
        // Implementation
    }

    fn trace_performance_metrics(&self) {
        // Implementation
    }

    fn trace_recovery_operation(&self, strategy: &str, account_id: Option<Uuid>, status: &str) {
        // Implementation
    }
}
