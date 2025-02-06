use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::post,
};
use ratelimit::Ratelimiter;
use tokio::{net::TcpListener, sync::mpsc::UnboundedSender};


/// Externally-provided configuration.
struct GlobalConfig {
    /// The IP or hostname to listen for TCP connections.
    pub tcp_host: String,
    /// The port number to listen for TCP connections.
    pub tcp_port: u16,
    /// Max. number of requests per minute allowed by internal ratelimiter.
    pub req_per_minute: u64,
}

impl GlobalConfig {
    /// Initializes a config with default values.
    pub fn new() -> Self {
        Self {
            tcp_host: "0.0.0.0".to_string(),
            tcp_port: 4331,
            req_per_minute: 2,
        }
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self::new()
    }
}


/// Internally-shared state for route handlers.
struct GlobalState {
    sender: UnboundedSender<()>,
    ratelimiter: Ratelimiter,
}

impl GlobalState {
    /// Initializes the global state.
    ///
    /// # Params
    /// * `config` - Runtime configuration source.
    /// * `sender` - Sender for communicating requests to the background worker.
    ///
    /// # Panics
    /// This function *may* panic for some edge-case ratelimiter configurations.
    pub fn new(config: &GlobalConfig, sender: UnboundedSender<()>) -> Self {
        Self {
            sender,
            ratelimiter: {
                Ratelimiter::builder(config.req_per_minute, Duration::from_secs(60))
                    .initial_available(config.req_per_minute)
                    .max_tokens(config.req_per_minute)
                    .build()
                    .expect("invalid ratelimiter configuration")
            },
        }
    }
}


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<()>();

    let config = Arc::new(GlobalConfig::new());
    let state = Arc::new(GlobalState::new(config.as_ref(), sender));

    let router = Router::new()
        .route("/update", post(handle_update))
        .with_state(state);

    let config1 = config.clone();
    tokio::task::spawn_blocking(move || {
        tracing::info!("starting background thread");
        let mut receiver = receiver;

        loop {
            match receiver.blocking_recv() {
                Some(()) => {
                    tracing::info!("worker notified");
                    process_update(config1.clone());
                },
                None => break,
            }
        }

        tracing::info!("terminating background thread");
    });

    let address = format!("{}:{}", config.tcp_host, config.tcp_port);
    let listener = TcpListener::bind(&address).await
        .inspect_err(|err| tracing::error!("failed to bind tcp listener: {}", err))
        .unwrap();

    tracing::info!("serving axum app at: {}", &address);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await
        .inspect_err(|err| tracing::error!("serving axum app failed: {}", err))
        .unwrap();
}


/// Handler for `POST /update` route.
async fn handle_update(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<GlobalState>>,
) -> StatusCode {
    tracing::info!("requested /update from: {}", addr);

    match state.ratelimiter.try_wait() {
        Err(duration) => {
            tracing::warn!("internal ratelimit reached: {} s. left", duration.as_secs_f32());
            StatusCode::TOO_MANY_REQUESTS
        },
        Ok(()) => {
            if let Err(error) = state.sender.send(()) {
                tracing::error!("failed to notify worker thread: {}", error);
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::NO_CONTENT
            }
        },
    }
}


/// Background repository pull logic.
fn process_update(config: Arc<GlobalConfig>) {

}
