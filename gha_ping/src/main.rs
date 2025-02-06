use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::post,
};
use git2::{
    Cred, FetchOptions, RemoteCallbacks,
    build::RepoBuilder,
};
use ratelimit::Ratelimiter;
use tokio::{
    net::TcpListener,
    sync::mpsc::UnboundedSender,
};
use tracing::Level;


/// Externally-provided configuration.
#[derive(Debug)]
struct GlobalConfig {
    /// The IP or hostname to listen for TCP connections.
    pub tcp_host: String,
    /// The port number to listen for TCP connections.
    pub tcp_port: u16,
    /// Max. number of requests per minute allowed by internal ratelimiter.
    pub req_per_minute: u64,
    /// Source Git repository URL using SSH.
    pub ssh_repo_url: String,
    /// Path to a *public* deploy key for Git SSH connection.
    pub ssh_key_path_pub: String,
    /// Path to a *private* deploy key for Git SSH connection.
    pub ssh_key_path: String,
    /// Target Git directory into which the repository should be cloned.
    pub out_repo_path: PathBuf,
}

impl GlobalConfig {
    /// Initializes a config with default values.
    pub fn new() -> Self {
        Self {
            tcp_host: "0.0.0.0".into(),
            tcp_port: 4331,
            req_per_minute: 30,
            ssh_repo_url: "git@github.com:alexobolev/gha-ping.git".into(),
            ssh_key_path_pub: "./local/gha_ping_ed25519.pub".into(),
            ssh_key_path: "./local/gha_ping_ed25519".into(),
            out_repo_path: "./local/tmp".into(),
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
    pub sender: UnboundedSender<()>,
    pub ratelimiter: Ratelimiter,
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
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::TRACE)
        .init();

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<()>();

    let config = Arc::new(GlobalConfig::new());
    let state = Arc::new(GlobalState::new(config.as_ref(), sender));

    let router = Router::new()
        .route("/update", post(handle_update))
        .with_state(state);

    // This will check SSH key but not repository path.
    if !check_github_creds(config.clone()) {
        tracing::error!("can't verify github credentials");
        return;
    }

    let config1 = config.clone();
    tokio::task::spawn_blocking(move || {
        tracing::info!("starting background thread");
        let mut receiver = receiver;

        while let Some(()) = receiver.blocking_recv() {
            process_update(config1.clone());
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


/// Checks that given ssh credentials are valid for GitHub.
fn check_github_creds(config: Arc<GlobalConfig>) -> bool {
    let output = Command::new("ssh")
        .args([
            "-o", "IdentitiesOnly=yes",
            "-F", "none",
            "-i", &config.ssh_key_path,
            "-T", "git@github.com",
        ])
        .output();

    match output {
        Ok(output) => {
            output.status.code().is_some_and(|c| c == 1)
        },
        Err(error) => {
            tracing::error!("failed to execute ssh: {}", error);
            false
        }
    }
}


/// Clones the repository on request.
fn process_update(config: Arc<GlobalConfig>) {
    // Can't have global RepoBuilder and friends here because it has lifetimes...

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|url, username, allowed_types| {
        tracing::info!("logging in to: {}, allowed: {:?}", url, allowed_types);

        let username = username.unwrap_or("git");
        let publickey = Some(Path::new(&config.ssh_key_path_pub));
        let privatekey = Path::new(&config.ssh_key_path);

        let creds = Cred::ssh_key(username, publickey, privatekey, None);
        debug_assert!(creds.is_ok(), "failed to create SSH key credential");

        creds
    });

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let mut repo_builder = RepoBuilder::new();
    repo_builder.fetch_options(fetch_options);

    tracing::debug!("cloning repository: {}", &config.ssh_repo_url);
    match repo_builder.clone(&config.ssh_repo_url, &config.out_repo_path) {
        Ok(_) => tracing::info!("cloned the repository"),
        Err(error) =>
            tracing::error!("failed to clone: {} code = {:?}, class = {:?}",
                error.message(), error.code(), error.class()),
    }
}
