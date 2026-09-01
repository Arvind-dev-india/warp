//! OSS-channel `warp-tui` binary and `default-run` target.
//!
//! This is what bare `cargo run -p warp_tui` builds, so it hand-builds a
//! production config and needs no internal `warp-channel-config` generator
//! (mirrors `app/src/bin/oss.rs`). It is a console application (no GUI window,
//! no app bundle), so unlike the GUI binaries it sets no `windows_subsystem`
//! attribute and embeds no `Info.plist`.

use anyhow::Result;
use warp_core::AppId;
use warp_core::channel::{Channel, ChannelConfig, ChannelState, OzConfig, WarpServerConfig};

fn main() -> Result<()> {
    let mut state = ChannelState::new(
        Channel::Oss,
        ChannelConfig {
            app_id: AppId::new("dev", "warp", "WarpTui"),
            logfile_name: "warp-tui.log".into(),
            server_config: WarpServerConfig {
                server_root_url: std::env::var("WARP_SERVER_ROOT_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:8765".to_string())
                    .into(),
                rtc_server_url: std::env::var("WARP_WS_SERVER_URL")
                    .unwrap_or_else(|_| "ws://127.0.0.1:8765/graphql/v2".to_string())
                    .into(),
                session_sharing_server_url: None,
                firebase_auth_api_key: "local-mode-no-firebase".into(),
                iap_config: None,
            },
            oz_config: OzConfig::production(),
            telemetry_config: None,
            crash_reporting_config: None,
            autoupdate_config: None,
            mcp_static_config: None,
        },
    );
    if cfg!(debug_assertions) {
        state = state.with_additional_features(warp_core::features::DEBUG_FLAGS);
    }
    ChannelState::set(state);

    warp_tui::run()
}
