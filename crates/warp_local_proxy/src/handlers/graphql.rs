//! Dispatcher for `POST /graphql/v2`.
//!
//! Parses the GraphQL request body, extracts `operationName`, and routes to:
//! - the canned handlers (identity / settings / workspaces / models / experiments),
//! - the AI handlers (generateCommands, generateDialogue),
//! - cloud-stub handlers (empty Ok for polling APIs we don't support),
//! - or returns a structured `LOCAL_PROXY_UNIMPLEMENTED` GraphQL error.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::operations::{ai, canned, cloud_stubs};
use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct GraphqlRequest {
    /// `operationName` per the GraphQL-over-HTTP spec.
    #[serde(default, rename = "operationName")]
    pub operation_name: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub variables: Value,
}

#[derive(Debug, Serialize)]
pub struct GraphqlError {
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub extensions: Value,
}

#[derive(Debug, Serialize)]
pub struct GraphqlResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<GraphqlError>,
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GraphqlRequest>,
) -> impl IntoResponse {
    let op = req.operation_name.as_deref().unwrap_or("<unnamed>");
    tracing::info!(operation = op, "graphql request received");

    let data: Option<Value> = match op {
        // ---- canned identity / settings / models / experiments ----
        "CreateAnonymousUser" => Some(canned::create_anonymous_user()),
        "GetUser" => Some(canned::get_user()),
        "GetUserSettings" => Some(canned::get_user_settings()),
        "GetWorkspacesMetadataForUser" => Some(canned::get_workspaces_metadata_for_user()),
        "GetFeatureModelChoices" => Some(canned::get_feature_model_choices()),
        "FreeAvailableModels" | "free_available_models" => Some(canned::free_available_models()),
        "GetRequestLimitInfo" | "get_request_limit_info" => Some(canned::get_request_limit_info()),
        "GetExperiments" => Some(canned::get_experiments()),

        // ---- real AI handlers ----
        "GenerateCommands" | "generate_commands" => {
            Some(ai::generate_commands(&state.http, &state.config, &req.variables).await)
        }
        "GenerateDialogue" | "generate_dialogue" => {
            Some(ai::generate_dialogue(&state.http, &state.config, &req.variables).await)
        }

        // ---- cloud polling stubs (return empty/Ok rather than Err) ----
        "ListAmbientAgentTasks" | "list_ambient_agent_tasks" => {
            Some(cloud_stubs::list_ambient_agent_tasks())
        }
        "ListAiConversations" | "list_ai_conversation_metadata" => {
            Some(cloud_stubs::list_ai_conversation_metadata())
        }
        "UpdateEventSequence" | "update_event_sequence_on_server" => {
            Some(cloud_stubs::update_event_sequence_on_server())
        }

        // Anything we haven't taught the proxy yet → structured error.
        _ => None,
    };

    let body = if let Some(data) = data {
        GraphqlResponse {
            data: Some(data),
            errors: vec![],
        }
    } else {
        GraphqlResponse {
            data: None,
            errors: vec![GraphqlError {
                message: format!(
                    "warp_local_proxy: GraphQL operation '{op}' is not implemented yet. Add a \
                     handler in crates/warp_local_proxy/src/operations/. The cynic Rust type at \
                     crates/graphql/src/api/{{queries|mutations}}/<op>.rs is the source of truth \
                     for the response shape."
                ),
                extensions: serde_json::json!({
                    "code": "LOCAL_PROXY_UNIMPLEMENTED",
                    "operation": op,
                }),
            }],
        }
    };

    (StatusCode::OK, Json(body))
}
