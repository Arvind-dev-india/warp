//! HTTP handlers for the proxy.
//!
//! Each handler corresponds to a route in [`crate::server::router`]. Right now
//! we have:
//!
//! * `healthz` — liveness check.
//! * [`graphql::handle`] — accepts the client's `POST /graphql/v2` requests and
//!   routes them by `operationName`. v0 returns a structured "not yet
//!   implemented" GraphQL error for every operation; later commits replace the
//!   default branch with real handlers, op-by-op.

pub mod graphql;

use axum::{http::StatusCode, response::IntoResponse, Json};

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}
