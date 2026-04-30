use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use warp_local_proxy::{config::Config, server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,warp_local_proxy=debug")))
        .with(fmt::layer().with_target(false))
        .init();

    let config = Config::from_cli();
    let state = server::AppState::new(config);
    server::serve(state).await
}
