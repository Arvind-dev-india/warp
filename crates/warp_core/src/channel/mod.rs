mod config;
// [FORK] Env-var overrides for telemetry endpoints. See env_overrides.rs.
mod env_overrides;
mod state;

use std::fmt;

pub use config::*;
pub use state::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The official/first-party stable release.
    Stable,
    /// The official/first-party feature preview release.
    Preview,

    /// The internal-only nightly build.
    Dev,
    /// The internal-only HEAD build.
    Local,

    /// The open-source build of Warp.
    Oss,

    /// The integration test build.
    Integration,
}

impl Channel {
    /// Whether or not this channel is for internal use only
    pub fn is_dogfood(&self) -> bool {
        match self {
            Channel::Dev | Channel::Local => true,
            Channel::Stable | Channel::Preview | Channel::Integration | Channel::Oss => false,
        }
    }

    /// Whether this channel honors the `--server-root-url` / `--ws-server-url` /
    /// `--session-sharing-server-url` flags (and their `WARP_*` env-var equivalents).
    ///
    /// Release channels (`Stable`, `Preview`) ignore these overrides so shipped
    /// builds can't be redirected away from their baked-in server URLs. Internal-only channels
    /// (`Dev`, `Local`, `Integration`) continue to honor them for local development and testing.
    ///
    /// **[FORK]** `Channel::Oss` is included so this fork's open-source binary can be pointed at
    /// `warp_local_proxy` (a fork-owned localhost proxy that handles AI / identity / settings ops
    /// without ever reaching `app.warp.dev`). Without this, `WARP_SERVER_ROOT_URL` is silently
    /// ignored by the OSS build.
    pub fn allows_server_url_overrides(&self) -> bool {
        match self {
            Channel::Dev | Channel::Local | Channel::Integration | Channel::Oss => true,
            Channel::Stable | Channel::Preview => false,
        }
    }

    /// Returns the CLI command name corresponding to this channel.
    pub fn cli_command_name(&self) -> &'static str {
        match self {
            Channel::Stable => "oz",
            Channel::Dev => "oz-dev",
            Channel::Preview => "oz-preview",
            Channel::Local => "oz-local",
            Channel::Integration => "oz-integration",
            Channel::Oss => "warp-oss",
        }
    }

    /// Returns the Warp Control CLI command name corresponding to this channel.
    pub fn warpctrl_command_name(&self) -> &'static str {
        match self {
            Channel::Stable => "warpctrl",
            Channel::Dev => "warpctrl-dev",
            Channel::Preview => "warpctrl-preview",
            Channel::Local => "warpctrl-local",
            Channel::Integration => "warpctrl-integration",
            Channel::Oss => "warpctrl-oss",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Channel::Stable => "stable",
            Channel::Preview => "preview",
            Channel::Dev => "dev",
            Channel::Integration => "integration",
            Channel::Local => "local",
            Channel::Oss => "warp-oss",
        })
    }
}
