//! Builds the axum [`Router`] and runs the HTTP server.

use std::sync::Arc;

use axum::{
    routing::{any, get, post},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{config::Config, handlers};

/// Shared state passed to every handler. Holds the runtime config and a single
/// `reqwest::Client` reused across upstream backend calls.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("warp_local_proxy/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client should build");
        Self {
            config: Arc::new(config),
            http,
        }
    }
}

/// Builds the route table. Exposed as a free function so tests can drive it
/// without binding a real socket.
pub fn router(state: AppState) -> Router {
    let shared = Arc::new(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/graphql/v2", post(handlers::graphql::handle))
        .route("/ai/generate_code_review_content", post(handlers::ai_rest::handle))
        // OAuth2 device-flow stubs so headless CLI login completes locally.
        // Note: the upstream `oauth2` crate sends the token request to a
        // separate token URL (`/api/v1/oauth/token`) per `set_token_uri()` in
        // `app/src/server/server_api.rs::create_oauth_client`; the device
        // authorization URL is at `/api/v1/oauth/device/auth`.
        .route("/api/v1/oauth/device/auth", post(handlers::oauth::device_auth))
        .route("/api/v1/oauth/token", post(handlers::oauth::device_token))
        // Firebase-fallback proxy endpoints. The Warp client first calls
        // identitytoolkit / securetoken Firebase endpoints; when those fail
        // (our firebase_auth_api_key is bogus) it retries against these
        // proxy URLs. Response shape comes from
        // `crates/firebase/src/lib.rs::FetchAccessTokenResponse`.
        .route("/proxy/customToken", post(handlers::oauth::proxy_custom_token))
        .route("/proxy/token", post(handlers::oauth::proxy_refresh_token))
        // Cloud-only REST: agent runs, attachments, conversation snapshots.
        // Return 503 with structured error so the client knows it's unsupported.
        .route("/api/v1/agent/{*rest}", any(handlers::unsupported))
        .layer(TraceLayer::new_for_http())
        .with_state(shared)
}

/// Binds to `state.config.bind` and serves until shutdown.
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = state.config.bind;
    tracing::info!(%addr, "warp_local_proxy listening");
    tracing::info!(
        backend = %state.config.backend_base_url,
        auth_style = ?state.config.backend_auth_style,
        model = %state.config.default_model,
        chat_url = %state.config.chat_completions_url(),
        "ai backend configured"
    );

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
