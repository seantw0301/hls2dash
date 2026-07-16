mod cache;
mod channel;
mod config;
mod dash;
mod demux;
mod hls;
mod http;
mod mpegts;

use crate::channel::ChannelRegistry;
use crate::config::{Config, OutputMode};
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "hls2dash",
    about = "HLS pull → live MPEG-DASH or continuous MPEG-TS (H.264 + AAC)"
)]
struct Cli {
    /// Path to YAML config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config).with_context(|| {
        format!(
            "load config from {} (cwd={})",
            cli.config.display(),
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        )
    })?;
    let cfg = Arc::new(cfg);

    std::fs::create_dir_all(&cfg.cache.dir)
        .with_context(|| format!("create cache dir {}", cfg.cache.dir.display()))?;

    let play = match cfg.output_mode {
        OutputMode::Dash => format!(
            "http://{}:{}/live/<channel>/index.mpd",
            cfg.dash.listen, cfg.dash.port
        ),
        OutputMode::Mpegts => format!(
            "http://{}:{}/live/<channel>/mpegts",
            cfg.dash.listen, cfg.dash.port
        ),
    };

    info!(
        output_mode = cfg.output_mode.as_str(),
        play = %play,
        pull_sources = cfg.pull.len(),
        cache = %cfg.cache.dir.display(),
        segment_duration_secs = cfg.cache.segment_duration_secs,
        "hls2dash starting"
    );

    let retention = Arc::new(cache::RetentionRegistry::new(
        cfg.recovery.retention_grace_segments,
    ));

    let registry = ChannelRegistry::new(&cfg, Arc::clone(&retention));
    registry.seed_from_config(&cfg.pull).await?;

    let http_cfg = Arc::clone(&cfg);
    let janitor_cfg = Arc::clone(&cfg);
    let janitor_retention = Arc::clone(&retention);
    let http_registry = registry.clone();
    let http_retention = Arc::clone(&retention);

    let http_task = tokio::spawn(supervise("http", move || {
        let cfg = Arc::clone(&http_cfg);
        let registry = http_registry.clone();
        let retention = Arc::clone(&http_retention);
        async move { http::run(cfg, registry, retention).await }
    }));

    let janitor_task = tokio::spawn(supervise("cache-janitor", move || {
        let cfg = Arc::clone(&janitor_cfg);
        let retention = Arc::clone(&janitor_retention);
        async move {
            cache::run(cfg, retention).await;
            Ok(())
        }
    }));

    tokio::select! {
        _ = http_task => warn!("http supervisor ended"),
        _ = janitor_task => warn!("janitor supervisor ended"),
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received");
            let _ = registry.disable_all().await;
        }
    }

    Ok(())
}

async fn supervise<F, Fut>(name: &'static str, mut factory: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        info!(service = name, "service starting");
        match factory().await {
            Ok(()) => {
                warn!(service = name, "service returned Ok (unexpected); restarting");
                backoff = Duration::from_secs(1);
            }
            Err(err) => {
                error!(service = name, "service error: {err:#}; restarting");
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tokio::time::sleep(backoff).await;
    }
}
