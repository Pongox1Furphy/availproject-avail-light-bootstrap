#![doc = include_str!("../README.md")]

mod network;
mod server;
mod telemetry;
mod types;

use crate::telemetry::{MetricValue, Metrics};
use anyhow::{Context, Result};
use clap::Parser;
use libp2p::{multiaddr::Protocol, Multiaddr};
use std::{net::Ipv4Addr, time::Duration};
use tokio::time::{interval_at, Instant};
use tracing::{error, info, metadata::ParseLevelError, warn, Level, Subscriber};
use tracing_subscriber::{
    fmt::format::{self},
    EnvFilter, FmtSubscriber,
};
use types::RuntimeConfig;

const CLIENT_ROLE: &str = "bootstrap_node";

#[derive(Debug, Parser)]
#[clap(name = "Avail Bootstrap Node")]
struct CliOpts {
    #[clap(
        long,
        short = 'c',
        default_value = "config.yaml",
        help = "yaml configuration file"
    )]
    config: String,
}

fn parse_log_lvl(log_lvl: &str, default: Level) -> (Level, Option<ParseLevelError>) {
    log_lvl
        .to_uppercase()
        .parse::<Level>()
        .map(|lvl| (lvl, None))
        .unwrap_or_else(|err| (default, Some(err)))
}

fn json_subscriber(log_lvl: Level) -> impl Subscriber + Send + Sync {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::new(format!("avail_light_bootstrap={log_lvl}")))
        .event_format(format::json())
        .finish()
}

fn default_subscriber(log_lvl: Level) -> impl Subscriber + Send + Sync {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::new(format!("avail_light_bootstrap={log_lvl}")))
        .with_span_events(format::FmtSpan::CLOSE)
        .finish()
}

async fn run() -> Result<()> {
    let opts = CliOpts::parse();
    let cfg_path = &opts.config;
    let cfg: RuntimeConfig = confy::load_path(cfg_path)
        .context(format!("Failed to load configuration from path {cfg_path}"))?;

    let (log_lvl, parse_err) = parse_log_lvl(&cfg.log_level, Level::INFO);
    // set json trace format
    if cfg.log_format_json {
        tracing::subscriber::set_global_default(json_subscriber(log_lvl))
            .expect("global json subscriber to be set");
    } else {
        tracing::subscriber::set_global_default(default_subscriber(log_lvl))
            .expect("global default subscriber to be set");
    }
    if let Some(err) = parse_err {
        warn!("Using default log level: {err}");
    }

    info!("Using config: {:?}", cfg);

    let (id_keys, peer_id) = network::keypair((&cfg).into())?;

    let (network_client, network_event_loop) = network::init((&cfg).into(), id_keys)
        .context("Failed to initialize P2P Network Service.")?;

    tokio::spawn(server::run((&cfg).into()));

    let ot_metrics =
        telemetry::otlp::initialize(cfg.ot_collector_endpoint, peer_id, CLIENT_ROLE.into())
            .context("Cannot initialize OpenTelemetry service.")?;

    // Spawn the network task
    let loop_handle = tokio::spawn(network_event_loop.run());

    // Spawn metrics task
    let m_network_client = network_client.clone();
    tokio::spawn(async move {
        let pause_duration = Duration::from_secs(cfg.metrics_network_dump_interval);
        let mut interval = interval_at(Instant::now() + pause_duration, pause_duration);
        // repeat and send commands on given interval
        loop {
            interval.tick().await;
            // try and read current multiaddress
            if let Ok(Some(addr)) = m_network_client.get_multiaddress().await {
                // set Multiaddress
                _ = ot_metrics.set_multiaddress(addr.to_string()).await;
                if let Some(ip) = network::extract_ip(addr) {
                    // set IP
                    _ = ot_metrics.set_ip(ip).await;
                }
            }
            if let Ok(counted_peers) = m_network_client.count_dht_entries().await {
                if let Err(err) = ot_metrics
                    .record(MetricValue::KadRoutingPeerNum(counted_peers))
                    .await
                {
                    error!("Error recording network stats metric: {err}");
                }
            };
            _ = ot_metrics.record(MetricValue::HealthCheck()).await;
        }
    });

    // Listen on all interfaces
    network_client
        .start_listening(
            Multiaddr::empty()
                .with(Protocol::from(Ipv4Addr::UNSPECIFIED))
                .with(Protocol::Udp(cfg.port))
                .with(Protocol::QuicV1),
        )
        .await
        .context("Listening on UDP not to fail.")?;
    info!("Started listening on port: {:?}.", cfg.port);

    info!("Bootstrap node starting ...");
    network_client.bootstrap().await?;
    info!("Bootstrap done.");
    loop_handle.await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await.map_err(|err| {
        error!("{err}");
        err
    })
}
