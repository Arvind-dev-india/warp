//! Browser-targeted login pages.
//!
//! When the user clicks "Login" or "Sign up" in the GUI, the warp client opens
//! the system browser at `{server_root_url}/login/remote?scheme=<scheme>&state=<uuid>`
//! (or `/signup/remote`). The expected upstream behavior is: the user fills in
//! a form, the page authenticates them, and the page finally redirects to
//! `{scheme}://auth/desktop_redirect?refresh_token=<token>&state=<state>` —
//! a custom URL scheme that the OS routes back to the warp app, which then
//! extracts the tokens and finishes login.
//!
//! In local-proxy mode there's no real auth — we want the user to land back in
//! the app immediately as the canned local user. So both endpoints serve a
//! tiny HTML page that:
//!
//! 1. Auto-redirects via `window.location.href` to the deep-link URL with a
//!    fake refresh token (which the app will exchange via `/proxy/token` —
//!    that handler is already in place).
//! 2. Provides a manual button as fallback for browsers that block
//!    custom-scheme redirects without explicit user interaction.

use axum::{
    extract::Query,
    http::{header, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// `GET /login/remote` and `GET /signup/remote` — both served by this handler.
pub async fn handle(Query(q): Query<LoginQuery>) -> impl IntoResponse {
    let scheme = q.scheme.as_deref().unwrap_or("warposs");
    let state = q.state.as_deref().unwrap_or("");
    let refresh_token = "local-refresh-token";

    // Build the deep-link URL the warp client expects to receive.
    // Format from app/src/auth/auth_view_modal.rs:111
    //   "{scheme}://auth/desktop_redirect?refresh_token={token}&state={state}"
    let deep_link = format!(
        "{scheme}://auth/desktop_redirect?refresh_token={refresh_token}&state={state}"
    );

    tracing::info!(scheme = scheme, state = state, "browser auth redirect");

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>warp_local_proxy — local sign-in</title>
<style>
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: #0d1117;
    color: #c9d1d9;
    display: flex; align-items: center; justify-content: center;
    height: 100vh; margin: 0;
  }}
  .card {{
    max-width: 480px; padding: 32px; border: 1px solid #30363d;
    border-radius: 8px; text-align: center;
  }}
  h1 {{ margin-top: 0; font-size: 20px; }}
  p {{ color: #8b949e; line-height: 1.5; }}
  a.button {{
    display: inline-block; padding: 10px 20px; margin-top: 16px;
    background: #2ea043; color: white; border-radius: 6px;
    text-decoration: none; font-weight: 600;
  }}
  code {{ font-family: "SF Mono", Consolas, monospace; font-size: 12px; color: #58a6ff; }}
</style>
</head>
<body>
<div class="card">
  <h1>Signing you in locally…</h1>
  <p>warp_local_proxy is in front of <code>app.warp.dev</code>; you'll be returned to the Warp client as the local user automatically.</p>
  <p>If nothing happens within a few seconds, click below:</p>
  <p><a class="button" id="link" href="{deep_link}">Open Warp</a></p>
  <p style="margin-top: 24px; font-size: 12px;">You can close this tab once Warp has refocused.</p>
</div>
<script>
  // Try the deep-link redirect immediately. Browsers vary in their
  // handling of custom-scheme redirects from JS; the manual button is
  // the fallback.
  setTimeout(function() {{
    window.location.href = {deep_link_json};
  }}, 100);
</script>
</body>
</html>"#,
        deep_link = deep_link,
        deep_link_json = serde_json::to_string(&deep_link).unwrap_or_else(|_| String::from("\"\"")),
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}
