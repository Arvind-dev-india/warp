//! Fork-only: environment-variable overrides for outbound telemetry endpoints.
//!
//! Upstream channels bake telemetry endpoints (RudderStack URL/keys, Sentry DSN)
//! into the channel config and provide no runtime knob to redirect or suppress
//! them. This module adds an additive layer of env-var overrides that
//! [`crate::channel::ChannelState`] consults from a small number of getters.
//!
//! When every variable is unset, behavior is bit-for-bit identical to upstream.
//!
//! Recognised variables:
//!
//! | Variable                          | Effect when set                                                  |
//! |-----------------------------------|------------------------------------------------------------------|
//! | `WARP_DISABLE_TELEMETRY`          | Kill switch: empty RudderStack/Sentry destinations, hidden UI.   |
//! | `WARP_RUDDERSTACK_URL`            | Replace the RudderStack root URL (both UGC and non-UGC streams). |
//! | `WARP_RUDDERSTACK_WRITE_KEY`      | Replace the non-UGC RudderStack write key.                       |
//! | `WARP_RUDDERSTACK_UGC_WRITE_KEY`  | Replace the UGC RudderStack write key.                           |
//! | `WARP_SENTRY_DSN`                 | Replace the Sentry DSN.                                          |
//!
//! Values are read once at process start (via [`lazy_static`]).

use std::borrow::Cow;
use std::env;

use lazy_static::lazy_static;

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
/// Lets users disable the kill-switch in ad-hoc env files via
/// `WARP_DISABLE_TELEMETRY=0` rather than `unset`.
fn parse_truthy(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

lazy_static! {
    static ref DISABLE_TELEMETRY: bool = env::var(ENV_DISABLE_TELEMETRY)
        .ok()
        .is_some_and(|v| parse_truthy(&v));
    static ref RUDDERSTACK_URL: Option<String> = read_trimmed(ENV_RUDDERSTACK_URL);
    static ref RUDDERSTACK_NON_UGC_WRITE_KEY: Option<String> =
        read_trimmed(ENV_RUDDERSTACK_NON_UGC_WRITE_KEY);
    static ref RUDDERSTACK_UGC_WRITE_KEY: Option<String> =
        read_trimmed(ENV_RUDDERSTACK_UGC_WRITE_KEY);
    static ref SENTRY_DSN: Option<String> = read_trimmed(ENV_SENTRY_DSN);
}

/// Whether `WARP_DISABLE_TELEMETRY` is set to a truthy value.
pub fn telemetry_disabled() -> bool {
    *DISABLE_TELEMETRY
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
}
