//! HLS pull worker: poll playlist, download TS, feed demux → packager.

use crate::config::CacheConfig;
use crate::dash::DashPackager;
use crate::demux::TsDemuxBridge;
use crate::hls::playlist::{
    fetch_bytes, fetch_text, parse_media_playlist, resolve_media_playlist_url, resolve_uri,
    MediaPlaylist,
};
use anyhow::{Context, Result};
use reqwest::Client;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Shared last-error slot updated by the pull worker.
pub type ErrorSlot = Arc<Mutex<Option<String>>>;

/// Run one HLS pull session until cancel, end-list, or hard error.
///
/// Caller owns the endless retry loop (sleep `reconnect_secs` while enabled).
pub async fn pull_session(
    client: &Client,
    hls_url: &str,
    channel: &str,
    out_dir: PathBuf,
    cache: &CacheConfig,
    cancel: &CancellationToken,
    last_error: &ErrorSlot,
) -> Result<()> {
    let mut packager = DashPackager::resume(out_dir, cache)
        .with_context(|| format!("packager init for {channel}"))?;

    let (playlist_url, body) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            packager.finish().await;
            return Ok(());
        }
        r = resolve_media_playlist_url(client, hls_url) => r?,
    };

    info!(
        channel,
        playlist = %playlist_url,
        "HLS pull session started"
    );

    let mut demux = TsDemuxBridge::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut started = false;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let body = if started {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                r = fetch_text(client, &playlist_url) => r?,
            }
        } else {
            body.clone()
        };
        started = true;

        let pl: MediaPlaylist = parse_media_playlist(&body)?;
        let poll = Duration::from_secs_f64((pl.target_duration * 0.5).clamp(1.0, 6.0));

        // On first load of a live playlist, start near the live edge (last few segs).
        if seen.is_empty() && !pl.segments.is_empty() {
            let skip = pl.segments.len().saturating_sub(3);
            for seg in pl.segments.iter().take(skip) {
                seen.insert(seg.seq);
            }
        }

        for seg in &pl.segments {
            if cancel.is_cancelled() {
                break;
            }
            if seen.contains(&seg.seq) {
                continue;
            }
            seen.insert(seg.seq);

            if seg.discontinuity || demux.take_discontinuity() {
                warn!(channel, seq = seg.seq, "HLS discontinuity; resetting CMAF generation");
                packager.prepare_for_reconnect().await;
                demux = TsDemuxBridge::new();
            }

            let seg_url = resolve_uri(&playlist_url, &seg.uri)?;
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                r = fetch_bytes(client, &seg_url) => r.with_context(|| format!("segment {}", seg.seq))?,
            };

            let aus = demux.feed(&bytes)?;
            for au in aus {
                packager.handle_au(au)?;
            }

            if let Ok(mut slot) = last_error.lock() {
                *slot = None;
            }
        }

        if pl.end_list {
            info!(channel, "HLS ENDLIST reached");
            break;
        }

        // Bound seen set growth for long-running live.
        if seen.len() > 512 {
            let min_keep = pl.media_sequence.saturating_sub(64);
            seen.retain(|s| *s >= min_keep);
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(poll) => {}
        }
    }

    for au in demux.finish_segment()? {
        packager.handle_au(au)?;
    }
    packager.flush_tail();
    packager.finish().await;
    Ok(())
}

/// Endless pull loop: on any error sleep `reconnect_secs` and retry until cancelled.
pub async fn run_forever(
    client: Client,
    hls_url: String,
    channel: String,
    out_dir: PathBuf,
    cache: CacheConfig,
    reconnect_secs: u64,
    cancel: CancellationToken,
    last_error: ErrorSlot,
) {
    let reconnect = Duration::from_secs(reconnect_secs.max(1));
    info!(
        channel = %channel,
        url = %hls_url,
        reconnect_secs = reconnect.as_secs(),
        "pull worker started"
    );

    loop {
        if cancel.is_cancelled() {
            info!(channel = %channel, "pull worker cancelled");
            break;
        }

        match pull_session(
            &client,
            &hls_url,
            &channel,
            out_dir.clone(),
            &cache,
            &cancel,
            &last_error,
        )
        .await
        {
            Ok(()) => {
                if cancel.is_cancelled() {
                    break;
                }
                warn!(
                    channel = %channel,
                    "HLS session ended; reconnecting in {}s",
                    reconnect.as_secs()
                );
            }
            Err(err) => {
                let msg = format!("{err:#}");
                warn!(channel = %channel, error = %msg, "HLS pull failed; retry in {}s", reconnect.as_secs());
                if let Ok(mut slot) = last_error.lock() {
                    *slot = Some(msg);
                }
            }
        }

        if cancel.is_cancelled() {
            break;
        }

        // Reset packager generation between sessions.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(reconnect) => {}
        }
    }

    info!(channel = %channel, "pull worker exited");
}
