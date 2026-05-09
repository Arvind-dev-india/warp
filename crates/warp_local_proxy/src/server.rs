//! Builds the axum [`Router`] and runs the HTTP server.

use std::sync::Arc;

use axum::{
    routing::{any, get, post},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{config::Config, handlers, upstream::openai};

/// Default fallback model id used when the proxy can't reach the backend's
/// `/v1/models` endpoint at startup. Keeps the model picker non-empty.
pub const LOCAL_FALLBACK_MODEL_ID: &str = "local-model";

/// Shared state passed to every handler. Holds the runtime config, a single
/// `reqwest::Client` reused across upstream backend calls, and the list of
/// model ids fetched from the backend at startup (used to populate
/// FeatureModelChoice in the canned GraphQL responses).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub models: Arc<Vec<String>>,
    /// Directory for persisted conversation cache (one JSON file per task_id).
    pub conversation_cache_dir: std::path::PathBuf,
}

impl AppState {
    pub fn new(config: Config, models: Vec<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("warp_local_proxy/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client should build");
        Self {
            config: Arc::new(config),
            http,
            models: Arc::new(models),
            conversation_cache_dir: {
                let base = std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
                let dir = base
                    .join(".local")
                    .join("state")
                    .join("warp-local-proxy")
                    .join("conversations");
                std::fs::create_dir_all(&dir).ok();
                dir
            },
        }
    }

    /// Returns the model ids the proxy advertises to the Warp client. Always
    /// non-empty: when the backend's `/v1/models` returned nothing or
    /// errored, we fall back to a single `LOCAL_FALLBACK_MODEL_ID` entry so
    /// the client's model picker still works.
    pub fn advertised_models(&self) -> Vec<String> {
        if self.models.is_empty() {
            vec![LOCAL_FALLBACK_MODEL_ID.to_string()]
        } else {
            self.models.as_ref().clone()
        }
    }

    /// Returns the id the proxy uses by default when an op didn't pick one.
    /// Prefers the user's --default-model when it appears in the backend's
    /// list (or when no list is available), otherwise falls back to the
    /// first model the backend advertised.
    pub fn default_model_id(&self) -> String {
        if self.models.is_empty() || self.models.iter().any(|m| m == &self.config.default_model) {
            self.config.default_model.clone()
        } else {
            self.models[0].clone()
        }
    }

    /// Load persisted conversation history for a task.
    pub fn load_conversation(&self, task_id: &str) -> Vec<serde_json::Value> {
        let path = self.conversation_cache_dir.join(format!("{task_id}.json"));
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Save conversation history for a task to disk.
    pub fn save_conversation(&self, task_id: &str, messages: &[serde_json::Value]) {
        let path = self.conversation_cache_dir.join(format!("{task_id}.json"));
        if let Ok(data) = serde_json::to_string(messages) {
            std::fs::write(&path, data).ok();
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
        .route(
            "/ai/generate_code_review_content",
            post(handlers::ai_rest::handle),
        )
        // OAuth2 device-flow stubs so headless CLI login completes locally.
        // Note: the upstream `oauth2` crate sends the token request to a
        // separate token URL (`/api/v1/oauth/token`) per `set_token_uri()` in
        // `app/src/server/server_api.rs::create_oauth_client`; the device
        // authorization URL is at `/api/v1/oauth/device/auth`.
        .route(
            "/api/v1/oauth/device/auth",
            post(handlers::oauth::device_auth),
        )
        .route("/api/v1/oauth/token", post(handlers::oauth::device_token))
        // Firebase-fallback proxy endpoints. The Warp client first calls
        // identitytoolkit / securetoken Firebase endpoints; when those fail
        // (our firebase_auth_api_key is bogus) it retries against these
        // proxy URLs. Response shape comes from
        // `crates/firebase/src/lib.rs::FetchAccessTokenResponse`.
        .route(
            "/proxy/customToken",
            post(handlers::oauth::proxy_custom_token),
        )
        .route("/proxy/token", post(handlers::oauth::proxy_refresh_token))
        // Browser-targeted login / signup pages. The GUI opens these in the
        // user's browser; we serve a static landing page (NOT auto-redirect)
        // explaining the user is already signed in locally, with a manual
        // deep-link button as fallback. Auto-redirect was removed because
        // browsers without a warposs:// scheme handler hang on it.
        .route("/login/remote", get(handlers::browser_auth::handle_remote))
        .route("/signup/remote", get(handlers::browser_auth::handle_remote))
        // Fired after MintCustomToken — see app/src/auth/auth_manager.rs:680.
        .route(
            "/login_options/{custom_token}",
            get(handlers::browser_auth::handle_login_options),
        )
        // Cloud-only REST: agent runs, attachments, conversation snapshots.
        // Return 503 with structured error so the client knows it's unsupported.
        .route("/api/v1/agent/{*rest}", any(handlers::unsupported))
        // Multi-agent protobuf+SSE — the modern Agent Mode (Cmd+Enter).
        .route("/ai/multi-agent", post(handlers::multi_agent::handle))
        .route(
            "/ai/passive-suggestions",
            post(handlers::multi_agent::handle),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(shared)
}

/// Builds an [`AppState`] by fetching the backend's model list (best-effort)
/// and binds the HTTP server until shutdown.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let bind = config.bind;
    let probe_http = reqwest::Client::builder()
        .user_agent(concat!("warp_local_proxy/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client should build");

    tracing::info!(url = %config.models_url(), "fetching backend model list");
    let models = openai::fetch_models(&probe_http, &config).await;
    if models.is_empty() {
        tracing::warn!(
            "backend returned no usable model list; falling back to single \"{LOCAL_FALLBACK_MODEL_ID}\" entry"
        );
    } else {
        tracing::info!(count = models.len(), first = %models[0], "backend models available");
    }

    let state = AppState::new(config, models);
    tracing::info!(%bind, "warp_local_proxy listening");
    tracing::info!(
        backend = %state.config.backend_base_url,
        auth_style = ?state.config.backend_auth_style,
        default_model = %state.config.default_model,
        chat_url = %state.config.chat_completions_url(),
        "ai backend configured"
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
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
