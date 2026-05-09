//! OAuth2 device-flow stubs: `POST /api/v1/oauth/device/auth` and
//! `POST /api/v1/oauth/token`, plus Firebase-fallback proxy endpoints
//! `POST /proxy/customToken` and `POST /proxy/token`.
//!
//! ## Login flow on this fork
//!
//! 1. Client calls `POST /api/v1/oauth/device/auth` → we return a canned
//!    RFC 8628 device authorization response.
//! 2. Client polls `POST /api/v1/oauth/token` → we auto-approve and return a
//!    fake OAuth2 access token (an `alg=none` JWT with Firebase-shaped
//!    claims). The Warp client wraps this as a `FirebaseToken::Custom`.
//! 3. Client tries to exchange the custom token at Firebase
//!    `identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken`.
//!    With `firebase_auth_api_key="local-mode-no-firebase"` Firebase rejects
//!    the call (the only outbound leak in the current login flow — see
//!    `app/src/server/server_api/auth.rs::fetch_access_tokens_for_firebase_token`
//!    which retries via the warp-server proxy on Firebase failure).
//! 4. Client falls back to `POST /proxy/customToken?key=...` — that's us.
//!    We return `{id_token, refresh_token, expires_in}` with another fake JWT.
//! 5. Client uses `id_token` as Bearer auth for subsequent GraphQL calls,
//!    which land on our `/graphql/v2` and are served from the canned ops.
//! 6. When the id_token expires, client calls `POST /proxy/token` for refresh.
//!    Same response shape.

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
pub async fn device_auth(Form(req): Form<DeviceAuthRequest>) -> impl IntoResponse {
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

/// `POST /api/v1/oauth/token` — auto-approves the device code and returns a
/// canned access token. Real OAuth2 device flows poll this until the user
/// authorizes; we short-circuit because there's no human in the loop.
///
/// The token shape is OAuth2-compatible. The Warp client wraps the access
/// token as a `FirebaseToken::Custom` and then exchanges it against
/// Firebase / our /proxy/customToken endpoint to get an `id_token` it can
/// use as Bearer auth for GraphQL.
pub async fn device_token(Form(req): Form<DeviceTokenRequest>) -> impl IntoResponse {
    tracing::info!(
        grant_type = req.grant_type.as_deref().unwrap_or(""),
        device_code = req.device_code.as_deref().unwrap_or(""),
        "oauth device token requested"
    );

    let now = unix_now();
    let body: Value = json!({
        "access_token": fake_jwt(now),
        // RFC 6749 says token_type is case-insensitive but the upstream
        // `oauth2` crate's BasicTokenType enum uses serde rename_all=snake_case,
        // so it deserializes the Bearer variant from the lowercase string.
        "token_type": "bearer",
        "expires_in": 3600,
        "scope": "openid profile email",
    });
    Json(body)
}

#[derive(Debug, Deserialize)]
pub struct CustomTokenProxyRequest {
    /// Sent by the Warp client as `token` (the access_token from the OAuth2
    /// step, wrapped as a Firebase custom token).
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default, rename = "returnSecureToken")]
    pub return_secure_token: Option<String>,
}

/// `POST /proxy/customToken` — Firebase fallback. The Warp client first tries
/// `identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken`; when
/// that fails (it does, our firebase_auth_api_key is bogus) the client
/// retries against this endpoint expecting a Firebase-shaped response.
///
/// Response shape comes from `crates/firebase/src/lib.rs::FetchAccessTokenResponse`:
/// `{id_token, refresh_token, expires_in}` (snake_case or camelCase aliases),
/// all strings.
pub async fn proxy_custom_token(Form(req): Form<CustomTokenProxyRequest>) -> impl IntoResponse {
    tracing::info!(
        has_token = req.token.is_some(),
        "proxy custom token exchange"
    );
    Json(firebase_token_response())
}

#[derive(Debug, Deserialize)]
pub struct RefreshTokenProxyRequest {
    #[serde(default)]
    pub grant_type: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// `POST /proxy/token` — Firebase refresh fallback. Same response shape as
/// `/proxy/customToken`. Called when an id_token expires and the client
/// wants a fresh one.
pub async fn proxy_refresh_token(Form(req): Form<RefreshTokenProxyRequest>) -> impl IntoResponse {
    tracing::info!(
        grant_type = req.grant_type.as_deref().unwrap_or(""),
        "proxy refresh token exchange"
    );
    Json(firebase_token_response())
}

fn firebase_token_response() -> Value {
    let now = unix_now();
    json!({
        "id_token": fake_jwt(now),
        "refresh_token": format!("local-refresh-{now}"),
        "expires_in": "3600",
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
        assert_eq!(parts[2], "");
    }

    #[test]
    fn firebase_token_response_has_required_fields() {
        let v = firebase_token_response();
        assert!(v.get("id_token").and_then(|v| v.as_str()).is_some());
        assert!(v.get("refresh_token").and_then(|v| v.as_str()).is_some());
        assert!(v.get("expires_in").and_then(|v| v.as_str()).is_some());
    }
}
