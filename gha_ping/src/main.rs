use std::{
    env,
    net::SocketAddr,
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
    /// Path to a *private* deploy key for Git SSH connection.
    pub ssh_key_path: String,
    /// Target Git directory into which the repository should be cloned.
    pub out_repo_path: String,
    /// Use verbose logging.
    pub verbose: bool,
}

impl GlobalConfig {
    /// Initializes a config using environment and default values.
    pub fn new() -> Self {
        fn load_string(key: &'static str, default: &str) -> String {
            env::var(key).unwrap_or_else(|_| default.into())
        }

        fn load_u64(key: &'static str, default: u64) -> u64 {
            if let Ok(string) = env::var(key) {
                let msg = format!("invalid {} value: {}", key, &string);
                string.parse::<u64>().expect(&msg)
            } else {
                default
            }
        }

        fn load_bool(key: &'static str, default: bool) -> bool {
            if let Ok(string) = env::var(key) {
                let lowercase = string.to_lowercase();
                match lowercase.as_str() {
                    "t" | "true" | "on" | "yes" => true,
                    "f" | "false" | "off" | "no" => false,
                    other => panic!("invalid {} value: {}", key, other),
                }
            } else {
                default
            }
        }

        Self {
            tcp_host: load_string("GHAP_TCP_HOST", "0.0.0.0"),
            tcp_port: load_u64("GHAP_TCP_PORT", 4331) as u16,
            req_per_minute: load_u64("GHAP_MAX_RPM", 30),
            ssh_repo_url: load_string("GHAP_SSH_REPO_URL", "git@github.com:alexobolev/gha-ping.git"),
            ssh_key_path: load_string("GHAP_SSH_KEY_PATH", "./local/gha_ping_ed25519"),
            out_repo_path: load_string("GHAP_OUT_REPO_PATH", "./local/tmp"),
            verbose: load_bool("GHAP_VERBOSE", true),
        }
    }

    /// Logs contents of this configuration with `Level::INFO` severity.
    pub fn log_with_prefix(&self, prefix: &str) {
        tracing::info!("{}'tcp_host' = {}", prefix, self.tcp_host);
        tracing::info!("{}'tcp_port' = {}", prefix, self.tcp_port);
        tracing::info!("{}'req_per_minute' = {}", prefix, self.req_per_minute);
        tracing::info!("{}'ssh_repo_url' = {}", prefix, self.ssh_repo_url);
        tracing::info!("{}'ssh_key_path' = {}", prefix, self.ssh_key_path);
        tracing::info!("{}'out_repo_path' = {}", prefix, self.out_repo_path);
        tracing::info!("{}'verbose' = {}", prefix, self.verbose);
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
    pub ssh: String,
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
            ssh: format!("ssh -o IdentitiesOnly=yes -F none -i {}", config.ssh_key_path),
        }
    }
}


#[tokio::main]
async fn main() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<()>();

    let config = Arc::new(GlobalConfig::new());
    let state = Arc::new(GlobalState::new(config.as_ref(), sender));

    let max_level = if config.verbose { Level::TRACE } else { Level::INFO };
    tracing_subscriber::fmt().with_max_level(max_level).init();

    tracing::info!("initializing with config:");
    config.log_with_prefix("   ");

    tracing::debug!("veryfying github credentials");
    if !check_github_creds(config.clone(), state.clone()) {
        tracing::error!("can't verify github credentials");
        return;
    }

    let config1 = config.clone();
    let state1 = state.clone();
    tokio::task::spawn_blocking(move || {
        tracing::debug!("starting background thread");
        let mut receiver = receiver;

        while let Some(()) = receiver.blocking_recv() {
            process_update(config1.clone(), state1.clone());
        }

        tracing::debug!("terminating background thread");
    });

    let router = Router::new()
        .route("/update", post(handle_update))
        .with_state(state);

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


/// Checks that given Git repository and credentials are valid.
fn check_github_creds(config: Arc<GlobalConfig>, state: Arc<GlobalState>) -> bool {
    let result = Command::new("git")
        .env("GIT_SSH_COMMAND", &state.ssh)
        .args(["ls-remote", &config.ssh_repo_url])
        .output();

    match result {
        Ok(output) => output.status.success(),
        Err(error) => {
            tracing::error!("failed to execute git: {}", error);
            false
        }
    }
}


/// Clones the repository on request.
fn process_update(config: Arc<GlobalConfig>, state: Arc<GlobalState>) {
    tracing::info!("running `git clone` on request: {} -> {}",
        &config.ssh_repo_url, &config.out_repo_path);

    // TODO: Do something with existing directory...

    let result = Command::new("git")
        .env("GIT_SSH_COMMAND", &state.ssh)
        .args(["clone", &config.ssh_repo_url, &config.out_repo_path])
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                tracing::info!("git clone code:   {}", output.status.code().unwrap_or(-1));
                if let Some(stdout) = try_load_string(output.stdout) {
                    for line in stdout.lines() {
                        tracing::debug!("git clone stdout:   {}", line);
                    }
                }
            } else {
                tracing::error!("git clone code:   {}", output.status.code().unwrap_or(-1));
                if let Some(stderr) = try_load_string(output.stderr) {
                    for line in stderr.lines() {
                        tracing::error!("git clone stderr:   {}", line);
                    }
                }
            }
        },
        Err(error) => {
            tracing::error!("failed to execute git: {}", error);
        }
    }
}


/// Attempts to parse an input buffer first as a sequence of UTF-8 characters,
/// then as a sequence of UTF-16 characters.
fn try_load_string(source: Vec<u8>) -> Option<String> {
    if let Ok(str8) = String::from_utf8(source.clone()) {
        return Some(str8);
    }

    // SAFETY: Both u8 and u16 are POD types, and we're dealing with alignment here.
    let (front, slice, back) = unsafe { source.as_slice().align_to::<u16>() };
    if front.is_empty() && back.is_empty() {
        if let Ok(str16) = String::from_utf16(slice) {
            return Some(str16);
        }
    }

    None
}
