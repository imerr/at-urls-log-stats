use crate::config::Config;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use log::error;
use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::select;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// How often to run cleanup
const CLEANUP_INTERVAL_SECS: u64 = 10;
/// After how long to remove an entry
const CLEANUP_ENTRY_TTL_SECS: u64 = 60;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabel {
    domain: String,
    status_code: u16,
}

pub struct Metrics {
    cancel: CancellationToken,
    pub registry: Registry,
    domain_requests: Family<RequestLabel, Counter>,
    domain_last_seen: RwLock<HashMap<RequestLabel, Instant>>,
}

impl Metrics {
    pub fn new(config: &Config, cancel: CancellationToken) -> Arc<Self> {
        let mut registry = <Registry>::default();
        let domain_requests = Family::<RequestLabel, Counter>::default();
        // Register the metric family with the registry.
        registry.register(
            "requests_per_domain",
            "A counter of requests per domain.",
            domain_requests.clone(),
        );
        let metrics = Arc::new(Self {
            cancel,
            registry,
            domain_requests,
            domain_last_seen: RwLock::new(HashMap::new()),
        });
        tokio::spawn(metrics.clone().serve(config.listen_address.clone()));
        tokio::spawn(metrics.clone().periodic_cleanup());
        metrics
    }

    pub async fn serve(self: Arc<Self>, address: String) {
        // build our application with a route
        let app = Router::new()
            .route("/metrics", get(metrics))
            .with_state(self.clone());

        // run our app with hyper
        let listener = tokio::net::TcpListener::bind(address.as_str())
            .await
            .expect("failed to bind to address");
        select! {
            _ = self.cancel.cancelled() => {},
            r = axum::serve(listener, app) => {
            if let Err(e) = r {
                panic!("Could not start metric server: {}", e);
            }
        }};
    }
    pub async fn periodic_cleanup(self: Arc<Self>) {
        loop {
            select! {
                _ = self.cancel.cancelled() => {},
                _ = sleep(std::time::Duration::from_secs(CLEANUP_INTERVAL_SECS)) => {}
            }
            let now = Instant::now();
            let ttl = std::time::Duration::from_secs(CLEANUP_ENTRY_TTL_SECS);
            let mut last_seen = self.domain_last_seen.write().await;
            last_seen.retain(|label, last_seen| {
                if now.duration_since(*last_seen) > ttl {
                    self.domain_requests.remove(&label);
                    false
                } else {
                    true
                }
            });
        }
    }

    pub async fn request(self: &Arc<Self>, domain: String, status_code: u16) {
        let label = RequestLabel {
            domain,
            status_code,
        };
        self.domain_requests.get_or_create(&label).inc();
        let mut last_seen = self.domain_last_seen.write().await;
        if let Some(last_seen_time) = last_seen.get_mut(&label) {
            *last_seen_time = Instant::now()
        } else {
            last_seen.insert(label, Instant::now());
        }
    }
}

async fn metrics(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    let mut buffer = String::new();
    match encode(&mut buffer, &metrics.registry) {
        Ok(_) => (StatusCode::OK, buffer),
        Err(e) => {
            error!("Failed to encode state: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode metrics".to_string(),
            )
        }
    }
}
