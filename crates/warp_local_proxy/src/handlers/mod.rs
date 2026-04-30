//! HTTP handlers for the proxy.
//!
//! Each handler corresponds to a route in [`crate::server::router`].
//!
//! * `healthz` — liveness check.
//! * [`graphql::handle`] — accepts the client's `POST /graphql/v2` requests and
//!   routes them by `operationName` to either canned, AI, or cloud-stub handlers.
//! * [`ai_rest::handle`] — `POST /ai/generate_code_review_content`.
//! * [`oauth::device_auth`] / [`oauth::device_token`] — RFC 8628 device-flow
//!   stubs so headless `warp-oss login` completes against the proxy and the
//!   binary caches a fake auth state.

pub mod ai_rest;
pub mod graphql;
pub mod oauth;

use axum::{http::StatusCode, response::IntoResponse, Json};

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

pub async fn unsupported() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {
                "code": "LOCAL_PROXY_UNSUPPORTED",
                "message": "This endpoint is not implemented in warp_local_proxy. Cloud-only operations (agent runs, attachments, etc.) are not available in local mode."
            }
        })),
    )
}
