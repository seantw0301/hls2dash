//! HLS pull worker: dash → CMAF packager; mpegts → direct continuous TS stitch.

use crate::cache::RetentionRegistry;
use crate::config::{CacheConfig, MpegTsConfig, OutputMode, RecoveryConfig};
use crate::dash::DashPackager;
use crate::demux::TsDemuxBridge;
use crate::hls::dedup::{
    ContentDedupAction, SegmentDedup, SeqOrderAction,
};
use crate::hls::playlist::{
    fetch_text, parse_media_playlist, resolve_media_playlist_url, resolve_uri, MediaPlaylist,
    PlaylistSegment,
};
use crate::hls::recovery::{detect_sequence_gap, fetch_segment_bytes};
use crate::mpegts::{align_ts_packets_truncate, repair_ts_packets, TsStitcher};
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
    recovery: RecoveryConfig,
    retention: Arc<RetentionRegistry>,
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
                    &recovery,
                    &retention,
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
                    &recovery,
                    &retention,
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

fn prepare_ts_bytes(raw: &[u8], recovery: &RecoveryConfig) -> Result<Vec<u8>> {
    if recovery.ts_resync_packets {
        let (ts, stats) = repair_ts_packets(raw)?;
        if stats.packets_dropped > 0 || stats.resync_count > 0 {
            warn!(
                packets_kept = stats.packets_kept,
                packets_dropped = stats.packets_dropped,
                resync_count = stats.resync_count,
                "TS packet repair applied"
            );
        }
        Ok(ts)
    } else {
        align_ts_packets_truncate(raw)
    }
}

/// How to proceed after seq-order / content-fingerprint checks.
enum SegmentIngestDecision {
    Skip,
    Ingest,
    ResetAndIngest,
}

fn check_seq_order(channel: &str, last_seq: Option<u64>, seq: u64) -> SeqOrderAction {
    let action = SegmentDedup::check_seq_order(last_seq, seq);
    match action {
        SeqOrderAction::SkipRewind => {
            warn!(channel, seq, ?last_seq, "HLS segment rewind; skipping");
        }
        SeqOrderAction::SequenceReset => {
            warn!(channel, seq, ?last_seq, "HLS MEDIA-SEQUENCE reset detected");
        }
        SeqOrderAction::Advance => {}
    }
    action
}

fn check_content_dedup(
    dedup: &mut SegmentDedup,
    channel: &str,
    seq: u64,
    bytes: &[u8],
) -> SegmentIngestDecision {
    match dedup.check_content(channel, seq, bytes) {
        ContentDedupAction::Accept => SegmentIngestDecision::Ingest,
        ContentDedupAction::SkipDuplicate | ContentDedupAction::SkipReplay => {
            SegmentIngestDecision::Skip
        }
        ContentDedupAction::ResetOutput => {
            dedup.clear();
            SegmentIngestDecision::ResetAndIngest
        }
    }
}

async fn prepare_segment_seq_dash(
    channel: &str,
    seg: &PlaylistSegment,
    seen: &mut HashSet<u64>,
    dedup: &mut SegmentDedup,
    last_seq: &mut Option<u64>,
    packager: &mut DashPackager,
    demux: &mut TsDemuxBridge,
) -> bool {
    if seen.contains(&seg.seq) {
        return false;
    }

    match check_seq_order(channel, *last_seq, seg.seq) {
        SeqOrderAction::SkipRewind => {
            seen.insert(seg.seq);
            return false;
        }
        SeqOrderAction::SequenceReset => {
            seen.clear();
            dedup.clear();
            packager.prepare_for_reconnect().await;
            *demux = TsDemuxBridge::new();
            *last_seq = None;
        }
        SeqOrderAction::Advance => {}
    }

    seen.insert(seg.seq);
    true
}

fn prepare_segment_seq_mpegts(
    channel: &str,
    seg: &PlaylistSegment,
    seen: &mut HashSet<u64>,
    dedup: &mut SegmentDedup,
    last_seq: &mut Option<u64>,
    stitcher: &mut TsStitcher,
) -> Result<bool> {
    if seen.contains(&seg.seq) {
        return Ok(false);
    }

    match check_seq_order(channel, *last_seq, seg.seq) {
        SeqOrderAction::SkipRewind => {
            seen.insert(seg.seq);
            return Ok(false);
        }
        SeqOrderAction::SequenceReset => {
            seen.clear();
            dedup.clear();
            stitcher.prepare_for_reconnect()?;
            *last_seq = None;
        }
        SeqOrderAction::Advance => {}
    }

    seen.insert(seg.seq);
    Ok(true)
}

/// HLS → demux → CMAF / DASH packager.
async fn pull_session_dash(
    client: &Client,
    hls_url: &str,
    channel: &str,
    out_dir: PathBuf,
    cache: &CacheConfig,
    recovery: &RecoveryConfig,
    retention: &Arc<RetentionRegistry>,
    cancel: &CancellationToken,
    last_error: &ErrorSlot,
) -> Result<()> {
    let mut packager = DashPackager::resume(
        out_dir,
        cache,
        channel,
        Arc::clone(retention),
        recovery.dash_skip_to_keyframe,
    )
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
    let mut dedup = SegmentDedup::new(128);
    let mut started = false;
    let mut last_seq: Option<u64> = None;

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
                last_seq = Some(seg.seq);
            }
        }

        for seg in &pl.segments {
            if cancel.is_cancelled() {
                break;
            }
            if !prepare_segment_seq_dash(
                channel,
                seg,
                &mut seen,
                &mut dedup,
                &mut last_seq,
                &mut packager,
                &mut demux,
            )
            .await
            {
                continue;
            }

            if detect_sequence_gap(channel, last_seq, seg.seq) {
                let gap_dur = seg.duration_secs.max(0.1);
                let gap_ticks = (gap_dur * 1000.0).round().max(1.0) as u64;
                packager.inject_gap(gap_ticks);
            }

            let hls_discontinuity = seg.discontinuity;
            let ts_discontinuity = demux.take_discontinuity();

            if hls_discontinuity {
                warn!(channel, seq = seg.seq, "HLS discontinuity; resetting CMAF generation");
                dedup.clear();
                packager.prepare_for_reconnect().await;
                demux = TsDemuxBridge::new();
            } else if ts_discontinuity {
                packager.begin_sync_recovery();
            }

            let seg_url = resolve_uri(&playlist_url, &seg.uri)?;
            let fetch_result = fetch_segment_bytes(
                client,
                cancel,
                &seg_url,
                seg.seq,
                recovery,
            )
            .await;

            let bytes = match fetch_result {
                Ok(b) => b,
                Err(err) => {
                    if recovery.skip_missing_segments {
                        warn!(
                            channel,
                            seq = seg.seq,
                            error = %err,
                            "segment fetch failed; skipping"
                        );
                        let gap_ticks =
                            (seg.duration_secs.max(0.1) * 1000.0).round().max(1.0) as u64;
                        packager.inject_gap(gap_ticks);
                        last_seq = Some(seg.seq);
                        continue;
                    }
                    return Err(err).with_context(|| format!("segment {}", seg.seq));
                }
            };

            match check_content_dedup(&mut dedup, channel, seg.seq, &bytes) {
                SegmentIngestDecision::Skip => continue,
                SegmentIngestDecision::ResetAndIngest => {
                    packager.prepare_for_reconnect().await;
                    demux = TsDemuxBridge::new();
                    let _ = dedup.check_content(channel, seg.seq, &bytes);
                }
                SegmentIngestDecision::Ingest => {}
            }

            let ts_bytes = match prepare_ts_bytes(&bytes, recovery) {
                Ok(ts) => ts,
                Err(err) => {
                    if recovery.skip_missing_segments {
                        warn!(
                            channel,
                            seq = seg.seq,
                            error = %err,
                            "TS repair failed; skipping segment"
                        );
                        let gap_ticks =
                            (seg.duration_secs.max(0.1) * 1000.0).round().max(1.0) as u64;
                        packager.inject_gap(gap_ticks);
                        last_seq = Some(seg.seq);
                        continue;
                    }
                    return Err(err).with_context(|| format!("segment {}", seg.seq));
                }
            };

            let aus = demux.feed(&ts_bytes)?;
            for au in aus {
                packager.handle_au(au)?;
            }
            last_seq = Some(seg.seq);

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
async fn pull_session_mpegts(
    client: &Client,
    hls_url: &str,
    channel: &str,
    out_dir: PathBuf,
    cache: &CacheConfig,
    mpegts: &MpegTsConfig,
    recovery: &RecoveryConfig,
    retention: &Arc<RetentionRegistry>,
    cancel: &CancellationToken,
    last_error: &ErrorSlot,
) -> Result<()> {
    let mut stitcher = TsStitcher::resume(
        out_dir,
        cache.window_segments,
        channel,
        Arc::clone(retention),
        recovery.ts_resync_packets,
    )
    .with_context(|| format!("TS stitcher init for {channel}"))?;

    let (playlist_url, body) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(()),
        r = resolve_media_playlist_url(client, hls_url) => r?,
    };

    info!(channel, playlist = %playlist_url, "HLS→MPEG-TS stitch session started");

    let mut seen: HashSet<u64> = HashSet::new();
    let mut dedup = SegmentDedup::new(128);
    let mut started = false;
    let mut last_seq: Option<u64> = None;
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
        let poll = Duration::from_secs_f64((pl.target_duration * poll_factor).clamp(0.5, 6.0));

        if seen.is_empty() && !pl.segments.is_empty() {
            let skip = pl.segments.len().saturating_sub(keep_on_start);
            for seg in pl.segments.iter().take(skip) {
                seen.insert(seg.seq);
                last_seq = Some(seg.seq);
            }
        }

        for seg in &pl.segments {
            if cancel.is_cancelled() {
                break;
            }
            if !prepare_segment_seq_mpegts(
                channel,
                seg,
                &mut seen,
                &mut dedup,
                &mut last_seq,
                &mut stitcher,
            )? {
                continue;
            }

            if detect_sequence_gap(channel, last_seq, seg.seq) {
                let gap_dur = seg.duration_secs.max(0.1);
                stitcher
                    .skip_segment(gap_dur)
                    .with_context(|| format!("skip gap before segment {}", seg.seq))?;
            }

            if seg.discontinuity {
                warn!(channel, seq = seg.seq, "HLS discontinuity; resetting TS stitch generation");
                dedup.clear();
                stitcher.prepare_for_reconnect()?;
            }

            let seg_url = resolve_uri(&playlist_url, &seg.uri)?;
            let fetch_result = fetch_segment_bytes(
                client,
                cancel,
                &seg_url,
                seg.seq,
                recovery,
            )
            .await;

            let bytes = match fetch_result {
                Ok(b) => b,
                Err(err) => {
                    if recovery.skip_missing_segments {
                        warn!(
                            channel,
                            seq = seg.seq,
                            error = %err,
                            "segment fetch failed; skipping"
                        );
                        stitcher
                            .skip_segment(seg.duration_secs.max(0.1))
                            .with_context(|| format!("skip segment {}", seg.seq))?;
                        last_seq = Some(seg.seq);
                        continue;
                    }
                    return Err(err).with_context(|| format!("segment {}", seg.seq));
                }
            };

            match check_content_dedup(&mut dedup, channel, seg.seq, &bytes) {
                SegmentIngestDecision::Skip => continue,
                SegmentIngestDecision::ResetAndIngest => {
                    stitcher.prepare_for_reconnect()?;
                    let _ = dedup.check_content(channel, seg.seq, &bytes);
                }
                SegmentIngestDecision::Ingest => {}
            }

            match stitcher.push_segment(&bytes, seg.duration_secs) {
                Ok(_) => {}
                Err(err) => {
                    if recovery.skip_missing_segments {
                        warn!(
                            channel,
                            seq = seg.seq,
                            error = %err,
                            "stitch failed; skipping segment"
                        );
                        stitcher
                            .skip_segment(seg.duration_secs.max(0.1))
                            .with_context(|| format!("skip segment {}", seg.seq))?;
                    } else {
                        return Err(err).with_context(|| format!("stitch segment {}", seg.seq));
                    }
                }
            }
            last_seq = Some(seg.seq);

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
