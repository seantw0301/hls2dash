//! HLS pull worker: dash → CMAF packager; mpegts → direct continuous TS stitch.

use crate::config::{CacheConfig, MpegTsConfig, OutputMode};
use crate::dash::DashPackager;
use crate::demux::TsDemuxBridge;
use crate::hls::playlist::{
    fetch_bytes, fetch_text, parse_media_playlist, resolve_media_playlist_url, resolve_uri,
    MediaPlaylist,
};
use crate::mpegts::TsStitcher;
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

/// Endless pull loop: on any error sleep `reconnect_secs` and retry until cancelled.
pub async fn run_forever(
    client: Client,
    hls_url: String,
    channel: String,
    out_dir: PathBuf,
    cache: CacheConfig,
    reconnect_secs: u64,
    output_mode: OutputMode,
    mpegts: MpegTsConfig,
    cancel: CancellationToken,
    last_error: ErrorSlot,
) {
    let reconnect = Duration::from_secs(reconnect_secs.max(1));
    info!(
        channel = %channel,
        url = %hls_url,
        mode = output_mode.as_str(),
        reconnect_secs = reconnect.as_secs(),
        "pull worker started"
    );

    loop {
        if cancel.is_cancelled() {
            info!(channel = %channel, "pull worker cancelled");
            break;
        }

        let result = match output_mode {
            OutputMode::Dash => {
                pull_session_dash(
                    &client,
                    &hls_url,
                    &channel,
                    out_dir.clone(),
                    &cache,
                    &cancel,
                    &last_error,
                )
                .await
            }
            OutputMode::Mpegts => {
                pull_session_mpegts(
                    &client,
                    &hls_url,
                    &channel,
                    out_dir.clone(),
                    &cache,
                    &mpegts,
                    &cancel,
                    &last_error,
                )
                .await
            }
        };

        match result {
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

        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(reconnect) => {}
        }
    }

    info!(channel = %channel, "pull worker exited");
}

/// HLS → demux → CMAF / DASH packager.
async fn pull_session_dash(
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

    info!(channel, playlist = %playlist_url, "HLS→DASH pull session started");

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

/// HLS → continuous TS stitch (no CMAF).
///
/// Prefetches enough segments to fill the egress jitter buffer and polls the
/// playlist more aggressively than TARGETDURATION so late origin segments are
/// picked up before the paced `/mpegts` stream underruns.
async fn pull_session_mpegts(
    client: &Client,
    hls_url: &str,
    channel: &str,
    out_dir: PathBuf,
    cache: &CacheConfig,
    mpegts: &MpegTsConfig,
    cancel: &CancellationToken,
    last_error: &ErrorSlot,
) -> Result<()> {
    let mut stitcher = TsStitcher::resume(out_dir, cache.window_segments)
        .with_context(|| format!("TS stitcher init for {channel}"))?;

    let (playlist_url, body) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(()),
        r = resolve_media_playlist_url(client, hls_url) => r?,
    };

    info!(channel, playlist = %playlist_url, "HLS→MPEG-TS stitch session started");

    let mut seen: HashSet<u64> = HashSet::new();
    let mut started = false;
    let keep_on_start = (mpegts.live_holdback_segments + mpegts.min_buffer_segments)
        .max(3) as usize;
    let poll_factor = mpegts.ingest_poll_factor.clamp(0.05, 1.0);

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
        // Aggressive poll so delayed HLS segments land in the disk buffer early.
        let poll = Duration::from_secs_f64((pl.target_duration * poll_factor).clamp(0.5, 6.0));

        if seen.is_empty() && !pl.segments.is_empty() {
            let skip = pl.segments.len().saturating_sub(keep_on_start);
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

            if seg.discontinuity {
                warn!(channel, seq = seg.seq, "HLS discontinuity; resetting TS stitch generation");
                stitcher.prepare_for_reconnect()?;
            }

            let seg_url = resolve_uri(&playlist_url, &seg.uri)?;
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                r = fetch_bytes(client, &seg_url) => r.with_context(|| format!("segment {}", seg.seq))?,
            };

            stitcher
                .push_segment(&bytes, seg.duration_secs)
                .with_context(|| format!("stitch segment {}", seg.seq))?;

            if let Ok(mut slot) = last_error.lock() {
                *slot = None;
            }
        }

        if pl.end_list {
            info!(channel, "HLS ENDLIST reached");
            break;
        }

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

    Ok(())
}
