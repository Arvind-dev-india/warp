//! Builds the axum [`Router`] and runs the HTTP server.

use std::sync::Arc;

use axum::{routing::{get, post}, Router};
use tower_http::trace::TraceLayer;

use crate::{config::Config, handlers};

/// Shared state passed to every handler. Right now just holds the config.
/// Subsequent commits will add an HTTP client to talk to the AI backend.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

/// Builds the route table. Exposed as a free function so tests can drive it
/// without binding a real socket.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/graphql/v2", post(handlers::graphql::handle))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Binds to `state.config.bind` and serves until shutdown.
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = state.config.bind;
    let app = router(state.clone());

    tracing::info!(%addr, "warp_local_proxy listening");
    tracing::info!(
        backend = %state.config.backend_base_url,
        auth_style = ?state.config.backend_auth_style,
        model = %state.config.default_model,
        chat_url = %state.config.chat_completions_url(),
        "ai backend configured"
    );

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
