//! `musicbrainz-feeder` — the MusicBrainz identity feeder.
//!
//! Serves `fileType:card` for `contentKind:album|artist`. No credential:
//! MusicBrainz, the Cover Art Archive and ListenBrainz all serve this traffic
//! unauthenticated, so a freshly deployed feeder answers on its first boot.
//!
//! Env:
//! - `META_FEEDER_HTTP_LISTEN` — listen addr (default `0.0.0.0:8080`)
//! - `META_FEEDER_STATE_DIR`   — per-plugin cache root (default `/data/meta-feeder`)
//! - `CARD_TOP_N`, `CARD_DISCOVERY_N` — optional result-sizing seeds
//! - `RUST_LOG`                — tracing filter (default `info`)

use std::net::SocketAddr;

use meta_feeder_sdk::plugin::FeederPlugin;
use meta_feeder_sdk::serve_feeders;
use musicbrainz_feeder::musicbrainz::MusicBrainzCardPlugin;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let listen: SocketAddr = std::env::var("META_FEEDER_HTTP_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;
    let state_dir =
        std::env::var("META_FEEDER_STATE_DIR").unwrap_or_else(|_| "/data/meta-feeder".to_string());

    let plugins: Vec<Box<dyn FeederPlugin>> = vec![Box::new(MusicBrainzCardPlugin::new())];
    serve_feeders(plugins, state_dir, listen).await
}
