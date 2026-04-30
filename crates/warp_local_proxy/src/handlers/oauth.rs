//! OAuth2 device-flow stubs: `POST /api/v1/oauth/device/auth` and
//! `POST /api/v1/oauth/device/token`.
//!
//! The Warp client uses the standard `oauth2` Rust crate to drive RFC 8628
//! device-authorization grants against `{server_root_url}/api/v1/oauth/...`.
//! In local-proxy mode there's no real authorization server; we just hand the
//! client back canned responses so `warp-oss login` completes immediately and
//! the binary caches a fake auth state. Subsequent CLI commands then bypass
//! the "You are not logged in" short-circuit and start issuing real GraphQL
//! requests against this proxy.
//!
//! Body shape: the `oauth2` crate sends `application/x-www-form-urlencoded`
//! (per RFC 6749 / 8628). axum's `Form` extractor handles that.

use std::time::SystemTime;

use axum::{response::IntoResponse, Form, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct DeviceAuthRequest {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// `POST /api/v1/oauth/device/auth` — returns a canned device authorization
/// response. The user_code is shown to the human in real OAuth2 device flows;
/// here it's purely cosmetic since the proxy auto-approves the next /token call.
pub async fn device_auth(
    Form(req): Form<DeviceAuthRequest>,
) -> impl IntoResponse {
    tracing::info!(
        client_id = req.client_id.as_deref().unwrap_or(""),
        scope = req.scope.as_deref().unwrap_or(""),
        "oauth device auth requested"
    );

    Json(DeviceAuthResponse {
        device_code: "local-mode-device-code".into(),
        user_code: "LOCAL-MODE".into(),
        // verification_uri points at the proxy's healthz so a curious user
        // who copies the URL gets a benign 200 OK rather than a connection
        // refused.
        verification_uri: "http://127.0.0.1:8765/healthz".into(),
        verification_uri_complete: "http://127.0.0.1:8765/healthz".into(),
        expires_in: 600,
        interval: 1,
    })
}

#[derive(Debug, Deserialize)]
pub struct DeviceTokenRequest {
    #[serde(default)]
    pub grant_type: Option<String>,
    #[serde(default)]
    pub device_code: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
}

/// `POST /api/v1/oauth/device/token` — auto-approves the device code and
/// returns a canned access token. Real OAuth2 device flows poll this until
/// the user authorizes; we short-circuit because there's no human in the loop.
///
/// The token shape is OAuth2-compatible. The Warp client wraps it as a
/// `FirebaseToken` and passes it as `Authorization: Bearer ...` on subsequent
/// GraphQL calls — those land back here on `POST /graphql/v2` and we don't
/// inspect the bearer.
pub async fn device_token(
    Form(req): Form<DeviceTokenRequest>,
) -> impl IntoResponse {
    tracing::info!(
        grant_type = req.grant_type.as_deref().unwrap_or(""),
        device_code = req.device_code.as_deref().unwrap_or(""),
        "oauth device token requested"
    );

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let body: Value = json!({
        "access_token": fake_jwt(now),
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "openid profile email",
    });
    Json(body)
}

/// Builds a structurally-valid (but unsigned, untrusted) JWT so any client
/// code that decodes the access token to extract claims gets sensible values
/// instead of a parse error. We do NOT sign this — the client must not be
/// verifying signatures in local-proxy mode. If it does, a follow-up fork
/// edit will need to bypass that verification.
fn fake_jwt(issued_at_unix: u64) -> String {
    // header: {"alg":"none","typ":"JWT"}
    let header_b64 = base64url("{\"alg\":\"none\",\"typ\":\"JWT\"}");
    let payload = format!(
        "{{\"iss\":\"warp_local_proxy\",\"sub\":\"local-user-uid\",\"aud\":\"warp-cli\",\"iat\":{iat},\"exp\":{exp},\"email\":\"local@local\",\"firebase\":{{\"identities\":{{}},\"sign_in_provider\":\"anonymous\"}}}}",
        iat = issued_at_unix,
        exp = issued_at_unix + 3600,
    );
    let payload_b64 = base64url(&payload);
    format!("{header_b64}.{payload_b64}.")
}

/// Minimal base64url-without-padding encoder so we don't pull in a new
/// dependency just for this stub.
fn base64url(input: &str) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        out.push(ALPH[(b0 >> 2) as usize] as char);
        out.push(ALPH[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(ALPH[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(ALPH[(b2 & 0x3F) as usize] as char);
        }
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_roundtrip_shape() {
        // Sanity: encode known input, ensure no padding chars and only
        // base64url alphabet characters appear.
        let out = base64url("hi");
        for ch in out.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "unexpected char: {ch}"
            );
        }
        assert!(!out.contains('='));
    }

    #[test]
    fn fake_jwt_has_three_segments() {
        let token = fake_jwt(1_700_000_000);
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "expected header.payload.signature");
        assert!(parts[0].len() > 0);
        assert!(parts[1].len() > 0);
        // Signature segment is empty since alg=none.
        assert_eq!(parts[2], "");
    }
}
