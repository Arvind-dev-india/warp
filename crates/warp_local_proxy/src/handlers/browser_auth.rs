//! Browser-targeted login pages.
//!
//! When the user clicks "Login" or "Sign up" in the GUI, the warp client opens
//! the system browser at one of:
//!
//! * `{server_root_url}/login/remote?scheme=<scheme>&state=<uuid>`
//! * `{server_root_url}/signup/remote?scheme=<scheme>&state=<uuid>`
//! * `{server_root_url}/login_options/<custom_token>?state=<uuid>`  (after
//!   MintCustomToken succeeds — see app/src/auth/auth_manager.rs:680-703)
//!
//! The expected upstream behavior is: the user fills in a form, the page
//! authenticates them, and the page finally redirects to
//! `{scheme}://auth/desktop_redirect?refresh_token=<token>&state=<state>` —
//! a custom URL scheme that the OS routes back to the warp app, which then
//! extracts the tokens and finishes login.
//!
//! ## Local-proxy behavior
//!
//! In local mode there's no real auth. The user is already anonymous-signed-in
//! at the moment the login modal appears. We serve a small HTML page that:
//!
//! 1. Explains they're already signed in locally.
//! 2. Provides a manual "Continue in Warp" button that links to the deep-link
//!    URL. We deliberately do NOT auto-redirect, because:
//!    * Firefox+Linux without a registered URL-scheme handler will hang on
//!      auto-redirect (xdg-open finds no app, browser sits forever).
//!    * Forks built with `cargo` typically don't install a .desktop file with
//!      `MimeType=x-scheme-handler/warposs`, so the deep link won't route.
//! 3. Provides a manual "Cancel — already signed in" instruction so the user
//!    can dismiss the login modal in the warp UI without the browser dance.

use axum::{
    extract::{Path, Query},
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

/// `GET /login/remote` and `GET /signup/remote`.
pub async fn handle_remote(Query(q): Query<LoginQuery>) -> impl IntoResponse {
    render_page(q.scheme.as_deref(), q.state.as_deref())
}

/// `GET /login_options/{custom_token}` — fired after MintCustomToken.
pub async fn handle_login_options(
    Path(_custom_token): Path<String>,
    Query(q): Query<LoginQuery>,
) -> impl IntoResponse {
    render_page(q.scheme.as_deref(), q.state.as_deref())
}

fn render_page(scheme: Option<&str>, state: Option<&str>) -> impl IntoResponse {
    let scheme = scheme.unwrap_or("warposs");
    let state = state.unwrap_or("");
    let refresh_token = "local-refresh-token";

    let deep_link =
        format!("{scheme}://auth/desktop_redirect?refresh_token={refresh_token}&state={state}");

    tracing::info!(
        scheme = scheme,
        state = state,
        "browser auth landing page served"
    );

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
    max-width: 540px; padding: 32px; border: 1px solid #30363d;
    border-radius: 8px;
  }}
  h1 {{ margin: 0 0 16px; font-size: 20px; }}
  p {{ color: #8b949e; line-height: 1.55; margin: 0 0 12px; }}
  ol {{ color: #8b949e; line-height: 1.7; padding-left: 22px; }}
  .button {{
    display: inline-block; padding: 10px 20px; margin-top: 16px;
    background: #2ea043; color: white !important; border-radius: 6px;
    text-decoration: none; font-weight: 600;
  }}
  .button.secondary {{ background: #21262d; color: #c9d1d9 !important; margin-left: 8px; }}
  code {{ font-family: "SF Mono", Consolas, monospace; font-size: 12px; color: #58a6ff; }}
  .small {{ font-size: 12px; color: #6e7681; }}
</style>
</head>
<body>
<div class="card">
  <h1>You're already signed in locally.</h1>
  <p>warp_local_proxy is in front of <code>app.warp.dev</code>. There is no remote
     account in local mode — Warp is operating as a fully offline anonymous user.</p>

  <p style="margin-top: 24px;"><strong>Easiest path:</strong> close this tab and click <em>Cancel</em>
     on the login dialog in the Warp window. Your session will continue as the local user.</p>

  <p style="margin-top: 24px;"><strong>Or, if Warp's URL scheme is registered
     on this OS</strong> (true if you previously had upstream Warp installed
     via your package manager), the button below deep-links Warp with a
     synthetic refresh token:</p>

  <p>
    <a class="button" href="{deep_link}">Continue in Warp</a>
    <a class="button secondary" href="javascript:window.close()">Close tab</a>
  </p>

  <p class="small" style="margin-top: 24px;">
    Custom-scheme URL: <code>{deep_link}</code>
  </p>
</div>
</body>
</html>"#,
        deep_link = deep_link,
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}
