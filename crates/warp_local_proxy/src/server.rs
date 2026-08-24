//! Builds the axum [`Router`] and runs the HTTP server.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::IntoResponse,
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
    models: Arc<RwLock<Vec<String>>>,
    conversation_tasks: Arc<RwLock<HashMap<String, String>>>,
    agent_tasks: Arc<RwLock<HashMap<String, String>>>,
    launched_agents: Arc<RwLock<HashMap<String, String>>>,
    known_agent_addresses: Arc<RwLock<HashSet<String>>>,
    required_conversation_versions: Arc<RwLock<HashMap<String, u64>>>,
    next_message_version: Arc<AtomicU64>,
    pub agent_api: Arc<handlers::agent_api::AgentApiState>,
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
            models: Arc::new(RwLock::new(models)),
            conversation_tasks: Arc::new(RwLock::new(HashMap::new())),
            agent_tasks: Arc::new(RwLock::new(HashMap::new())),
            launched_agents: Arc::new(RwLock::new(HashMap::new())),
            known_agent_addresses: Arc::new(RwLock::new(HashSet::new())),
            required_conversation_versions: Arc::new(RwLock::new(HashMap::new())),
            next_message_version: Arc::new(AtomicU64::new(1)),
            agent_api: Arc::new(handlers::agent_api::AgentApiState::new()),
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

    /// Returns the configured model first, followed by usable models discovered
    /// from the optional upstream backend.
    pub fn advertised_models(&self) -> Vec<String> {
        let configured = self.default_model_id();
        let mut models = vec![configured.clone()];
        for model in self.models.read().expect("models lock poisoned").iter() {
            if model != &configured
                && !is_likely_embedding_model(model)
                && !models.iter().any(|existing| existing == model)
            {
                models.push(model.clone());
            }
        }
        models
    }

    /// Returns the explicitly configured model even when the optional
    /// backend's model-list endpoint is unavailable or incomplete.
    pub fn default_model_id(&self) -> String {
        if self.config.default_model.trim().is_empty() {
            LOCAL_FALLBACK_MODEL_ID.to_string()
        } else {
            self.config.default_model.clone()
        }
    }

    /// Refresh optional backend discovery without making model exposure depend
    /// on that endpoint. The configured model remains authoritative.
    pub async fn refresh_models(&self) {
        let models = openai::fetch_models(&self.http, &self.config).await;
        *self.models.write().expect("models lock poisoned") = models;
    }

    pub fn register_conversation_task(&self, conversation_id: &str, task_id: &str) {
        if !is_safe_cache_key(conversation_id) || !is_safe_cache_key(task_id) {
            return;
        }
        self.conversation_tasks
            .write()
            .expect("conversation task lock poisoned")
            .insert(conversation_id.to_string(), task_id.to_string());
        self.register_agent_address(conversation_id);
        self.register_agent_address(task_id);

        let task_path = self.conversation_cache_dir.join(format!("{task_id}.json"));
        if let Ok(data) = std::fs::read_to_string(&task_path) {
            if serde_json::from_str::<Vec<serde_json::Value>>(&data)
                .is_ok_and(|messages| conversation_is_terminal(&messages))
            {
                let alias_path = self
                    .conversation_cache_dir
                    .join(format!("{conversation_id}.json"));
                let _ = std::fs::write(&alias_path, data);
                self.write_alias_version(conversation_id, &alias_path);
            }
        }
    }

    pub fn register_agent_task(&self, agent_name: &str, task_id: &str) {
        if agent_name.is_empty() || !is_safe_cache_key(task_id) {
            return;
        }
        self.agent_tasks
            .write()
            .expect("agent task lock poisoned")
            .insert(agent_name.to_string(), task_id.to_string());
        self.register_agent_address(agent_name);
        self.register_agent_address(task_id);

        if let Some(conversation_id) = self
            .launched_agents
            .read()
            .expect("launched agents lock poisoned")
            .get(agent_name)
            .cloned()
        {
            self.register_conversation_task(&conversation_id, task_id);
        }
    }

    pub fn register_launched_agent(&self, agent_name: &str, conversation_id: &str) {
        if agent_name.is_empty() || !is_safe_cache_key(conversation_id) {
            return;
        }
        self.launched_agents
            .write()
            .expect("launched agents lock poisoned")
            .insert(agent_name.to_string(), conversation_id.to_string());
        self.register_agent_address(agent_name);
        self.register_agent_address(conversation_id);

        if let Some(task_id) = self
            .agent_tasks
            .read()
            .expect("agent task lock poisoned")
            .get(agent_name)
            .cloned()
        {
            self.register_conversation_task(conversation_id, &task_id);
        }
    }

    pub fn resolve_agent_address(&self, address: &str) -> Option<String> {
        if address.is_empty() {
            return None;
        }
        if let Some(conversation_id) = self
            .launched_agents
            .read()
            .expect("launched agents lock poisoned")
            .get(address)
            .cloned()
        {
            return Some(
                self.conversation_tasks
                    .read()
                    .expect("conversation task lock poisoned")
                    .get(&conversation_id)
                    .cloned()
                    .unwrap_or(conversation_id),
            );
        }
        if let Some(task_id) = self
            .agent_tasks
            .read()
            .expect("agent task lock poisoned")
            .get(address)
            .cloned()
        {
            return Some(task_id);
        }
        if let Some(task_id) = self
            .conversation_tasks
            .read()
            .expect("conversation task lock poisoned")
            .get(address)
            .cloned()
        {
            return Some(task_id);
        }
        let is_known_id = self
            .agent_tasks
                .read()
                .expect("agent task lock poisoned")
                .values()
                .any(|task_id| task_id == address)
            || self
                .launched_agents
                .read()
                .expect("launched agents lock poisoned")
                .values()
                .any(|conversation_id| conversation_id == address);
        let is_registered = self
            .known_agent_addresses
            .read()
            .expect("known agent addresses lock poisoned")
            .contains(address);
        (is_known_id || is_registered).then(|| address.to_string())
    }

    pub fn register_agent_address(&self, address: &str) {
        if is_safe_cache_key(address) {
            self.known_agent_addresses
                .write()
                .expect("known agent addresses lock poisoned")
                .insert(address.to_string());
        }
    }

    pub fn mark_agent_message_sent(&self, conversation_id: &str) -> u64 {
        let version = self.next_message_version.fetch_add(1, Ordering::Relaxed);
        self.required_conversation_versions
            .write()
            .expect("conversation versions lock poisoned")
            .insert(conversation_id.to_string(), version);
        version
    }

    pub fn conversation_cache_path(&self, conversation_id: &str) -> Option<std::path::PathBuf> {
        self.conversation_cache_target(conversation_id)
            .map(|(path, _)| path)
    }

    pub fn conversation_cache_target(
        &self,
        conversation_id: &str,
    ) -> Option<(std::path::PathBuf, u64)> {
        if !is_safe_cache_key(conversation_id) {
            return None;
        }
        let has_task = self
            .conversation_tasks
            .read()
            .expect("conversation task lock poisoned")
            .contains_key(conversation_id);
        let is_launched = self
            .launched_agents
            .read()
            .expect("launched agents lock poisoned")
            .values()
            .any(|launched_id| launched_id == conversation_id);
        (has_task || is_launched).then(|| {
            let required_version = self
                .required_conversation_versions
                .read()
                .expect("conversation versions lock poisoned")
                .get(conversation_id)
                .copied()
                .unwrap_or_default();
            (
                self.conversation_cache_dir
                    .join(format!("{conversation_id}.json")),
                required_version,
            )
        })
    }

    /// Load persisted conversation history for a task.
    pub fn load_conversation(&self, task_id: &str) -> Vec<serde_json::Value> {
        if !is_safe_cache_key(task_id) {
            tracing::warn!(task_id, "rejecting unsafe conversation cache key");
            return Vec::new();
        }
        let path = self.conversation_cache_dir.join(format!("{task_id}.json"));
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Save conversation history for a task to disk.
    pub fn save_conversation(&self, task_id: &str, messages: &[serde_json::Value]) {
        if !is_safe_cache_key(task_id) {
            tracing::warn!(task_id, "rejecting unsafe conversation cache key");
            return;
        }
        let path = self.conversation_cache_dir.join(format!("{task_id}.json"));
        if let Ok(data) = serde_json::to_string(messages) {
            std::fs::write(&path, &data).ok();
            let aliases = self
                .conversation_tasks
                .read()
                .expect("conversation task lock poisoned")
                .iter()
                .filter_map(|(conversation_id, mapped_task_id)| {
                    (mapped_task_id == task_id).then_some(conversation_id.clone())
                })
                .collect::<Vec<_>>();
            if conversation_is_terminal(messages) {
                for conversation_id in aliases {
                    let alias_path = self
                        .conversation_cache_dir
                        .join(format!("{conversation_id}.json"));
                    std::fs::write(&alias_path, &data).ok();
                    self.write_alias_version(&conversation_id, &alias_path);
                }
            }
        }
    }

    fn write_alias_version(&self, conversation_id: &str, alias_path: &std::path::Path) {
        let version = self
            .required_conversation_versions
            .read()
            .expect("conversation versions lock poisoned")
            .get(conversation_id)
            .copied()
            .unwrap_or_default();
        std::fs::write(alias_version_path(alias_path), version.to_string()).ok();
    }
}

fn alias_version_path(alias_path: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.version", alias_path.to_string_lossy()))
}

fn conversation_is_terminal(messages: &[serde_json::Value]) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    last.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
        && last
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|content| !content.trim().is_empty())
        && last
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|calls| calls.is_empty())
}

fn is_safe_cache_key(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_likely_embedding_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    normalized.contains("embedding") || normalized.contains("embed-text")
}

/// Accept WebSocket upgrades on `/graphql/v2` and idle silently.
/// The Warp client opens a WS for real-time cloud object sync; we accept
/// the connection so it stops retrying with errors every 30s.
async fn ws_idle(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.protocols(["graphql-transport-ws"])
        .on_upgrade(|mut socket: WebSocket| async move {
            // Just wait until the client closes the connection
            while socket.recv().await.is_some() {}
        })
}

/// Builds the route table. Exposed as a free function so tests can drive it
/// without binding a real socket.
pub fn router(state: AppState) -> Router {
    let shared = Arc::new(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/graphql/v2", post(handlers::graphql::handle).get(ws_idle))
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
        // Local implementations of the agent REST surfaces used by run
        // restoration, orchestration event delivery, and agent messaging.
        .route(
            "/api/v1/agent/events/stream",
            get(handlers::agent_api::stream_events),
        )
        .route(
            "/api/v1/agent/events/{run_id}",
            post(handlers::agent_api::report_event),
        )
        .route(
            "/api/v1/agent/runs",
            get(handlers::agent_api::list_runs),
        )
        .route(
            "/api/v1/agent/runs/{run_id}",
            get(handlers::agent_api::get_run),
        )
        .route(
            "/api/v1/agent/runs/{run_id}/event-sequence",
            axum::routing::patch(handlers::agent_api::acknowledge),
        )
        .route(
            "/api/v1/agent/runs/{run_id}/client-events",
            post(handlers::agent_api::acknowledge),
        )
        .route(
            "/api/v1/agent/runs/{run_id}/followups",
            post(handlers::agent_api::acknowledge),
        )
        .route(
            "/api/v1/agent/messages",
            post(handlers::agent_api::send_messages),
        )
        .route(
            "/api/v1/agent/messages/{run_id}",
            get(handlers::agent_api::list_messages),
        )
        .route(
            "/api/v1/agent/messages/{message_id}/delivered",
            post(handlers::agent_api::mark_message_delivered),
        )
        .route(
            "/api/v1/agent/messages/{message_id}/read",
            post(handlers::agent_api::read_message),
        )
        .route(
            "/api/v1/agent/identities",
            get(handlers::agent_api::list_identities),
        )
        .route(
            "/api/v1/agent/connected-self-hosted-workers",
            get(handlers::agent_api::list_connected_workers),
        )
        .route(
            "/api/v1/agent/tasks/{task_id}/cancel",
            post(handlers::agent_api::cancel_task),
        )
        .route("/api/v1/mcp/factory", any(handlers::mcp_factory::handle))
        // Remaining cloud-only REST operations return an explicit error.
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

    let models_url_display = config.models_url().unwrap_or_else(|| "(azure: default model only)".into());
    tracing::info!(url = %models_url_display, "fetching backend model list");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthStyle;

    fn config(default_model: &str) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            backend_base_url: "http://127.0.0.1:3113/v1".into(),
            backend_auth_style: AuthStyle::Bearer,
            backend_api_key: None,
            azure_api_version: None,
            default_model: default_model.into(),
        }
    }

    #[test]
    fn configured_model_is_authoritative_and_embeddings_are_filtered() {
        let state = AppState::new(
            config("configured-chat-model"),
            vec![
                "text-embedding-model".into(),
                "discovered-chat-model".into(),
            ],
        );

        assert_eq!(
            state.advertised_models(),
            vec!["configured-chat-model", "discovered-chat-model"]
        );
        assert_eq!(state.default_model_id(), "configured-chat-model");
    }

    #[test]
    fn launched_agent_ids_resolve_to_child_cache_files() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        let task_id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();

        state.register_launched_agent("child", &conversation_id);
        state.register_agent_task("child", &task_id);
        state.save_conversation(
            &task_id,
            &[serde_json::json!({"role": "assistant", "content": "CHILD_OK"})],
        );

        let path = state
            .conversation_cache_path(&conversation_id)
            .expect("launched conversation should resolve to its task cache");
        assert!(path.is_file());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(alias_version_path(
            &state
                .conversation_cache_dir
                .join(format!("{conversation_id}.json")),
        ))
        .unwrap();
        std::fs::remove_file(
            state
                .conversation_cache_dir
                .join(format!("{task_id}.json")),
        )
        .unwrap();
    }

    #[test]
    fn unsafe_cache_keys_cannot_escape_the_conversation_directory() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        state.register_conversation_task("../../outside", "safe-task");
        state.register_conversation_task("safe-conversation", "../outside");
        assert!(state.conversation_cache_path("../../outside").is_none());
        assert!(state
            .conversation_cache_path("safe-conversation")
            .is_none());
    }

    #[test]
    fn agent_addresses_resolve_only_registered_agents() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        state.register_agent_task("known-child", "known-task-id");
        state.register_launched_agent("launched-child", "known-conversation-id");

        assert_eq!(
            state.resolve_agent_address("known-child").as_deref(),
            Some("known-task-id")
        );
        assert_eq!(
            state.resolve_agent_address("launched-child").as_deref(),
            Some("known-conversation-id")
        );

        state.register_agent_task("launched-child", "launched-task-id");
        assert_eq!(
            state.resolve_agent_address("launched-child").as_deref(),
            Some("launched-task-id")
        );
        assert_eq!(
            state
                .resolve_agent_address("known-conversation-id")
                .as_deref(),
            Some("launched-task-id")
        );
        assert!(state.resolve_agent_address("missing-child").is_none());
    }

    #[test]
    fn explicit_parent_run_ids_are_valid_message_addresses() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        state.register_agent_address("parent-run-id");
        assert_eq!(
            state.resolve_agent_address("parent-run-id").as_deref(),
            Some("parent-run-id")
        );
    }

    #[test]
    fn child_alias_is_published_only_after_terminal_assistant_output() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        let task_id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();
        state.register_conversation_task(&conversation_id, &task_id);

        state.save_conversation(
            &task_id,
            &[serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call-1"}]
            })],
        );
        let alias = state.conversation_cache_path(&conversation_id).unwrap();
        assert!(!alias.exists());

        state.save_conversation(
            &task_id,
            &[serde_json::json!({"role": "assistant", "content": "done"})],
        );
        assert!(alias.exists());

        std::fs::remove_file(alias).unwrap();
        std::fs::remove_file(alias_version_path(
            &state
                .conversation_cache_dir
                .join(format!("{conversation_id}.json")),
        ))
        .unwrap();
        std::fs::remove_file(
            state
                .conversation_cache_dir
                .join(format!("{task_id}.json")),
        )
        .unwrap();
    }

    #[test]
    fn followup_message_requires_a_new_alias_version() {
        let state = AppState::new(config("configured-chat-model"), vec![]);
        let task_id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();
        state.register_conversation_task(&conversation_id, &task_id);
        state.save_conversation(
            &task_id,
            &[serde_json::json!({"role": "assistant", "content": "first"})],
        );
        let (alias, initial_required) = state
            .conversation_cache_target(&conversation_id)
            .unwrap();
        assert_eq!(initial_required, 0);
        assert_eq!(
            std::fs::read_to_string(alias_version_path(&alias)).unwrap(),
            "0"
        );

        let followup_version = state.mark_agent_message_sent(&conversation_id);
        assert!(followup_version > initial_required);
        state.save_conversation(
            &task_id,
            &[serde_json::json!({"role": "assistant", "content": "followup"})],
        );
        assert_eq!(
            std::fs::read_to_string(alias_version_path(&alias)).unwrap(),
            followup_version.to_string()
        );

        std::fs::remove_file(&alias).unwrap();
        std::fs::remove_file(alias_version_path(&alias)).unwrap();
        std::fs::remove_file(
            state
                .conversation_cache_dir
                .join(format!("{task_id}.json")),
        )
        .unwrap();
    }
}
