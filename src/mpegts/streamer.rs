//! Live-edge cursor that streams cached continuous `.ts` segments over HTTP.
//!
//! Aligned with `trans_server` `/mpegts` behavior (holdback, lag jump, idle
//! timeout, generation reset → end stream). Segments are already stitched at
//! ingest — this path does **not** remux through CMAF.
//!
//! Egress is paced at ~1× media realtime (from `seg_N.dur` / EXTINF).
//! When pacing is enabled, each segment is split into TS-aligned sub-chunks
//! sent evenly across the segment duration instead of one burst + silence.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use crate::cache::RetentionRegistry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const DEFAULT_POLL: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_SEG_DUR_SECS: f64 = 2.0;
const TS_PACKET_SIZE: usize = 188;
const DEFAULT_EGRESS_CHUNK_MS: u64 = 250;
const MIN_EGRESS_CHUNK_MS: u64 = 50;
const MAX_EGRESS_CHUNK_MS: u64 = 2000;

/// Options for one `/mpegts` client stream.
#[derive(Debug, Clone)]
pub struct MpegTsStreamOpts {
    pub poll_interval_secs: u64,
    pub holdback_segments: u64,
    pub min_buffer_segments: u64,
    pub max_segment_lag: u64,
    pub send_queue: usize,
    pub pace_egress: bool,
    /// Sub-chunk spacing when `pace_egress` is true (milliseconds).
    pub egress_chunk_ms: u64,
    pub default_segment_duration_secs: f64,
}

impl Default for MpegTsStreamOpts {
    fn default() -> Self {
        Self {
            poll_interval_secs: 2,
            holdback_segments: 6,
            min_buffer_segments: 3,
            max_segment_lag: 10,
            send_queue: 32,
            pace_egress: true,
            egress_chunk_ms: DEFAULT_EGRESS_CHUNK_MS,
            default_segment_duration_secs: DEFAULT_SEG_DUR_SECS,
        }
    }
}

/// Streams multiplexed MPEG-TS bytes for one channel cache directory.
pub struct MpegTsStreamer {
    channel_id: String,
    channel_dir: PathBuf,
    retention: Arc<RetentionRegistry>,
    poll: Duration,
    holdback: u64,
    min_buffer: u64,
    max_lag: u64,
    pace_egress: bool,
    egress_chunk_ms: u64,
    default_dur: f64,
    next_number: Option<u64>,
    /// Generation marker: lowest segment number seen at join; rewind past this → reset.
    gen_floor: Option<u64>,
    /// In-progress smooth egress for the current segment.
    smooth: Option<SmoothSegment>,
}

/// Evenly paced sub-chunks of one on-disk segment.
struct SmoothSegment {
    seg_num: u64,
    data: Bytes,
    offset: usize,
    slice_bytes: usize,
    slice_interval: Duration,
    next_at: Instant,
}

/// Spawns the streamer and returns a receiver of TS byte chunks.
pub fn spawn_mpegts_stream(
    channel_id: String,
    channel_dir: PathBuf,
    opts: MpegTsStreamOpts,
    retention: Arc<RetentionRegistry>,
) -> mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel(opts.send_queue.max(1));
    let streamer = MpegTsStreamer::new(channel_id, channel_dir, opts, retention);
    tokio::spawn(async move {
        streamer.run(tx).await;
    });
    rx
}

impl MpegTsStreamer {
    fn new(
        channel_id: String,
        channel_dir: PathBuf,
        opts: MpegTsStreamOpts,
        retention: Arc<RetentionRegistry>,
    ) -> Self {
        Self {
            channel_id,
            channel_dir,
            retention,
            poll: if opts.poll_interval_secs == 0 {
                DEFAULT_POLL
            } else {
                Duration::from_secs(opts.poll_interval_secs)
            },
            holdback: opts.holdback_segments,
            min_buffer: opts.min_buffer_segments.max(1),
            max_lag: opts.max_segment_lag.max(1),
            pace_egress: opts.pace_egress,
            egress_chunk_ms: opts.egress_chunk_ms.clamp(MIN_EGRESS_CHUNK_MS, MAX_EGRESS_CHUNK_MS),
            default_dur: if opts.default_segment_duration_secs.is_finite()
                && opts.default_segment_duration_secs > 0.0
            {
                opts.default_segment_duration_secs
            } else {
                DEFAULT_SEG_DUR_SECS
            },
            next_number: None,
            gen_floor: None,
            smooth: None,
        }
    }

    async fn run(mut self, tx: mpsc::Sender<Result<Bytes, std::io::Error>>) {
        if !self.wait_for_min_buffer(&tx).await {
            return;
        }

        let mut last_progress = Instant::now();
        let mut underrun = false;
        let mut active_cursor: Option<u64> = None;

        loop {
            if tx.is_closed() {
                break;
            }

            if let Some(mut smooth) = self.smooth.take() {
                let next_at = smooth.next_at;
                let seg_num = smooth.seg_num;
                if self.wait_until(next_at, &tx).await {
                    self.smooth = Some(smooth);
                    break;
                }
                self.touch_cursor(&mut active_cursor, seg_num);
                if underrun {
                    underrun = false;
                    tracing::debug!(
                        channel = %self.channel_id,
                        "mpegts underrun recovered during smooth egress"
                    );
                }

                let chunk = smooth.take_slice();
                if !chunk.is_empty() {
                    last_progress = Instant::now();
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }

                if smooth.is_done() {
                    // segment fully egressed
                } else {
                    smooth.next_at += smooth.slice_interval;
                    self.smooth = Some(smooth);
                }
                continue;
            }

            match self.next_chunk().await {
                Ok(Some((chunk, dur_secs, seg_num))) => {
                    self.touch_cursor(&mut active_cursor, seg_num);
                    if underrun {
                        underrun = false;
                        tracing::debug!(
                            channel = %self.channel_id,
                            "mpegts underrun recovered; re-anchoring pace"
                        );
                    }

                    if self.pace_egress {
                        if chunk.is_empty() {
                            if self.sleep_duration(dur_secs, &tx).await {
                                break;
                            }
                            last_progress = Instant::now();
                            continue;
                        }
                        self.smooth = Some(SmoothSegment::new(
                            chunk,
                            dur_secs,
                            seg_num,
                            self.egress_chunk_ms,
                        ));
                        continue;
                    }

                    last_progress = Instant::now();
                    if !chunk.is_empty() && tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    underrun = true;
                    if last_progress.elapsed() >= IDLE_TIMEOUT {
                        tracing::info!(
                            channel = %self.channel_id,
                            "mpegts stream idle timeout; flushing"
                        );
                        break;
                    }
                    tokio::time::sleep(self.poll).await;
                }
                Err(err) => {
                    tracing::warn!(
                        channel = %self.channel_id,
                        error = %err,
                        "mpegts stream failed"
                    );
                    let _ = tx.send(Err(std::io::Error::other(err.to_string()))).await;
                    break;
                }
            }
        }

        if let Some(cursor) = active_cursor {
            self.retention
                .unregister_mpegts_cursor(&self.channel_id, cursor);
        }
    }

    fn touch_cursor(&self, active_cursor: &mut Option<u64>, seg_num: u64) {
        if active_cursor != &Some(seg_num) {
            if let Some(old) = active_cursor.take() {
                self.retention
                    .unregister_mpegts_cursor(&self.channel_id, old);
            }
            *active_cursor = Some(seg_num);
            self.retention
                .register_mpegts_cursor(&self.channel_id, seg_num);
        }
    }

    async fn wait_until(&self, deadline: Instant, tx: &mpsc::Sender<Result<Bytes, std::io::Error>>) -> bool {
        let now = Instant::now();
        if now < deadline {
            tokio::select! {
                biased;
                _ = tx.closed() => return true,
                _ = tokio::time::sleep(deadline.saturating_duration_since(now)) => {}
            }
        }
        false
    }

    async fn sleep_duration(
        &self,
        dur_secs: f64,
        tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    ) -> bool {
        let d = Duration::from_secs_f64(dur_secs.clamp(0.1, 60.0));
        tokio::select! {
            biased;
            _ = tx.closed() => true,
            _ = tokio::time::sleep(d) => false,
        }
    }

    /// Block until `min_buffer` segments exist (or client disconnects / idle).
    async fn wait_for_min_buffer(&self, tx: &mpsc::Sender<Result<Bytes, std::io::Error>>) -> bool {
        let started = Instant::now();
        loop {
            if tx.is_closed() {
                return false;
            }
            match scan_ts_segments(&self.channel_dir) {
                Ok(segs) if (segs.len() as u64) >= self.min_buffer => {
                    tracing::info!(
                        channel = %self.channel_id,
                        buffered = segs.len(),
                        min_buffer = self.min_buffer,
                        "mpegts buffer ready; starting paced egress"
                    );
                    return true;
                }
                Ok(_) | Err(_) => {}
            }
            if started.elapsed() >= IDLE_TIMEOUT {
                tracing::warn!(
                    channel = %self.channel_id,
                    min_buffer = self.min_buffer,
                    "mpegts buffer wait timed out"
                );
                let _ = tx
                    .send(Err(std::io::Error::other(
                        "mpegts buffer not ready (min_buffer timeout)",
                    )))
                    .await;
                return false;
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<(Bytes, f64, u64)>> {
        let segments = scan_ts_segments(&self.channel_dir)?;
        if segments.is_empty() {
            return Ok(None);
        }

        let live_edge = *segments.keys().next_back().unwrap_or(&0);
        let floor = *segments.keys().next().unwrap_or(&0);
        if self.gen_floor.is_none() {
            self.gen_floor = Some(floor);
        }

        // True generation wipe: live edge fell below the floor we joined with.
        if let Some(gen_floor) = self.gen_floor {
            if live_edge < gen_floor {
                return Err(anyhow!("ts cache generation changed; reconnect"));
            }
        }

        let (next, jump) =
            resolve_cursor(self.next_number, live_edge, floor, self.holdback, self.max_lag);
        match jump {
            CursorJump::Forward => {
                tracing::debug!(
                    channel = %self.channel_id,
                    cursor = self.next_number.unwrap_or(0),
                    live_edge,
                    floor,
                    holdback = self.holdback,
                    "mpegts client lagging; jumping to safe live edge"
                );
            }
            CursorJump::Rewind => {
                tracing::info!(
                    channel = %self.channel_id,
                    cursor = self.next_number.unwrap_or(0),
                    live_edge,
                    "upstream sequence reset detected; ending mpegts stream for client reconnect"
                );
                return Err(anyhow!(
                    "upstream generation reset (cursor={} live_edge={})",
                    self.next_number.unwrap_or(0),
                    live_edge
                ));
            }
            CursorJump::None => {}
        }

        self.next_number = Some(next);
        let Some(path) = segments.get(&next) else {
            // Behind pruned window → snap to safe edge on next poll.
            if next < floor {
                let safe = live_edge.saturating_sub(self.holdback).max(floor);
                self.next_number = Some(safe);
            }
            return Ok(None);
        };

        let dur_path = path.with_extension("dur");
        if !path.exists() && dur_path.exists() {
            // Skipped segment (gap): pace through duration without TS payload.
            let dur = read_segment_duration(path, self.default_dur).await;
            self.retention.touch_seg(&self.channel_id, next);
            self.next_number = Some(next.saturating_add(1));
            return Ok(Some((Bytes::new(), dur, next)));
        }

        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        if bytes.is_empty() {
            let dur = read_segment_duration(path, self.default_dur).await;
            self.retention.touch_seg(&self.channel_id, next);
            self.next_number = Some(next.saturating_add(1));
            return Ok(Some((Bytes::new(), dur, next)));
        }
        if bytes.len() % 188 != 0 || bytes[0] != 0x47 {
            return Err(anyhow!("invalid TS segment {}", path.display()));
        }

        let dur = read_segment_duration(path, self.default_dur).await;
        self.retention.touch_seg(&self.channel_id, next);
        self.next_number = Some(next.saturating_add(1));
        Ok(Some((Bytes::from(bytes), dur, next)))
    }
}

impl SmoothSegment {
    fn new(data: Bytes, dur_secs: f64, seg_num: u64, chunk_ms: u64) -> Self {
        let (slice_bytes, slice_interval) = plan_smooth_slices(data.len(), dur_secs, chunk_ms);
        Self {
            seg_num,
            data,
            offset: 0,
            slice_bytes,
            slice_interval,
            next_at: Instant::now(),
        }
    }

    fn take_slice(&mut self) -> Bytes {
        if self.offset >= self.data.len() {
            return Bytes::new();
        }
        let remaining = self.data.len() - self.offset;
        let take = remaining.min(self.slice_bytes);
        let end = self.offset + take;
        let chunk = self.data.slice(self.offset..end);
        self.offset = end;
        chunk
    }

    fn is_done(&self) -> bool {
        self.offset >= self.data.len()
    }
}

/// Split `data_len` bytes across `dur_secs` using ~`chunk_ms` spacing.
fn plan_smooth_slices(data_len: usize, dur_secs: f64, chunk_ms: u64) -> (usize, Duration) {
    let dur = dur_secs.clamp(0.1, 60.0);
    if data_len == 0 {
        return (0, Duration::from_secs_f64(dur));
    }
    let chunk_secs = (chunk_ms as f64 / 1000.0).clamp(0.05, 2.0);
    let max_slices = data_len.div_ceil(TS_PACKET_SIZE).max(1);
    let slices = ((dur / chunk_secs).ceil() as usize).max(1).min(max_slices);
    let mut per_slice = data_len / slices;
    per_slice = (per_slice / TS_PACKET_SIZE) * TS_PACKET_SIZE;
    if per_slice == 0 {
        per_slice = data_len;
    }
    let interval = Duration::from_secs_f64(dur / slices as f64);
    (per_slice, interval)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorJump {
    None,
    Forward,
    Rewind,
}

fn resolve_cursor(
    current: Option<u64>,
    live_edge: u64,
    floor: u64,
    holdback: u64,
    max_lag: u64,
) -> (u64, CursorJump) {
    let max_lag = max_lag.max(1);
    // Never aim below the oldest cached segment (holdback can exceed buffer depth).
    let safe_edge = live_edge.saturating_sub(holdback).max(floor);
    match current {
        Some(n) if safe_edge > n.saturating_add(max_lag) => (safe_edge, CursorJump::Forward),
        Some(n) if n > live_edge.saturating_add(max_lag) => (safe_edge, CursorJump::Rewind),
        Some(n) if n < floor => (safe_edge, CursorJump::Forward),
        Some(n) => (n, CursorJump::None),
        None => (safe_edge, CursorJump::None),
    }
}

fn scan_ts_segments(dir: &Path) -> Result<BTreeMap<u64, PathBuf>> {
    let mut segments = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(segments);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(num) = parse_ts_seg_number(name) {
            segments.insert(num, path);
            continue;
        }
        if let Some(num) = parse_ts_dur_number(name) {
            let ts_path = dir.join(format!("seg_{num}.ts"));
            segments
                .entry(num)
                .or_insert_with(|| if ts_path.exists() { ts_path } else { path });
        }
    }
    Ok(segments)
}

fn parse_ts_dur_number(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".dur")?;
    rest.parse().ok()
}

fn parse_ts_seg_number(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".ts")?;
    rest.parse().ok()
}

async fn read_segment_duration(ts_path: &Path, fallback: f64) -> f64 {
    let dur_path = ts_path.with_extension("dur");
    match tokio::fs::read_to_string(&dur_path).await {
        Ok(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(fallback),
        Err(_) => fallback,
    }
}

/// True when channel cache has at least `min_buffer` continuous TS segments.
pub fn channel_mpegts_ready(channel_dir: &Path, min_buffer: u64) -> bool {
    let need = min_buffer.max(1);
    match scan_ts_segments(channel_dir) {
        Ok(s) => (s.len() as u64) >= need,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_holds_within_lag_window() {
        assert_eq!(
            resolve_cursor(Some(10), 16, 1, 4, 10),
            (10, CursorJump::None)
        );
        assert_eq!(resolve_cursor(None, 16, 1, 4, 10), (12, CursorJump::None));
    }

    #[test]
    fn cursor_jumps_forward_to_safe_edge() {
        assert_eq!(
            resolve_cursor(Some(10), 40, 1, 4, 10),
            (36, CursorJump::Forward)
        );
    }

    #[test]
    fn cursor_rewinds_after_sequence_reset() {
        assert_eq!(
            resolve_cursor(Some(501), 3, 1, 0, 10),
            (3, CursorJump::Rewind)
        );
    }

    #[test]
    fn cursor_clamps_safe_edge_to_floor_when_holdback_exceeds_depth() {
        // live=4, holdback=6 → raw safe would be 0; clamp to floor=1
        assert_eq!(resolve_cursor(None, 4, 1, 6, 10), (1, CursorJump::None));
    }

    #[test]
    fn smooth_plan_splits_long_segment_into_many_slices() {
        let (per_slice, interval) = plan_smooth_slices(2_000_000, 11.0, 250);
        assert_eq!(per_slice % TS_PACKET_SIZE, 0);
        assert!(per_slice > 0);
        let slices = 2_000_000usize.div_ceil(per_slice);
        assert!(slices >= 40);
        assert!(interval.as_secs_f64() > 0.0);
    }

    #[test]
    fn smooth_plan_single_slice_for_short_segment() {
        let (per_slice, _) = plan_smooth_slices(376, 0.2, 250);
        assert_eq!(per_slice, 376);
    }
}
