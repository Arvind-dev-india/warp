use axum::body::Bytes;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

const MCP_SESSION_ID: &str = "warp-local-factory";

pub async fn handle(method: Method, body: Bytes) -> Response {
    match method {
        Method::POST => handle_post(&body),
        Method::DELETE => StatusCode::NO_CONTENT.into_response(),
        Method::GET => (
            StatusCode::METHOD_NOT_ALLOWED,
            [("allow", "POST, DELETE")],
            "Factory MCP does not expose a standalone SSE subscription.",
        )
            .into_response(),
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn handle_post(body: &[u8]) -> Response {
    let request: Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": error.to_string() }
                }),
            );
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }

    let result = match method {
        "initialize" => json!({
            "protocolVersion": request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "warp-local-factory",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
        "tools/list" => json!({ "tools": [] }),
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        "ping" => json!({}),
        _ => {
            return json_response(
                StatusCode::OK,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                }),
            );
        }
    };

    json_response(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::HeaderName::from_static("mcp-session-id"),
        HeaderValue::from_static(MCP_SESSION_ID),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_returns_server_capabilities() {
        let response = handle_post(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["mcp-session-id"],
            HeaderValue::from_static(MCP_SESSION_ID)
        );
    }

    #[test]
    fn notifications_are_accepted_without_a_response_body() {
        let response = handle_post(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
}
