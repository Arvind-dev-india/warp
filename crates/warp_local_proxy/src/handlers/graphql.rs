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

/// Normalise a GraphQL operation name so that every variant the client might
/// send (PascalCase, camelCase, snake_case, or with a `get`/`Get` prefix) maps
/// to the same canonical key.  The canonical form is **lowercase** with
/// underscores stripped — e.g. `"getFreeAvailableModels"`, `"FreeAvailableModels"`,
/// and `"free_available_models"` all become `"freeavailablemodels"`.
fn canonical_op(raw: &str) -> String {
    let s = raw.replace('_', "").to_ascii_lowercase();
    // Strip leading "get" when the remainder starts with an uppercase concept.
    // This handles cynic's `getFreeAvailableModels` → `freeavailablemodels` while
    // preserving ops whose name *is* just "get…" (e.g. "GetUser" → "user").
    s.strip_prefix("get").unwrap_or(&s).to_owned()
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GraphqlRequest>,
) -> impl IntoResponse {
    let op = req.operation_name.as_deref().unwrap_or("<unnamed>");
    tracing::info!(operation = op, "graphql request received");

    // Match on a normalised key so PascalCase / camelCase / snake_case /
    // get-prefixed variants all resolve to the same handler.
    let key = canonical_op(op);

    let data: Option<Value> = match key.as_str() {
        // ---- canned identity / settings / models / experiments ----
        "createanonymoususer" => Some(canned::create_anonymous_user()),
        "user" => Some(canned::get_user(&state)),
        "usersettings" => Some(canned::get_user_settings()),
        "workspacesmetadataforuser" => Some(canned::get_workspaces_metadata_for_user(&state)),
        "featuremodelchoices" => Some(canned::get_feature_model_choices(&state)),
        "freeavailablemodels" => Some(canned::free_available_models(&state)),
        "requestlimitinfo" => Some(canned::get_request_limit_info()),
        "experiments" => Some(canned::get_experiments()),
        "referralinfo" => Some(canned::get_referral_info()),
        "usergithubinfo" => Some(canned::user_github_info()),
        "conversationusage" => Some(canned::get_conversation_usage()),
        // Triggered by the "Sign in to use AI" button in Settings.
        "mintcustomtoken" => Some(canned::mint_custom_token()),

        // ---- real AI handlers ----
        "generatecommands" => {
            Some(ai::generate_commands(&state.http, &state.config, &req.variables).await)
        }
        "generatedialogue" => {
            Some(ai::generate_dialogue(&state.http, &state.config, &req.variables).await)
        }

        // ---- cloud polling stubs (return empty/Ok rather than Err) ----
        "listambientagenttasks" => Some(cloud_stubs::list_ambient_agent_tasks()),
        "listaiconversations" | "listaiconversationmetadata" => {
            Some(cloud_stubs::list_ai_conversation_metadata())
        }
        "updateeventsequence" | "updateeventsequenceonserver" => {
            Some(cloud_stubs::update_event_sequence_on_server())
        }
        // Observed during real `warp-oss login` integration runs.
        "updateusersettings" => Some(cloud_stubs::update_user_settings()),
        "updatedcloudobjects" => Some(cloud_stubs::get_updated_cloud_objects()),
        // Observed when the GUI launched.
        "bulkcreateobjects" => Some(cloud_stubs::bulk_create_objects(&req.variables)),
        "creategenericstringobject" => {
            Some(cloud_stubs::create_generic_string_object(&req.variables))
        }
        "cloudenvironmentsquery" => Some(cloud_stubs::get_cloud_environments()),
        "availableharnesses" => Some(cloud_stubs::get_available_harnesses()),
        "updateagenttask" => Some(cloud_stubs::update_agent_task()),
        "deleteaiconversation" | "deleteconversation" => {
            Some(cloud_stubs::delete_ai_conversation(&req.variables))
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
