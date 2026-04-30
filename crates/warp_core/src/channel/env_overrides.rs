//! Fork-only: environment-variable controls for outbound telemetry.
//!
//! ## Default policy in this fork
//!
//! **Telemetry is OFF by default.** Upstream Warp ships RudderStack analytics
//! and Sentry crash reporting enabled-by-default (gated only by a privacy
//! toggle that defaults to `true`). This fork inverts that: nothing is sent
//! unless the user explicitly opts in.
//!
//! Opt in by setting `WARP_ENABLE_TELEMETRY` to a truthy value
//! (`1`, `true`, `yes`, `on` — case-insensitive). When opt-in is in effect,
//! the channel-baked endpoints (or the redirect env vars below) are used
//! and the existing privacy UI / settings continue to work as upstream.
//!
//! ## Recognised variables
//!
//! | Variable                          | Effect when set                                                                                      |
//! |-----------------------------------|------------------------------------------------------------------------------------------------------|
//! | `WARP_ENABLE_TELEMETRY`           | Opt back in to telemetry. Required in this fork; default is OFF.                                     |
//! | `WARP_DISABLE_TELEMETRY`          | Force telemetry off (overrides opt-in). Useful for one-shot CI / sandbox runs.                       |
//! | `WARP_RUDDERSTACK_URL`            | Replace the RudderStack root URL (applies to both UGC and non-UGC streams).                          |
//! | `WARP_RUDDERSTACK_WRITE_KEY`      | Replace the non-UGC RudderStack write key.                                                           |
//! | `WARP_RUDDERSTACK_UGC_WRITE_KEY`  | Replace the UGC RudderStack write key.                                                               |
//! | `WARP_SENTRY_DSN`                 | Replace the Sentry DSN.                                                                              |
//!
//! Values are read once at process start (via [`lazy_static`]).

use std::borrow::Cow;
use std::env;

use lazy_static::lazy_static;

const ENV_ENABLE_TELEMETRY: &str = "WARP_ENABLE_TELEMETRY";
const ENV_DISABLE_TELEMETRY: &str = "WARP_DISABLE_TELEMETRY";
const ENV_RUDDERSTACK_URL: &str = "WARP_RUDDERSTACK_URL";
const ENV_RUDDERSTACK_NON_UGC_WRITE_KEY: &str = "WARP_RUDDERSTACK_WRITE_KEY";
const ENV_RUDDERSTACK_UGC_WRITE_KEY: &str = "WARP_RUDDERSTACK_UGC_WRITE_KEY";
const ENV_SENTRY_DSN: &str = "WARP_SENTRY_DSN";

fn read_trimmed(name: &str) -> Option<String> {
    let raw = env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Treats common "off" sentinels (`""`, `"0"`, `"false"`, `"no"`, `"off"`,
/// case-insensitive, whitespace-trimmed) as `false`; everything else as `true`.
/// Lets users override either env-var policy in ad-hoc env files via
/// `=0` rather than `unset`.
fn parse_truthy(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

lazy_static! {
    static ref ENABLE_TELEMETRY: bool = env::var(ENV_ENABLE_TELEMETRY)
        .ok()
        .is_some_and(|v| parse_truthy(&v));
    static ref EXPLICIT_DISABLE_TELEMETRY: bool = env::var(ENV_DISABLE_TELEMETRY)
        .ok()
        .is_some_and(|v| parse_truthy(&v));
    static ref RUDDERSTACK_URL: Option<String> = read_trimmed(ENV_RUDDERSTACK_URL);
    static ref RUDDERSTACK_NON_UGC_WRITE_KEY: Option<String> =
        read_trimmed(ENV_RUDDERSTACK_NON_UGC_WRITE_KEY);
    static ref RUDDERSTACK_UGC_WRITE_KEY: Option<String> =
        read_trimmed(ENV_RUDDERSTACK_UGC_WRITE_KEY);
    static ref SENTRY_DSN: Option<String> = read_trimmed(ENV_SENTRY_DSN);
}

/// Returns `true` when telemetry is currently disabled in this fork.
///
/// Disabled when the user has not opted in via `WARP_ENABLE_TELEMETRY`,
/// or when they have explicitly opted out via `WARP_DISABLE_TELEMETRY`
/// (the explicit opt-out wins).
pub fn telemetry_disabled() -> bool {
    *EXPLICIT_DISABLE_TELEMETRY || !*ENABLE_TELEMETRY
}

pub fn rudderstack_url_override() -> Option<Cow<'static, str>> {
    RUDDERSTACK_URL.clone().map(Cow::Owned)
}

pub fn rudderstack_non_ugc_write_key_override() -> Option<Cow<'static, str>> {
    RUDDERSTACK_NON_UGC_WRITE_KEY.clone().map(Cow::Owned)
}

pub fn rudderstack_ugc_write_key_override() -> Option<Cow<'static, str>> {
    RUDDERSTACK_UGC_WRITE_KEY.clone().map(Cow::Owned)
}

pub fn sentry_url_override() -> Option<Cow<'static, str>> {
    SENTRY_DSN.clone().map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::parse_truthy;

    #[test]
    fn falsy_values() {
        for v in ["", "0", "false", "FALSE", "no", "No", "off", "OFF", "  off  "] {
            assert!(!parse_truthy(v), "expected `{v}` to be falsy");
        }
    }

    #[test]
    fn truthy_values() {
        for v in ["1", "true", "TRUE", "yes", "on", "anything-else", "redirect"] {
            assert!(parse_truthy(v), "expected `{v}` to be truthy");
        }
    }

    /// Pure-logic mirror of [`super::telemetry_disabled`] so we can verify the
    /// state machine without touching real process env vars (which would race
    /// other tests in the same process).
    fn computed(enable: bool, disable: bool) -> bool {
        disable || !enable
    }

    #[test]
    fn default_off_when_neither_set() {
        assert!(computed(false, false), "fork default must be off");
    }

    #[test]
    fn opt_in_enables() {
        assert!(!computed(true, false));
    }

    #[test]
    fn explicit_disable_wins_over_opt_in() {
        assert!(computed(true, true));
    }

    #[test]
    fn explicit_disable_alone() {
        assert!(computed(false, true));
    }
}
