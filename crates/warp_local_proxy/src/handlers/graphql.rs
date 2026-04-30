//! Dispatcher for `POST /graphql/v2`.
//!
//! v0: parse the request, log the operation name, and reply with a structured
//! GraphQL `errors` payload telling the client the operation is not yet
//! implemented locally. Real per-operation handlers (canned identity / models /
//! AI inference) will replace the default branch in subsequent commits.

use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct GraphqlRequest {
    /// `operationName` per the GraphQL-over-HTTP spec. Optional but every
    /// real Warp client request sets it.
    #[serde(default, rename = "operationName")]
    pub operation_name: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub variables: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct GraphqlError {
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub extensions: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct GraphqlResponse<T: Serialize = serde_json::Value> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<GraphqlError>,
}

pub async fn handle(Json(req): Json<GraphqlRequest>) -> impl IntoResponse {
    let op = req.operation_name.as_deref().unwrap_or("<unnamed>");
    tracing::info!(operation = op, "graphql request received");

    let body = GraphqlResponse::<serde_json::Value> {
        data: None,
        errors: vec![GraphqlError {
            message: format!(
                "warp_local_proxy: GraphQL operation '{op}' is not implemented yet. \
                 Subsequent commits add per-operation handlers."
            ),
            extensions: serde_json::json!({
                "code": "LOCAL_PROXY_UNIMPLEMENTED",
                "operation": op,
            }),
        }],
    };

    (StatusCode::OK, Json(body))
}
