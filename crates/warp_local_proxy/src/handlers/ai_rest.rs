//! REST handler for the `/ai/generate_code_review_content` endpoint.
//!
//! Upstream the client POSTs JSON describing a PR (title, body, commit
//! messages...). For local mode we treat it as a chat completion request and
//! return a single string the client formats into PR title / body / commit.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppState;
use crate::upstream::openai::{chat_completion, ChatMessage};

#[derive(Debug, Deserialize)]
pub struct CodeReviewRequest {
    /// Free-form prompt the upstream API uses; just forward it. Subset of
    /// the real upstream shape; cynic types tolerate extra incoming fields.
    #[serde(default)]
    pub output_type: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, Value>,
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Value>,
) -> impl IntoResponse {
    let prompt = req
        .get("prompt")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let output_type = req
        .get("output_type")
        .and_then(|s| s.as_str())
        .unwrap_or("PR_BODY");

    tracing::info!(output_type, "code review request received");

    let system = format!(
        "You are generating PR copy for a code review. Output type: {output_type}. \
         Keep PR titles under 70 characters. Keep commit messages imperative. \
         Output the requested content directly with no markdown fences."
    );

    let messages = vec![
        ChatMessage {
            role: "system",
            content: system,
        },
        ChatMessage {
            role: "user",
            content: prompt,
        },
    ];

    match chat_completion(&state.http, &state.config, messages, false, Some(1024)).await {
        Ok(text) => (StatusCode::OK, Json(json!({"content": text}))).into_response(),
        Err(err) => {
            tracing::error!(?err, "backend failed on code review");
            let body = json!({
                "error": {
                    "code": "LOCAL_PROXY_BACKEND_ERROR",
                    "message": err.to_string(),
                }
            });
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}
